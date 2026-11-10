#!/usr/bin/env python3
"""End-to-end check of pulp-mcp against the real program: python3 mcp/smoke.py [dist/PULP]

Speaks MCP to dist/PULP/pulp-mcp on stdio (the handshake era, then one stateless-era request), has it
start PULP without a sound card, programs a beat and a melody, exports and renders it, and asserts that
the audio is not silent. PULP runs without a window. Exits non-zero on the first failure.
"""
import array, base64, json, os, subprocess, sys, tempfile, wave

dist = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "..", "dist", "PULP"))
work = tempfile.mkdtemp(prefix="pulp-mcp-smoke-")
server = subprocess.Popen([os.path.join(dist, "pulp-mcp")], stdin=subprocess.PIPE, stdout=subprocess.PIPE, cwd=work, text=True)
next_id = 0


def request(method, params=None):
    global next_id
    next_id += 1
    server.stdin.write(json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}) + "\n")
    server.stdin.flush()
    reply = json.loads(server.stdout.readline())
    assert reply["id"] == next_id, reply
    return reply


def call(name, **arguments):
    result = request("tools/call", {"name": name, "arguments": arguments})["result"]
    first = result["content"][0]
    print(f"--- {name} {arguments}\n{first.get('text', '<' + first['type'] + '>')}")
    return result


def peak(path):
    with wave.open(path) as w:
        assert w.getsampwidth() == 2, "ask for 16 bits: the wave module reads those"
        samples = array.array("h", w.readframes(w.getnframes()))
        return w.getnframes(), max(abs(s) for s in samples) / 32768.0


init = request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "smoke", "version": "0"}})["result"]
assert init["protocolVersion"] == "2025-06-18" and "tools" in init["capabilities"], init
server.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
assert len(request("tools/list")["result"]["tools"]) == 5
modern = request("tools/list", {"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}})["result"]
assert modern["resultType"] == "complete", modern

started = call("pulp_start", no_audio=True, headless=True, data_dir="data")
assert not started["isError"] and "channels=4" in started["content"][0]["text"], started

beat = "\n".join(
    [f"step 0 {s} 1" for s in (0, 4, 8, 12)] + [f"step 2 {s} 1" for s in range(0, 16, 2)] + ["step 3 4 1", "step 3 12 1"]
    + ["addchannel trio", "addnote 4 60 0 480", "addnote 4 64 480 480", "addnote 4 67 960 960 0.6", "set tempo 128", "steps", "set channel 4", "piano", "state"])
out = call("pulp_run", commands=beat)
text = out["content"][0]["text"]
assert not out["isError"], out
assert "steps x...x...x...x...  Kick" in text and "steps x.x.x.x.x.x.x.x.  Hat" in text and "notes=3" in text and "tempo=128.00" in text and "channels=5" in text

bad = call("pulp_run", commands="nonsense\nstate")
assert bad["isError"] and "error: unknown command" in bad["content"][0]["text"] and "state playing=" in bad["content"][0]["text"]

out = call("pulp_run", commands="export live.wav pattern 44100 16\nwaitexport\nsave song.pulp")
assert not out["isError"] and "exported live.wav" in out["content"][0]["text"], out
frames, level = peak(os.path.join(work, "live.wav"))
print(f"live.wav: {frames} frames, peak {level:.3f}")
assert frames > 44100 and level > 0.05

shot = call("pulp_screenshot", target="rack")
png = base64.b64decode(shot["content"][0]["data"])
assert shot["content"][0]["mimeType"] == "image/png" and png[:8] == b"\x89PNG\r\n\x1a\n" and len(png) > 2000

closed = call("pulp_run", commands="quit")
assert "PULP has closed" in closed["content"][0]["text"]

out = call("pulp_render", project="song.pulp", output="offline.wav", options=["--rate", "44100", "--bits", "16", "--pattern"])
assert not out["isError"], out
frames, level = peak(os.path.join(work, "offline.wav"))
print(f"offline.wav: {frames} frames, peak {level:.3f}")
assert frames > 44100 and level > 0.05

server.stdin.close()
assert server.wait(timeout=10) == 0
print(f"\nall good ({work})")
