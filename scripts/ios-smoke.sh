#!/usr/bin/env bash
# iOS simulator smoke test: build examples with `space3d build --target ios --sim`, run them on
# a dedicated, headless simulator and check that they really render.
#
#   scripts/ios-smoke.sh                              # hello_cube + graphics_showcase
#   scripts/ios-smoke.sh -e hello_cube --release      # one example, release profile
#   scripts/ios-smoke.sh --keep-device --out /tmp/shots
#
# Per example:
#   1. build + bundle (`cargo run -p space3d-cli -- build --target ios --sim -p <example>`);
#   2. install on the simulator device "Space3d Test" (created with `xcrun simctl create` when
#      missing, booted headless with `simctl boot` — Simulator.app is never opened, no window
#      takes focus), launch with the console attached and the unified log streamed;
#   3. wait for the renderer and for frames, screenshot (`simctl io screenshot`) and check the
#      picture is a rendered scene (not blank / one color);
#   4. background (launch Settings) and foreground again: the app must survive, drop and
#      recreate its surface (`suspended` / `resumed` in the log) and render again;
#   5. relaunch with SPACE3D_EXIT_AFTER_FRAMES and expect the engine's auto-exit line (fps);
#   6. fail on panics, `ERROR` log lines, wgpu validation errors, winit's per-frame iOS
#      event-order lines (the console run sets a RUST_LOG without them) and new crash reports.
#
# Rotation is not covered: simctl cannot rotate a device (only Simulator.app can).
#
# Options:
#   -e, --example NAME   example package to run (repeatable; default: hello_cube graphics_showcase)
#   --release            build with the release profile
#   --out DIR            screenshots + logs (default: target/ios-smoke)
#   --device NAME        simulator device name (default: "Space3d Test")
#   --device-type ID     device type for a new device (default: iPhone 17, else the first iPhone)
#   --runtime ID         simulator runtime for a new device (default: the newest iOS runtime)
#   --frames N           frames for the auto-exit run (default: 300)
#   --keep-device        leave the device booted (default: shut down; deleted if this run created it)
#   --no-build           use the bundles already in <out>/build
#
# Needs macOS with Xcode (iOS SDK + an iOS simulator runtime), `rustup target add
# aarch64-apple-ios-sim`, python3 (ships with Xcode). `space3d doctor --platforms ios` checks
# the setup.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXAMPLES=()
RELEASE=""
OUT="$ROOT/target/ios-smoke"
DEVICE_NAME="Space3d Test"
DEVICE_TYPE=""
RUNTIME=""
FRAMES=300
KEEP_DEVICE=0
BUILD=1

while [ $# -gt 0 ]; do
  case "$1" in
    -e|--example) EXAMPLES+=("$2"); shift 2 ;;
    --release) RELEASE="--release"; shift ;;
    --out) OUT="$2"; shift 2 ;;
    --device) DEVICE_NAME="$2"; shift 2 ;;
    --device-type) DEVICE_TYPE="$2"; shift 2 ;;
    --runtime) RUNTIME="$2"; shift 2 ;;
    --frames) FRAMES="$2"; shift 2 ;;
    --keep-device) KEEP_DEVICE=1; shift ;;
    --no-build) BUILD=0; shift ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed -e '$d' -e 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
  esac
