#!/usr/bin/env bash
# Android smoke test: build example APKs with `space3d build --target android`, run them on a
# headless emulator (no window, never takes focus) and check that each one renders, survives
# HOME + relaunch, a rotation and Android destroying its activity, gets Back (hello_cube: a tap
# pauses, Back quits) — with no panics, wgpu errors or crashes in logcat. Screenshots and logs land in target/android-smoke/.
#
#   scripts/android-smoke.sh                       # hello_cube + web_demo
#   scripts/android-smoke.sh graphics_showcase     # any examples/<name> with src/lib.rs
#
# Needs what `space3d doctor --platforms android` checks, plus the SDK's emulator and a system
# image (created on first use; see docs/platforms.md "Android smoke test"). Environment:
#
#   SPACE3D_AVD             AVD name (default space3d_smoke; created if missing)
#   SPACE3D_SYSTEM_IMAGE    image for a new AVD (default: Android 17 Google APIs, 16 KB pages)
#   SPACE3D_EMULATOR_GPU    emulator -gpu mode (default host on macOS, swiftshader_indirect elsewhere)
#   SPACE3D_WGPU_BACKEND    wgpu backend on the emulator (default gl, see docs; "" = engine default)
#   SPACE3D_EMULATOR_PORT   console port (default 5680; adb serial emulator-<port>)
#   SPACE3D_RELEASE=1       release APKs
#   SPACE3D_NO_BUILD=1      reuse the APKs from a previous run
#   SPACE3D_KEEP_EMULATOR=1 leave the emulator running (it is shut down otherwise, if this run
#                           started it)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
OUT="$TARGET_DIR/android-smoke"
EXAMPLES=("$@")
[ ${#EXAMPLES[@]} -gt 0 ] || EXAMPLES=(hello_cube web_demo)

if [ -z "${ANDROID_HOME:-}" ]; then
  for d in "$HOME/Library/Android/sdk" "$HOME/Android/Sdk"; do
    [ -d "$d" ] && ANDROID_HOME="$d" && break
  done
fi
: "${ANDROID_HOME:?set ANDROID_HOME to the Android SDK (space3d doctor --platforms android)}"
export ANDROID_HOME
ADB="$ANDROID_HOME/platform-tools/adb"
EMULATOR="$ANDROID_HOME/emulator/emulator"
AVDMANAGER="$ANDROID_HOME/cmdline-tools/latest/bin/avdmanager"
[ -x "$AVDMANAGER" ] || AVDMANAGER="$(command -v avdmanager || true)"
for tool in "$ADB" "$EMULATOR"; do
  [ -x "$tool" ] || { echo "missing $tool: sdkmanager \"platform-tools\" \"emulator\"" >&2; exit 1; }
done

AVD="${SPACE3D_AVD:-space3d_smoke}"
PORT="${SPACE3D_EMULATOR_PORT:-5680}"
SERIAL="emulator-$PORT"
if [ "$(uname -m)" = "arm64" ] || [ "$(uname -m)" = "aarch64" ]; then HOST_ABI=arm64-v8a; else HOST_ABI=x86_64; fi
IMAGE="${SPACE3D_SYSTEM_IMAGE:-system-images;android-37.0;google_apis_ps16k;$HOST_ABI}"
if [ "$(uname -s)" = "Darwin" ]; then DEFAULT_GPU=host; else DEFAULT_GPU=swiftshader_indirect; fi
GPU="${SPACE3D_EMULATOR_GPU:-$DEFAULT_GPU}"
BACKEND="${SPACE3D_WGPU_BACKEND-gl}"
mkdir -p "$OUT"

adb() { "$ADB" -s "$SERIAL" "$@"; }
log() { printf '\033[1m==> %s\033[0m\n' "$*"; }
FAILURES=()
fail() { printf '\033[31mFAIL: %s\033[0m\n' "$*"; FAILURES+=("$*"); }
pass() { printf '\033[32mok: %s\033[0m\n' "$*"; }

# ---- build ---------------------------------------------------------------------------------
if [ -z "${SPACE3D_NO_BUILD:-}" ]; then
  log "building the space3d CLI"
  cargo build -q -p space3d-cli --manifest-path "$ROOT/Cargo.toml"
  for ex in "${EXAMPLES[@]}"; do
    log "space3d build --target android ($ex)"
    (cd "$ROOT/examples/$ex" && "$TARGET_DIR/debug/space3d" build --target android \
      ${SPACE3D_RELEASE:+--release} --out "$OUT/$ex")
  done
fi

# ---- emulator ------------------------------------------------------------------------------
STARTED_EMULATOR=""
cleanup() {
  if [ -n "$STARTED_EMULATOR" ] && [ -z "${SPACE3D_KEEP_EMULATOR:-}" ]; then
    log "shutting the emulator down"
    adb emu kill >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

if ! adb get-state >/dev/null 2>&1; then
  if ! "$EMULATOR" -list-avds | grep -qx "$AVD"; then
    [ -n "$AVDMANAGER" ] || { echo "no avdmanager: sdkmanager \"cmdline-tools;latest\"" >&2; exit 1; }
    [ -d "$ANDROID_HOME/${IMAGE//;//}" ] || { echo "missing system image: sdkmanager \"$IMAGE\"" >&2; exit 1; }
    log "creating AVD $AVD ($IMAGE)"
    echo no | "$AVDMANAGER" create avd -n "$AVD" -k "$IMAGE" -d small_phone >/dev/null
    config="$HOME/.android/avd/$AVD.avd/config.ini"
    sed -i.bak -e 's/^hw.ramSize=.*/hw.ramSize=3072/' -e 's/^hw.gpu.enabled=.*/hw.gpu.enabled=yes/' "$config"
  fi
  log "booting $AVD headless (-gpu $GPU) as $SERIAL"
  "$EMULATOR" -avd "$AVD" -port "$PORT" -no-window -no-audio -no-boot-anim -no-snapshot \
    -gpu "$GPU" -memory 3072 >"$OUT/emulator.log" 2>&1 &
  STARTED_EMULATOR=1
  "$ADB" -s "$SERIAL" wait-for-device
fi
for _ in $(seq 1 180); do
  [ "$(adb shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = "1" ] && break
  sleep 2
done
[ "$(adb shell getprop sys.boot_completed | tr -d '\r')" = "1" ] || { echo "emulator did not boot" >&2; exit 1; }
for s in window_animation_scale transition_animation_scale animator_duration_scale; do
  adb shell settings put global "$s" 0
done
adb shell settings put system accelerometer_rotation 0
adb shell settings put system user_rotation 0
adb shell setprop debug.space3d.wgpu_backend "${BACKEND:-''}"
log "emulator: Android $(adb shell getprop ro.build.version.release | tr -d '\r'), $(adb shell getprop ro.product.cpu.abi | tr -d '\r'), page size $(adb shell getconf PAGE_SIZE | tr -d '\r'), wgpu backend ${BACKEND:-default}"
# A freshly booted emulator may show "System UI / Launcher isn't responding" for a while.
sleep 5
adb shell am broadcast -a android.intent.action.CLOSE_SYSTEM_DIALOGS >/dev/null 2>&1 || true

# ---- checks --------------------------------------------------------------------------------
# `png_check FILE`: prints "WxH distinct=N lit=P%" and fails when the image is (nearly) one color.
png_check() {
  python3 - "$1" <<'PY'
import struct, sys, zlib
data = open(sys.argv[1], "rb").read()
assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
pos, idat, w = 8, b"", 0
while pos < len(data):
    n, kind = struct.unpack(">I4s", data[pos:pos + 8])
    body = data[pos + 8:pos + 8 + n]
    if kind == b"IHDR":
        w, h, depth, ctype = struct.unpack(">IIBB", body[:10])
        bpp = {2: 3, 6: 4}[ctype]
    elif kind == b"IDAT":
        idat += body
    pos += 12 + n
raw, stride = zlib.decompress(idat), w * bpp
prev, rows, i = bytearray(stride), [], 0
for _ in range(h):
    f, line = raw[i], bytearray(raw[i + 1:i + 1 + stride]); i += 1 + stride
    for x in range(stride):
        a = line[x - bpp] if x >= bpp else 0
        b, c = prev[x], (prev[x - bpp] if x >= bpp else 0)
        if f == 1: line[x] = (line[x] + a) & 255
        elif f == 2: line[x] = (line[x] + b) & 255
        elif f == 3: line[x] = (line[x] + (a + b) // 2) & 255
        elif f == 4:
            p = a + b - c; pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
            line[x] = (line[x] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
    rows.append(line); prev = line
colors, lit, total = set(), 0, 0
for y in range(0, h, 4):
    for x in range(0, w, 4):
        px = tuple(rows[y][x * bpp:x * bpp + 3]); colors.add(px); total += 1
        lit += sum(px) > 60
print(f"{w}x{h} distinct={len(colors)} lit={100 * lit // total}%")
sys.exit(0 if len(colors) > 16 and lit * 10 > total else 1)
PY
}

wait_for_log() { # PATTERN SECONDS
  for _ in $(seq 1 "$2"); do
    adb logcat -d -s space3d:V | grep -q "$1" && return 0
    sleep 1
  done
  return 1
}

check_screen() { # NAME DESCRIPTION
  local png="$OUT/$1.png"
  adb exec-out screencap -p >"$png"
  if summary="$(png_check "$png")"; then pass "$2: $summary ($png)"; else fail "$2: looks empty: $summary ($png)"; fi
}

for ex in "${EXAMPLES[@]}"; do
  apk="$OUT/$ex/$ex.apk"
  [ -f "$apk" ] || { fail "$ex: no APK at $apk"; continue; }
  pkg="$(sed -n 's/^application_id = "\(.*\)"/\1/p' "$ROOT/examples/$ex/Cargo.toml" | head -1)"
  [ -n "$pkg" ] || pkg="com.example.$ex"
  activity="$pkg/android.app.NativeActivity"
  rm -f "$OUT/$ex"-*.png "$OUT/$ex"-*.txt
  log "$ex ($pkg): install + launch"
  adb install -r "$apk" >/dev/null
  adb logcat -c
  adb shell am start -W -n "$activity" >/dev/null
  if wait_for_log "space3d renderer initialised" 180; then
    pass "$ex: $(adb logcat -d -s space3d:V | grep -o 'adapter=.*' | head -1)"
  else
    fail "$ex: no renderer after 180 s"
  fi
  sleep 6
  check_screen "$ex-1-start" "$ex renders"
  sleep 1
  adb exec-out screencap -p >"$OUT/$ex-1-start-b.png"
  if cmp -s "$OUT/$ex-1-start.png" "$OUT/$ex-1-start-b.png"; then fail "$ex: the picture does not change (frames stuck?)"; else pass "$ex: frames advance"; fi

  log "$ex: HOME, then relaunch (suspend/resume: surface dropped and recreated)"
  adb shell input keyevent KEYCODE_HOME
  sleep 3
  adb shell am start -W -n "$activity" >/dev/null
  sleep 5
  check_screen "$ex-2-resumed" "$ex renders after resume"

  log "$ex: rotate to landscape"
  adb shell settings put system user_rotation 1
  sleep 5
  check_screen "$ex-3-landscape" "$ex renders in landscape"
  adb shell settings put system user_rotation 0
  sleep 3

  if [ "$ex" = hello_cube ]; then
    log "$ex: tap pauses the cube"
    size="$(adb shell wm size | sed -n 's/.*: *\([0-9]*\)x\([0-9]*\).*/\1 \2/p' | tail -1)"
    adb shell input tap $(( ${size% *} / 2 )) $(( ${size#* } / 2 ))
    sleep 2
    adb exec-out screencap -p >"$OUT/$ex-4-paused.png"; sleep 1.5
    adb exec-out screencap -p >"$OUT/$ex-4-paused-b.png"
    if cmp -s "$OUT/$ex-4-paused.png" "$OUT/$ex-4-paused-b.png"; then pass "$ex: a tap paused the rotation"; else fail "$ex: a tap did not pause the rotation"; fi
    # A second tap resumes it: the frames were paused, not stuck.
    adb shell input tap $(( ${size% *} / 2 )) $(( ${size#* } / 2 ))
    sleep 2
    adb exec-out screencap -p >"$OUT/$ex-4-resumed.png"
    if cmp -s "$OUT/$ex-4-paused-b.png" "$OUT/$ex-4-resumed.png"; then fail "$ex: a second tap did not resume the rotation"; else pass "$ex: a second tap resumed it"; fi
  fi

  log "$ex: Android destroys the activity (its task is removed), then a fresh launch"
  task="$(adb shell am stack list | tr -d '\r' | sed -n "s|.*taskId=\([0-9]*\): $pkg/.*|\1|p" | head -1)"
  adb shell am stack remove "$task"
  for _ in $(seq 1 30); do
    [ -z "$(adb shell pidof "$pkg" | tr -d '\r' || true)" ] && break
    sleep 1
  done
  if [ -z "$(adb shell pidof "$pkg" | tr -d '\r' || true)" ] && adb logcat -d -s space3d:V | grep -q "activity is being destroyed"; then
    pass "$ex: a destroyed activity ends the app"
  else
    fail "$ex: still running after its activity was destroyed"
  fi
  adb shell am start -W -n "$activity" >/dev/null
  wait_for_log "space3d renderer initialised" 180 || true
  sleep 6
  check_screen "$ex-5-relaunched" "$ex renders after a fresh launch"

  log "$ex: Back"
  adb shell input keyevent KEYCODE_BACK
  sleep 4
  running="$(adb shell pidof "$pkg" | tr -d '\r' || true)"
  if [ "$ex" = hello_cube ]; then
    # The game quits on Back (KeyCode::BrowserBack → AppExit): the process ends.
    if [ -z "$running" ]; then pass "$ex: Back quit the app"; else fail "$ex: still running after Back"; fi
  else
    # Back is the game's key (predictive back is off): a game that ignores it keeps running.
    if [ -n "$running" ]; then pass "$ex: Back went to the game"; else fail "$ex: Back closed the app"; fi
  fi
  adb shell am force-stop "$pkg"

  adb logcat -d >"$OUT/$ex-logcat.txt"
  adb logcat -d -s space3d:V >"$OUT/$ex-space3d.txt"
  if grep -E "panicked|wgpu error|GPU device lost|FATAL EXCEPTION|ANR in $pkg|Fatal signal" "$OUT/$ex-logcat.txt" >"$OUT/$ex-errors.txt"; then
    fail "$ex: errors in logcat (see $OUT/$ex-errors.txt):"; head -5 "$OUT/$ex-errors.txt"
  else
    pass "$ex: no panics or wgpu errors in logcat ($OUT/$ex-logcat.txt)"
  fi
done

echo
if [ ${#FAILURES[@]} -eq 0 ]; then
  log "android smoke: all checks passed (${EXAMPLES[*]}); screenshots in $OUT"
else
  log "android smoke: ${#FAILURES[@]} failure(s)"; printf '  - %s\n' "${FAILURES[@]}"; exit 1
fi
