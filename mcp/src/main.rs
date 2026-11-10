//! pulp-mcp: a Model Context Protocol server on stdio that drives PULP.
//!
//! It starts `PULP --control` as a child process and passes script commands (docs/SCRIPTING.md) to its
//! stdin; what the commands log comes back on PULP's stdout. Each batch is followed by `log <mark>`, and
//! the mark coming back says the batch is done. Offline renders go through `pulp_render`.
//!
//! Both protocol eras are answered: the `initialize` handshake (2024-11-05 to 2025-11-25) and the
//! stateless one (2026-07-28: `server/discover`, the version in every request's `_meta`).

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const HELP: &str = include_str!("../../docs/SCRIPTING.md");
const MODERN: &str = "2026-07-28";
const LEGACY: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const MARK: &str = "@@pulp-mcp done ";
const INSTRUCTIONS: &str = "Drives the PULP digital audio workstation. Call pulp_help once for the command \
    language, then pulp_run with one command per line. pulp_screenshot shows what the program looks like.";

struct Daw {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    batch: u64,
}

#[derive(Default)]
struct Server {
    daw: Option<Daw>,
}

/// A program that ships beside this one, unless the environment names another.
fn program(env: &str, name: &str) -> PathBuf {
    if let Some(p) = std::env::var_os(env) {
        return p.into();
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|p| Some(p.parent()?.join(name)));
    beside.unwrap_or_else(|| name.into())
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() {
                T[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    s
}

fn text(s: String) -> Value {
    json!([{ "type": "text", "text": s }])
}

fn tools() -> Value {
    json!([
        {
            "name": "pulp_start",
            "description": "Start the PULP DAW (its window opens on the desktop, unless headless). Optional: pulp_run starts it with the defaults when it is not running.",
            "inputSchema": { "type": "object", "properties": {
                "project": { "type": "string", "description": "A .pulp or .pulpz project to open" },
                "no_audio": { "type": "boolean", "description": "Do not open a sound card: nothing is heard, everything else (including export) works. Default false" },
                "data_dir": { "type": "string", "description": "Keep settings and autosaves in this folder instead of the user's own (for tests)" },
                "size": { "type": "string", "description": "Window size, e.g. 1500x860" },
                "headless": { "type": "boolean", "description": "Run without a window (PULP is started with no display). Commands, export and screenshots work the same. Default false" }
            } }
        },
        {
            "name": "pulp_run",
            "description": "Run PULP script commands, one per line, and return what they logged. Examples: `state`, `steps`, \
    `step 0 4 1` (channel 0, step 4, on), `addchannel trio`, `addnote 4 60 0 480` (channel, key, tick, length; 240 ticks = one step), \
    `set tempo 128`, `play`, `stop`, `save song.pulp`, `export out.wav` then `waitexport`, `wait 500`, `quit` (closes PULP without asking). \
    pulp_help lists every command. Relative paths are relative to this server's working directory. A line starting with `error:` in the answer means that command failed; the others still ran.",
            "inputSchema": { "type": "object", "required": ["commands"], "properties": {
                "commands": { "type": "string", "description": "Script commands, one per line" },
                "timeout_s": { "type": "number", "description": "How long to wait for the commands to finish. Default 120" }
            } }
        },
        {
            "name": "pulp_screenshot",
            "description": "A picture of PULP or one of its windows, as a PNG.",
            "inputSchema": { "type": "object", "properties": {
                "target": { "type": "string", "description": "main (default), toolbar, browser, rack, piano, playlist, mixer, chan, settings, export, events, link, fx" },
                "scale": { "type": "number", "description": "Size factor, default 1" }
            } }
        },
        {
            "name": "pulp_render",
            "description": "Render a saved .pulp project to a WAV file with no window and no sound card (the pulp_render program). PULP does not need to be running.",
            "inputSchema": { "type": "object", "required": ["project", "output"], "properties": {
                "project": { "type": "string" },
                "output": { "type": "string", "description": "The WAV file to write" },
                "options": { "type": "array", "items": { "type": "string" }, "description": "Extra arguments: --rate <hz>, --bits <16|24|32>, --dither, --song, --pattern, --tail <seconds>, --factory <dir>" }
            } }
        },
        {
            "name": "pulp_help",
            "description": "The reference of every script command pulp_run accepts.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

impl Server {
    fn running(&mut self) -> bool {
        if self
            .daw
            .as_mut()
            .is_some_and(|d| !matches!(d.child.try_wait(), Ok(None)))
        {
            self.daw = None;
        }
        self.daw.is_some()
    }

    fn start(&mut self, a: &Value) -> Result<String, String> {
        if self.running() {
            return Err(
                "PULP is already running (send `quit` through pulp_run to close it)".into(),
            );
        }
        let path = program("PULP_BIN", "PULP");
        let mut cmd = Command::new(&path);
        cmd.arg("--control");
        if a["no_audio"].as_bool().unwrap_or(false) {
            cmd.arg("--no-audio");
        }
        if a["headless"].as_bool().unwrap_or(false) {
            cmd.env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY");
        }
        for (key, flag) in [
            ("project", "--project"),
            ("data_dir", "--data-dir"),
            ("size", "--size"),
        ] {
            if let Some(v) = a[key].as_str() {
                cmd.args([flag, v]);
            }
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| {
                format!(
                    "cannot start {}: {e} (set PULP_BIN to the PULP program)",
                    path.display()
                )
            })?;
        let stdin = child.stdin.take().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in out.lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        self.daw = Some(Daw {
            child,
            stdin,
            lines,
            batch: 0,
        });
        let said = self.run("state", Duration::from_secs(60))?;
        if self.daw.is_none() {
            return Err(format!(
                "PULP started and closed at once; run `{} --control` by hand to see why",
                path.display()
            ));
        }
        Ok(format!("PULP is running\n{said}"))
    }

    fn run(&mut self, commands: &str, timeout: Duration) -> Result<String, String> {
        if !self.running() {
            self.start(&json!({}))?;
        }
        let daw = self.daw.as_mut().unwrap();
        daw.batch += 1;
        let mark = format!("{MARK}{}", daw.batch);
        writeln!(daw.stdin, "{}\nlog {mark}", commands.trim_end())
            .and_then(|_| daw.stdin.flush())
            .map_err(|e| format!("PULP is not listening: {e}"))?;
        let deadline = Instant::now() + timeout;
        let mut out: Vec<String> = Vec::new();
        loop {
            match daw
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(l) if l == mark => break,
                Ok(l) if l.starts_with(MARK) => {} // the end of an earlier batch that timed out
                Ok(l) => out.push(l),
                Err(RecvTimeoutError::Timeout) => {
                    out.push(format!("error: not finished after {} s; PULP is still at it, and what it logs will come with the next call", timeout.as_secs()));
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    if let Some(mut d) = self.daw.take() {
                        let _ = d.child.wait();
                    }
                    out.push("PULP has closed".into());
                    break;
                }
            }
        }
        let failed = out.iter().any(|l| l.starts_with("error:"));
        let all = out.join("\n");
        if failed {
            Err(all)
        } else {
            Ok(all)
        }
    }

    fn screenshot(&mut self, a: &Value) -> Result<Value, String> {
        let target = a["target"].as_str().unwrap_or("main");
        if !target.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err("target is one word, e.g. main or mixer".into());
        }
        let file = std::env::temp_dir().join(format!("pulp-mcp-{}.png", std::process::id()));
        let log = self.run(
            &format!(
                "shot {target} \"{}\" {}",
                file.display(),
                a["scale"].as_f64().unwrap_or(1.0)
            ),
            Duration::from_secs(60),
        )?;
        let png =
            std::fs::read(&file).map_err(|e| format!("no picture was written: {e}\n{log}"))?;
        let _ = std::fs::remove_file(&file);
        Ok(json!([{ "type": "image", "data": base64(&png), "mimeType": "image/png" }]))
    }

    fn render(a: &Value) -> Result<String, String> {
        let (Some(project), Some(output)) = (a["project"].as_str(), a["output"].as_str()) else {
            return Err("project and output are required".into());
        };
        let path = program("PULP_RENDER_BIN", "pulp_render");
        let options = a["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str);
        let o = Command::new(&path)
            .args([project, output])
            .args(options)
            .output()
            .map_err(|e| {
                format!(
                    "cannot start {}: {e} (set PULP_RENDER_BIN to the pulp_render program)",
                    path.display()
                )
            })?;
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        if o.status.success() {
            Ok(said)
        } else {
            Err(said)
        }
    }

    fn call(&mut self, name: &str, a: &Value) -> Option<Value> {
        // ponytail: one request at a time, so nothing else is answered while a long pulp_run waits.
        // Read requests on a thread of their own if a client ever needs ping during a long export.
        let result = match name {
            "pulp_start" => self.start(a).map(text),
            "pulp_run" => match a["commands"].as_str() {
                Some(c) => self
                    .run(
                        c,
                        Duration::from_secs_f64(
                            a["timeout_s"].as_f64().unwrap_or(120.0).clamp(1.0, 86400.0),
                        ),
                    )
                    .map(text),
                None => Err("commands is required".into()),
            },
            "pulp_screenshot" => self.screenshot(a),
            "pulp_render" => Self::render(a).map(text),
            "pulp_help" => Ok(text(HELP.into())),
            _ => return None,
        };
        Some(match result {
            Ok(content) => json!({ "content": content, "isError": false }),
            Err(why) => json!({ "content": text(why), "isError": true }),
        })
    }

    /// One JSON-RPC message in, at most one out (notifications and responses get no answer).
    fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id")?.clone();
        let method = msg["method"].as_str()?;
        let params = &msg["params"];
        let error = |code: i32, message: &str, data: Value| {
            Some(
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message, "data": data } }),
            )
        };
        let supported = || std::iter::once(MODERN).chain(LEGACY).collect::<Vec<_>>();
        if let Some(v) = params["_meta"][VERSION_KEY].as_str() {
            if v != MODERN {
                return error(
                    -32022,
                    "Unsupported protocol version",
                    json!({ "supported": supported(), "requested": v }),
                );
            }
        }
        let mut result = match method {
            "initialize" => {
                let asked = params["protocolVersion"].as_str().unwrap_or("");
                json!({
                    "protocolVersion": if LEGACY.contains(&asked) { asked } else { LEGACY[0] },
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "pulp-mcp", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS
                })
            }
            "server/discover" => {
                json!({ "supportedVersions": [MODERN], "capabilities": { "tools": {} }, "instructions": INSTRUCTIONS, "ttlMs": 3600000, "cacheScope": "public" })
            }
            "ping" => json!({}),
            "tools/list" => json!({ "tools": tools(), "ttlMs": 3600000, "cacheScope": "public" }),
            "tools/call" => match self
                .call(params["name"].as_str().unwrap_or(""), &params["arguments"])
            {
                Some(r) => r,
                None => return error(-32602, "Unknown tool", json!({ "name": params["name"] })),
            },
            _ => return error(-32601, "Method not found", json!({ "method": method })),
        };
        // what the stateless era wants on every result; the handshake era ignores it
        result["resultType"] = json!("complete");
        result["_meta"] = json!({ "io.modelcontextprotocol/serverInfo": { "name": "pulp-mcp", "version": env!("CARGO_PKG_VERSION") } });
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }
}

fn main() {
    let mut server = Server::default();
    let mut stdout = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => server.handle(&msg),
            Err(_) if line.trim().is_empty() => None,
            Err(e) => Some(
                json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("Parse error: {e}") } }),
            ),
        };
        if let Some(reply) = reply {
            if writeln!(stdout, "{reply}")
                .and_then(|_| stdout.flush())
                .is_err()
            {
                break;
            }
        }
    }
    // The client has gone. Dropping the server closes PULP's stdin, and PULP then closes by itself:
    // at once if nothing is unsaved, otherwise after asking the person in front of it.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(&[0xfb, 0xff, 0xfe, 0x00]), "+//+AA==");
    }

    #[test]
    fn both_protocol_eras() {
        let mut s = Server::default();
        // handshake era: the client's version is taken if known, the newest known one otherwise
        let r = s.handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}})).unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r["id"], 1);
        let r = s.handle(&json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}})).unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
        assert!(s
            .handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .is_none());
        // stateless era
        let meta = |v: &str| json!({"_meta": {VERSION_KEY: v}});
        let r = s
            .handle(
                &json!({"jsonrpc":"2.0","id":"d","method":"server/discover","params":meta(MODERN)}),
            )
            .unwrap();
        assert_eq!(r["result"]["supportedVersions"], json!([MODERN]));
        assert_eq!(r["result"]["resultType"], "complete");
        let r = s
            .handle(
                &json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":meta("1900-01-01")}),
            )
            .unwrap();
        assert_eq!(r["error"]["code"], -32022);
        assert_eq!(r["error"]["data"]["requested"], "1900-01-01");
        let r = s
            .handle(&json!({"jsonrpc":"2.0","id":4,"method":"tools/list","params":meta(MODERN)}))
            .unwrap();
        let names: Vec<_> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "pulp_start",
                "pulp_run",
                "pulp_screenshot",
                "pulp_render",
                "pulp_help"
            ]
        );
        // tools that need no PULP
        let r = s.handle(&json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"pulp_help","arguments":{}}})).unwrap();
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("addnote"));
        assert_eq!(r["result"]["isError"], false);
        let r = s.handle(&json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"nope","arguments":{}}})).unwrap();
        assert_eq!(r["error"]["code"], -32602);
        let r = s
            .handle(&json!({"jsonrpc":"2.0","id":7,"method":"resources/list"}))
            .unwrap();
        assert_eq!(r["error"]["code"], -32601);
        let r = s.handle(&json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"pulp_render","arguments":{}}})).unwrap();
        assert_eq!(r["result"]["isError"], true);
    }
}