done
[ ${#EXAMPLES[@]} -eq 0 ] && EXAMPLES=(hello_cube graphics_showcase)

log() { printf '\033[1m[ios-smoke]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[31m[ios-smoke] FAIL:\033[0m %s\n' "$*" >&2; exit 1; }
# Run a command with a deadline (no `timeout` on macOS): `deadline SECS cmd...`.
deadline() { local secs="$1"; shift; perl -e 'alarm shift @ARGV; exec @ARGV or exit 127' "$secs" "$@"; }
simctl() { deadline 120 xcrun simctl "$@"; }

[ "$(uname -s)" = Darwin ] || die "iOS needs macOS with Xcode"
xcrun --sdk iphonesimulator --show-sdk-version >/dev/null 2>&1 \
  || die "no iOS simulator SDK: install Xcode, then: xcodebuild -downloadPlatform iOS"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

# ---------------------------------------------------------------------------------- device
UDID=""
CREATED=0
STREAM_PID=""
CONSOLE_PID=""
cleanup() {
  local status=$?
  [ -n "$STREAM_PID" ] && kill "$STREAM_PID" 2>/dev/null || true
  [ -n "$CONSOLE_PID" ] && kill "$CONSOLE_PID" 2>/dev/null || true
  if [ -n "$UDID" ] && [ "$KEEP_DEVICE" = 0 ]; then
    log "shutting down $DEVICE_NAME ($UDID)"
    simctl shutdown "$UDID" >/dev/null 2>&1 || true
    if [ "$CREATED" = 1 ]; then
      log "deleting $DEVICE_NAME ($UDID)"
      simctl delete "$UDID" >/dev/null 2>&1 || true
    fi
  fi
  exit $status
}
trap cleanup EXIT

# `name|udid|state` of the device called $DEVICE_NAME, if any.
find_device() {
  simctl list devices --json | DEVICE_NAME="$DEVICE_NAME" python3 -c '
import json, os, sys
for runtime, devices in json.load(sys.stdin)["devices"].items():
    for d in devices:
        if d["name"] == os.environ["DEVICE_NAME"] and d.get("isAvailable", True):
            print(d["udid"] + "|" + d["state"]); sys.exit(0)'
}

DEVICE="$(find_device || true)"
if [ -z "$DEVICE" ]; then
  if [ -z "$RUNTIME" ] || [ -z "$DEVICE_TYPE" ]; then
    read -r AUTO_RUNTIME AUTO_TYPE < <(simctl list runtimes --json | python3 -c '
import json, sys
runtimes = [r for r in json.load(sys.stdin)["runtimes"]
            if r.get("isAvailable") and (r.get("platform") == "iOS" or r["name"].startswith("iOS "))]
if not runtimes:
    sys.exit("no available iOS simulator runtime (xcodebuild -downloadPlatform iOS)")
r = max(runtimes, key=lambda r: [int(p) for p in r["version"].split(".")])
types = [t["identifier"] for t in r.get("supportedDeviceTypes", []) if t.get("productFamily") == "iPhone"]
preferred = "com.apple.CoreSimulator.SimDeviceType.iPhone-17"
print(r["identifier"], preferred if preferred in types else (types[0] if types else preferred))') || true
    RUNTIME="${RUNTIME:-${AUTO_RUNTIME:-}}"
    DEVICE_TYPE="${DEVICE_TYPE:-${AUTO_TYPE:-}}"
  fi
  [ -n "$RUNTIME" ] && [ -n "$DEVICE_TYPE" ] \
    || die "no available iOS simulator runtime: xcodebuild -downloadPlatform iOS (or pass --runtime/--device-type)"
  log "creating simulator device \"$DEVICE_NAME\" ($DEVICE_TYPE, $RUNTIME)"
  UDID="$(simctl create "$DEVICE_NAME" "$DEVICE_TYPE" "$RUNTIME")"
  CREATED=1
  STATE=Shutdown
else
  UDID="${DEVICE%%|*}"
  STATE="${DEVICE##*|}"
fi
if [ "$STATE" != Booted ]; then
  log "booting $DEVICE_NAME ($UDID) headless"
  simctl boot "$UDID"
fi
deadline 300 xcrun simctl bootstatus "$UDID" -b >/dev/null || die "$DEVICE_NAME did not finish booting"
log "device ready: $DEVICE_NAME ($UDID)"

# ---------------------------------------------------------------------------------- helpers
# Is the app with bundle id $1 running? Prints its pid.
app_pid() {
  simctl spawn "$UDID" launchctl list 2>/dev/null \
    | awk -v id="UIKitApplication:$1[" 'index($3, id) == 1 && $1 ~ /^[0-9]+$/ { print $1; exit }'
}

# Wait up to $2 seconds for the regex $3 in file $1.
wait_for_line() {
  local file="$1" secs="$2" pattern="$3" i=0
  while [ $i -lt $((secs * 2)) ]; do
    grep -Eq "$pattern" "$file" 2>/dev/null && return 0
    perl -e 'select(undef, undef, undef, 0.5)'
    i=$((i + 1))
  done
  return 1
}

# Check that a screenshot shows a rendered scene: not (nearly) one color, many colors.
check_picture() {
  python3 - "$1" <<'PY'
import struct, sys, zlib
path = sys.argv[1]
data = open(path, "rb").read()
assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
pos, chunks, idat = 8, {}, b""
while pos < len(data):
    n, kind = struct.unpack(">I4s", data[pos:pos + 8])
    body = data[pos + 8:pos + 8 + n]
    pos += 12 + n
    if kind == b"IDAT":
        idat += body
    else:
        chunks[kind] = body
w, h, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", chunks[b"IHDR"])
assert depth == 8 and interlace == 0 and ctype in (2, 6), f"unsupported PNG ({depth} bit, type {ctype})"
bpp = 3 if ctype == 2 else 4
raw, stride = zlib.decompress(idat), w * bpp
rows, prev, off = [], bytearray(stride), 0
for _ in range(h):
    f, line = raw[off], bytearray(raw[off + 1:off + 1 + stride])
    off += 1 + stride
    for i in range(stride):
        a = line[i - bpp] if i >= bpp else 0
        b = prev[i]
        c = prev[i - bpp] if i >= bpp else 0
        if f == 1: line[i] = (line[i] + a) & 255
        elif f == 2: line[i] = (line[i] + b) & 255
        elif f == 3: line[i] = (line[i] + (a + b) // 2) & 255
        elif f == 4:
            p = a + b - c
            pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
            line[i] = (line[i] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
    rows.append(line)
    prev = line
counts = {}
for y in range(0, h, 4):
    row = rows[y]
    for x in range(0, w, 4):
        i = x * bpp
        key = (row[i] >> 3, row[i + 1] >> 3, row[i + 2] >> 3)
        counts[key] = counts.get(key, 0) + 1
total = sum(counts.values())
dominant = max(counts.values()) / total
print(f"{w}x{h}, {len(counts)} colors, dominant color {dominant:.0%}")
if len(counts) < 24 or dominant > 0.95:
    sys.exit(f"{path} does not look like a rendered scene")
PY
}

# Lines that fail the run, from a log file.
bad_lines() {
  grep -E "panicked|\bERROR\b|Validation Error|wgpu error|\[space3d:panic\]|EXC_|Fatal error" "$1" || true
}

# ---------------------------------------------------------------------------------- examples
SUMMARY=()
START_MARK="$OUT/.started"
touch "$START_MARK"
for EXAMPLE in "${EXAMPLES[@]}"; do
  log "=== $EXAMPLE"
  BUILD_DIR="$OUT/build"
  if [ "$BUILD" = 1 ]; then
    log "building (space3d build --target ios --sim -p $EXAMPLE $RELEASE)"
    (cd "$ROOT" && cargo run -q -p space3d-cli -- build --target ios --sim -p "$EXAMPLE" $RELEASE --out "$BUILD_DIR") \
      || die "$EXAMPLE: build failed"
  fi
  APP="$BUILD_DIR/aarch64-apple-ios-sim/$EXAMPLE.app"
  [ -d "$APP" ] || die "$EXAMPLE: no bundle at $APP"
  BUNDLE_ID="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$APP/Info.plist")"
  CONSOLE="$OUT/$EXAMPLE.console.log"
  OSLOG="$OUT/$EXAMPLE.oslog.txt"
  : > "$CONSOLE"

  simctl terminate "$UDID" "$BUNDLE_ID" >/dev/null 2>&1 || true
  simctl install "$UDID" "$APP" || die "$EXAMPLE: install failed"

  # Unified log of the app process (engine lines are subsystem "space3d").
  deadline 900 xcrun simctl spawn "$UDID" log stream --style compact --level info \
    --predicate "process == \"$EXAMPLE\"" > "$OSLOG" 2>&1 &
  STREAM_PID=$!

  # Run with the console attached (stderr: the engine's log lines), in the background. The
  # RUST_LOG leaves winit out: the engine keeps its per-frame iOS event-order lines quiet.
  SIMCTL_CHILD_RUST_LOG="info,space3d_platform=debug,wgpu_core=warn,wgpu_hal=warn,naga=warn,egui_wgpu=error" \
    deadline 600 xcrun simctl launch --console-pty --terminate-running-process "$UDID" "$BUNDLE_ID" \
    > "$CONSOLE" 2>&1 &
  CONSOLE_PID=$!

  wait_for_line "$CONSOLE" 90 "renderer initialised" || { tail -20 "$CONSOLE" >&2; die "$EXAMPLE: no renderer after 90 s"; }
  ADAPTER="$(grep -Eo "adapter=.* draw_path=[A-Za-z]+" "$CONSOLE" | head -1)"
  log "renderer: $ADAPTER"
  sleep 6   # frames (the showcase builds its content off the frame first)
  PID="$(app_pid "$BUNDLE_ID")"
  [ -n "$PID" ] || { tail -30 "$CONSOLE" >&2; die "$EXAMPLE: not running after start-up"; }
  simctl io "$UDID" screenshot "$OUT/$EXAMPLE.png" >/dev/null 2>&1
  PICTURE="$(check_picture "$OUT/$EXAMPLE.png")" || die "$EXAMPLE: $PICTURE"
  log "screenshot $OUT/$EXAMPLE.png: $PICTURE"

  # Background and foreground again.
  simctl launch "$UDID" com.apple.Preferences >/dev/null
  wait_for_line "$CONSOLE" 30 "suspended" || die "$EXAMPLE: no suspend when backgrounded"
  sleep 2
  simctl io "$UDID" screenshot "$OUT/$EXAMPLE-background.png" >/dev/null 2>&1
  simctl launch "$UDID" "$BUNDLE_ID" >/dev/null
  RESUMES_BEFORE=1
  i=0
  while [ "$(grep -c "DEBUG .*resumed" "$CONSOLE" || true)" -le "$RESUMES_BEFORE" ] && [ $i -lt 60 ]; do
    perl -e 'select(undef, undef, undef, 0.5)'; i=$((i + 1))
  done
  [ $i -lt 60 ] || die "$EXAMPLE: not resumed after returning to the foreground"
  sleep 3
  [ "$(app_pid "$BUNDLE_ID")" = "$PID" ] || die "$EXAMPLE: the app did not survive background/foreground"
  simctl io "$UDID" screenshot "$OUT/$EXAMPLE-resumed.png" >/dev/null 2>&1
  PICTURE="$(check_picture "$OUT/$EXAMPLE-resumed.png")" || die "$EXAMPLE after resume: $PICTURE"
  log "background/foreground ok; screenshot $OUT/$EXAMPLE-resumed.png: $PICTURE"
  simctl terminate "$UDID" "$BUNDLE_ID" >/dev/null 2>&1 || true
  wait "$CONSOLE_PID" 2>/dev/null || true
  CONSOLE_PID=""

  # Frame loop + clean exit: the engine quits after $FRAMES frames and prints its fps.
  EXIT_LOG="$OUT/$EXAMPLE.exit.log"
  SIMCTL_CHILD_SPACE3D_EXIT_AFTER_FRAMES="$FRAMES" \
    deadline 300 xcrun simctl launch --console-pty --terminate-running-process "$UDID" "$BUNDLE_ID" \
    > "$EXIT_LOG" 2>&1 || true
  EXIT_LINE="$(grep -E "space3d: auto-exit after" "$EXIT_LOG" || true)"
  [ -n "$EXIT_LINE" ] || { tail -20 "$EXIT_LOG" >&2; die "$EXAMPLE: no auto-exit after $FRAMES frames"; }
  [ -z "$(app_pid "$BUNDLE_ID")" ] || die "$EXAMPLE: still running after AppExit"
  log "${EXIT_LINE#space3d: }"

  kill "$STREAM_PID" 2>/dev/null || true
  wait "$STREAM_PID" 2>/dev/null || true
  STREAM_PID=""
  BAD="$(bad_lines "$CONSOLE"; bad_lines "$EXIT_LOG"; bad_lines "$OSLOG")"
  [ -z "$BAD" ] || { echo "$BAD" | head -20 >&2; die "$EXAMPLE: errors in the logs ($OUT)"; }
  ! grep -q "platform_impl::ios::app_state" "$CONSOLE" \
    || die "$EXAMPLE: winit's per-frame event-order lines reached the log with RUST_LOG set ($CONSOLE)"
  grep -q "\[space3d:log\]" "$OSLOG" || die "$EXAMPLE: no engine lines in the unified log ($OSLOG)"
  CRASHES="$(find "$HOME/Library/Logs/DiagnosticReports" -name "$EXAMPLE-*.ips" -newer "$START_MARK" 2>/dev/null || true)"
  [ -z "$CRASHES" ] || die "$EXAMPLE: crash report(s): $CRASHES"

  simctl uninstall "$UDID" "$BUNDLE_ID" >/dev/null 2>&1 || true
  SUMMARY+=("$EXAMPLE: ok — $PICTURE; ${EXIT_LINE#space3d: }")
done

log "=== all passed on $DEVICE_NAME ($UDID); screenshots and logs in $OUT"
for line in "${SUMMARY[@]}"; do log "$line"; done
