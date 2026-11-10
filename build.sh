#!/bin/sh
# Rebuilds PULP on Linux (arm64 or x86-64) from a clean checkout. The counterpart of build.bat.
#   ./build.sh           release build of everything, runs the tests, fills dist/PULP (PULP, pulp_render, pulp-mcp, factory)
#   ./build.sh quick     release build of engine, render and tests only (no JUCE), runs the tests
#   ./build.sh app       release build of the program and the MCP server only, fills dist/PULP, skips the tests
#   ./build.sh testplugins   the two VST3 plugins that scripts/m10_vst3.txt hosts (into build/release/testplugins)
#   ./build.sh library   fetches the recordings (tools/fetch-samples.py, about 420 MB, once) and rebuilds the factory instruments
# Needs cmake 3.24+ (or uvx, which then fetches it), ninja, a C++20 compiler, cargo, and the packages JUCE
# wants on Linux (README, "Linux").
set -e
cd "$(dirname "$0")"
command -v cmake >/dev/null || cmake() { uvx cmake "$@"; }

# Fetches the recordings listed in factory/sources.json (checksums verified) and builds the factory
# instruments: from those recordings where there are any, by synthesis for the rest. Without Python
# or a network the synthesized instruments are still built.
makelibrary() {
  "$1/pulp_render" --make-factory factory
  python3 -I tools/fetch-samples.py || true
  "$1/pulp_render" --build-library factory/sources.json factory-cache factory
  python3 -I tools/make_credits.py || true
}

case "${1:-}" in
  quick|library)
    dir=build/quick
    cmake -S . -B $dir -G Ninja -DCMAKE_BUILD_TYPE=Release -DPULP_BUILD_APP=OFF
    if [ "$1" = library ]; then
      cmake --build $dir --target pulp_render
      makelibrary $dir
    else
      cmake --build $dir
      shift
      $dir/pulp_tests "$@"
    fi
    ;;
  ""|app|testplugins)
    cmake -S . -B build/release -G Ninja -DCMAKE_BUILD_TYPE=Release -DPULP_BUILD_APP=ON
    if [ "$1" = testplugins ]; then
      cmake --build build/release --target testplugins
      exit 0
    fi
    if [ -z "$1" ]; then
      cmake --build build/release
      build/release/pulp_tests
      # the factory instruments are built once; "./build.sh library" rebuilds them
      [ -d factory/Instruments ] || makelibrary build/release
    fi
    cmake --build build/release --target dist
    cargo build --release --manifest-path mcp/Cargo.toml
    cp mcp/target/release/pulp-mcp dist/PULP/
    echo
    echo "Built dist/PULP/PULP and dist/PULP/pulp-mcp"
    ;;
  *)
    sed -n '2,9p' "$0"
    exit 2
    ;;
esac
