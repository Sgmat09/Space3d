#!/usr/bin/env bash
# Build Space3d examples for the web, serve them and run each in headless Google Chrome on
# WebGPU and on WebGL2 (scripts/web-smoke.mjs drives Chrome over the DevTools protocol).
#
#   scripts/web-smoke.sh                          # web_demo + graphics_showcase, every backend
#   scripts/web-smoke.sh web_demo                 # one example
#   BACKENDS=webgl2 scripts/web-smoke.sh          # some backends (below)
#   PROFILE=debug scripts/web-smoke.sh            # dev-profile wasm: no LTO/wasm-opt, bigger
#   NO_BUILD=1 scripts/web-smoke.sh               # reuse the existing dist/web
#
# Backends: `webgpu` (Chrome's WebGPU), `webgl2` (Chrome without a WebGPU adapter: wgpu's
# automatic WebGL2 fallback, what browsers without WebGPU get), `webgl2-url` (WebGPU available,
# `?webgl2` in the URL: the engine's override).
#
# Environment: CHROME (browser binary), CHROME_PROFILE (throwaway --user-data-dir, removed
# afterwards unless KEEP_PROFILE=1; default: a new directory in $TMPDIR per run, so concurrent
# runs never share one), OUT (default target/web-smoke).
# Output per example and backend: $OUT/<example>-<backend>-{1,2,3}.png (two shots 0.5 s apart,
# one after the input), .console.log and .json (the checks' summary). Exit status 0 when every
# run passed. Needs: rustup target wasm32-unknown-unknown, the wasm-bindgen CLI matching
# Cargo.lock (`space3d doctor` prints the install command), Node 22+, Google Chrome.
# Headless only: no window is shown and the focus never moves.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
EXAMPLES=("$@")
[ ${#EXAMPLES[@]} -gt 0 ] || EXAMPLES=(web_demo graphics_showcase)
read -r -a BACKEND_LIST <<< "${BACKENDS:-webgpu webgl2 webgl2-url}"
PROFILE=${PROFILE:-release}
OUT=${OUT:-$ROOT/target/web-smoke}
CHROME_PROFILE=${CHROME_PROFILE:-$(mktemp -d "${TMPDIR:-/tmp}/space3d-web-smoke-chrome.XXXXXX")}
mkdir -p "$OUT"

node_major=$(node -p 'process.versions.node.split(".")[0]' 2>/dev/null || echo 0)
[ "$node_major" -ge 22 ] || { echo "Node 22+ is needed (global fetch/WebSocket); found: $(node --version 2>&1)" >&2; exit 2; }
if [ -z "${CHROME:-}" ]; then
  case "$(uname)" in
    Darwin) CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" ;;
    *) CHROME=$(command -v google-chrome || command -v google-chrome-stable || command -v chromium || true) ;;
  esac
fi
[ -x "$CHROME" ] || { echo "Google Chrome not found; set CHROME=/path/to/chrome" >&2; exit 2; }
export CHROME

# Per-example input and expectations (see scripts/web-smoke.mjs for the flags): sets `args`.
example_args() {
  case "$1" in
    web_demo)
      # A drag orbits (and does not change the palette: no `palette 3`), a click cycles the
      # palette (colours change), `3` switches the wave pattern; the crystal
      # (assets/crystal.obj + .mtl + .png) comes over HTTP.
      args=(--drag --click --key Digit3 --expect-log 'palette 2' --reject-log 'palette 3'
        --expect-log 'pattern 3' --expect-animation --expect-color-shift --ready-log 'crystal loaded'
        --expect-request 'assets/crystal\.obj' --expect-request 'assets/crystal\.mtl'
        --expect-request 'assets/crystal\.png')
      ;;
    graphics_showcase)
      # The content is generated in the page (inline on the web) before the first shot; `1`
      # turns bloom off. Its audio starts with that key press, the first gesture.
      args=(--ready-log 'content ready' --settle 4 --key Digit1 --expect-log 'bloom: off' --expect-audio
        --expect-animation)
      ;;
    *) args=(--click) ;;
  esac
}

echo "== building the space3d CLI"
cargo build -q -p space3d-cli
TARGET_DIR=$(cargo metadata -q --format-version 1 --no-deps | node -p 'JSON.parse(require("fs").readFileSync(0)).target_directory')
SPACE3D="$TARGET_DIR/debug/space3d"
release_flag=()
[ "$PROFILE" = release ] && release_flag=(--release)

SERVER_PID=
cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  [ "${KEEP_PROFILE:-0}" = 1 ] || rm -rf "$CHROME_PROFILE"
}
trap cleanup EXIT

results=()
status=0
for ex in "${EXAMPLES[@]}"; do
  dir="examples/$ex"
  [ -d "$dir" ] || { echo "no example $dir" >&2; exit 2; }
  if [ "${NO_BUILD:-0}" != 1 ]; then
    echo "== $ex: space3d build --target web ${release_flag[*]:-}"
    "$SPACE3D" -C "$dir" build --target web ${release_flag[@]+"${release_flag[@]}"}
  fi

  echo "== $ex: space3d serve --no-build"
  log="$OUT/$ex.serve.log"
  "$SPACE3D" -C "$dir" serve --no-build --port 0 > "$log" 2>&1 &
  SERVER_PID=$!
  url=
  for _ in $(seq 1 100); do
    url=$(sed -n 's/^serving .* at \(http:[^ ]*\)\/ .*/\1/p' "$log")
    [ -n "$url" ] && break
    kill -0 "$SERVER_PID" 2>/dev/null || { cat "$log" >&2; exit 1; }
    sleep 0.1
  done
  [ -n "$url" ] || { echo "server did not start" >&2; cat "$log" >&2; exit 1; }
  # Headers the page depends on: wasm MIME type (streaming compilation) and cross-origin
  # isolation.
  headers=$(curl -sfI "$url/${ex}_bg.wasm")
  grep -qi '^content-type: application/wasm' <<< "$headers" || { echo "$ex: wrong wasm MIME type:" >&2; echo "$headers" >&2; status=1; }
  grep -qi '^cross-origin-embedder-policy: require-corp' <<< "$headers" || { echo "$ex: no COEP header" >&2; status=1; }

  example_args "$ex"
  for backend in "${BACKEND_LIST[@]}"; do
    query=
    variant=()
    case "$backend" in
      webgpu) expect=webgpu ;;
      webgl2) expect=webgl2; variant=(--no-webgpu) ;;
      webgl2-url) expect=webgl2; query='?webgl2' ;;
      *) echo "unknown backend $backend (webgpu, webgl2, webgl2-url)" >&2; exit 2 ;;
    esac
    echo "== $ex on $backend: $url/$query ${variant[*]:-}"
    if node scripts/web-smoke.mjs --url "$url/$query" --backend "$expect" --name "$ex-$backend" \
      --out "$OUT" --profile "$CHROME_PROFILE" ${variant[@]+"${variant[@]}"} ${args[@]+"${args[@]}"} > "$OUT/$ex-$backend.stdout.json"; then
      results+=("PASS $ex $backend")
    else
      results+=("FAIL $ex $backend  (see $OUT/$ex-$backend.json)")
      status=1
    fi
  done
  kill "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
  SERVER_PID=
done

echo
printf '%s\n' "${results[@]}"
echo "screenshots and logs: $OUT"
exit $status
