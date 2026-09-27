#!/usr/bin/env node
// Drive one Space3d web build in headless Google Chrome over the DevTools protocol and check
// that it really runs: the renderer initialises on the expected backend, frames are drawn
// (two screenshots differ and are not blank), input reaches the game, assets and the wasm
// arrive over HTTP with the right MIME type, and nothing panics or logs an error.
//
// Usually run by scripts/web-smoke.sh (which builds and serves the example). By hand:
//
//   node scripts/web-smoke.mjs --url 'http://127.0.0.1:8080/?webgl2' --backend webgl2 \
//     --name web_demo-webgl2 --out target/web-smoke --click --expect-log 'palette 2'
//
// Options (all optional except --url; --help prints this):
//   --url URL             page to open
//   --name NAME           file prefix for screenshots/logs (default: "page")
//   --out DIR             output directory (default: target/web-smoke)
//   --backend B           webgpu | webgl2 | any: backend the engine must report (default any)
//   --no-webgpu           start Chrome without a WebGPU adapter: the WebGL2 fallback
//   --chrome PATH         Chrome binary (default: $CHROME, else the macOS/Linux default path)
//   --profile DIR         throwaway --user-data-dir (default: $TMPDIR/space3d-web-smoke-chrome)
//   --size WxH            viewport (default 1280x720)
//   --timeout SECS        wait for the renderer at most this long (default 120)
//   --settle SECS         run this long after the renderer exists before the first shot (default 3)
//   --ready-log REGEX     also wait for this console line before the first shot
//   --click               click the canvas centre (pointer down/up) as the input test
//   --key CODE            press a key (KeyboardEvent.code, e.g. Digit2, KeyQ); repeatable
//   --wheel DY            scroll the wheel over the canvas centre by DY pixels
//   --expect-log REGEX    console line that must appear after the input; repeatable
//   --expect-request RE   a request whose URL matches must succeed (e.g. assets/); repeatable
//   --expect-color-shift  the mean colour (chromaticity) of lit pixels must move after the input:
//                         by 0.04 at least, and 1.5× more than between the shots before it
//   --expect-audio        the game's AudioContext is suspended before the input (browser autoplay
//                         policy) and running after it (the input is the page's first gesture)
//   --min-lit FRACTION    fraction of pixels brighter than the background (default 0.02)
//   --allow-error REGEX   console errors matching this are not failures; repeatable
//   --keep-open           leave Chrome running (debugging)
//
// Exit status 0 when every check passed; the summary is printed as JSON and written to
// <out>/<name>.json, console output to <out>/<name>.console.log, screenshots to
// <out>/<name>-{1,2,3}.png. Needs Node 22+ (global fetch/WebSocket); no npm packages.

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { inflateSync } from "node:zlib";

// ------------------------------------------------------------------------------ arguments

function parseArgs(argv) {
  const repeat = new Set(["key", "expect-log", "expect-request", "allow-error"]);
  const flags = new Set(["click", "expect-color-shift", "expect-audio", "keep-open", "no-webgpu", "help"]);
  const o = { key: [], "expect-log": [], "expect-request": [], "allow-error": [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith("--")) throw new Error(`unexpected argument ${a}`);
    const k = a.slice(2);
    if (flags.has(k)) {
      o[k] = true;
      continue;
    }
    const v = argv[++i];
    if (v === undefined) throw new Error(`${a} needs a value`);
    if (repeat.has(k)) o[k].push(v);
    else o[k] = v;
  }
  if (o.help) {
    // The header comment is the manual.
    const self = readFileSync(new URL(import.meta.url), "utf8").split("\n");
    console.log(self.slice(1, self.findIndex((l) => l.startsWith("import"))).map((l) => l.replace(/^\/\/ ?/, "")).join("\n"));
    process.exit(0);
  }
  if (!o.url) throw new Error("--url is required (--help for the options)");
  return o;
}

const opts = parseArgs(process.argv.slice(2));
const name = opts.name ?? "page";
const outDir = resolve(opts.out ?? "target/web-smoke");
const backend = (opts.backend ?? "any").toLowerCase();
const [width, height] = (opts.size ?? "1280x720").split("x").map(Number);
const timeoutMs = Number(opts.timeout ?? 120) * 1000;
const settleMs = Number(opts.settle ?? 3) * 1000;
const minLit = Number(opts["min-lit"] ?? 0.02);
const profile = resolve(opts.profile ?? join(tmpdir(), "space3d-web-smoke-chrome"));
const chrome =
  opts.chrome ??
  process.env.CHROME ??
  (process.platform === "darwin"
    ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    : "google-chrome");
mkdirSync(outDir, { recursive: true });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ------------------------------------------------------------------------------ chrome

/// Start headless Chrome on a free DevTools port; resolves to { proc, port }.
async function launchChrome() {
  mkdirSync(profile, { recursive: true });
  const portFile = join(profile, "DevToolsActivePort");
  rmSync(portFile, { force: true });
  const args = [
    "--headless=new",
    `--user-data-dir=${profile}`,
    "--remote-debugging-port=0",
    `--window-size=${width},${height}`,
    "--no-first-run",
    "--no-default-browser-check",
    // Never touch the login keychain (a keychain prompt would take the user's focus).
    "--use-mock-keychain",
    "--password-store=basic",
    "--disable-extensions",
    "--disable-sync",
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-default-apps",
    "--hide-scrollbars",
    "--mute-audio",
    // Chrome's desktop default: an AudioContext stays suspended until a user gesture.
    "--autoplay-policy=document-user-activation-required",
    // Keep rAF running at full rate in a page nobody looks at.
    "--disable-background-timer-throttling",
    "--disable-renderer-backgrounding",
    "--disable-backgrounding-occluded-windows",
    ...(opts["no-webgpu"]
      ? // No WebGPU adapter (`navigator.gpu` exists, `requestAdapter()` resolves to null — as on
        // a blocklisted GPU): what wgpu's WebGL2 fallback is for.
        ["--disable-features=WebGPUService"]
      : // WebGPU even where the GPU is not on Chrome's allow list (headless, some Linux drivers).
        ["--enable-unsafe-webgpu", ...(process.platform === "linux" ? ["--enable-features=Vulkan"] : [])]),
    "about:blank",
  ];
  const proc = spawn(chrome, args, { stdio: ["ignore", "ignore", "pipe"] });
  let stderr = "";
  proc.stderr.on("data", (d) => {
    stderr += d;
    if (stderr.length > 1 << 20) stderr = stderr.slice(-(1 << 19));
  });
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    if (proc.exitCode !== null) throw new Error(`Chrome exited (${proc.exitCode}):\n${stderr}`);
    if (existsSync(portFile)) {
      const port = Number(readFileSync(portFile, "utf8").split("\n")[0]);
      if (port > 0) return { proc, port, stderr: () => stderr };
    }
    await sleep(100);
  }
  proc.kill("SIGKILL");
  throw new Error(`Chrome did not open a DevTools port within 30 s:\n${stderr}`);
}

/// Minimal CDP client on Node's built-in WebSocket.
class Cdp {
  constructor(ws) {
    this.ws = ws;
    this.nextId = 1;
    this.pending = new Map();
    this.handlers = [];
    ws.addEventListener("message", (ev) => {
      const msg = JSON.parse(typeof ev.data === "string" ? ev.data : Buffer.from(ev.data).toString());
      if (msg.id !== undefined) {
        const p = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (!p) return;
        if (msg.error) p.reject(new Error(`${p.method}: ${msg.error.message}`));
        else p.resolve(msg.result);
      } else {
        for (const h of this.handlers) h(msg.method, msg.params);
      }
    });
  }

  static async connect(url) {
    const ws = new WebSocket(url);
    await new Promise((res, rej) => {
      ws.addEventListener("open", res, { once: true });
      ws.addEventListener("error", () => rej(new Error(`cannot connect to ${url}`)), { once: true });
    });
    return new Cdp(ws);
  }

  send(method, params = {}) {
    const id = this.nextId++;
    this.ws.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject, method }));
  }

  on(handler) {
    this.handlers.push(handler);
  }

  close() {
    this.ws.close();
  }
}

// ------------------------------------------------------------------------------ PNG

/// Decode an 8-bit RGB/RGBA, non-interlaced PNG (what Chrome's screenshots are).
function decodePng(buf) {
  let off = 8;
  let w = 0;
  let h = 0;
  let colorType = 0;
  const idat = [];
  while (off < buf.length) {
    const len = buf.readUInt32BE(off);
    const type = buf.toString("ascii", off + 4, off + 8);
    const data = buf.subarray(off + 8, off + 8 + len);
    if (type === "IHDR") {
      w = data.readUInt32BE(0);
      h = data.readUInt32BE(4);
      if (data[8] !== 8 || data[12] !== 0) throw new Error("unsupported PNG (bit depth/interlace)");
      colorType = data[9];
    } else if (type === "IDAT") idat.push(data);
    else if (type === "IEND") break;
    off += 12 + len;
  }
  const bpp = { 2: 3, 6: 4 }[colorType];
  if (!bpp) throw new Error(`unsupported PNG colour type ${colorType}`);
  const raw = inflateSync(Buffer.concat(idat));
  const stride = w * bpp;
  const px = Buffer.alloc(h * stride);
  for (let y = 0; y < h; y++) {
    const filter = raw[y * (stride + 1)];
    const src = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
    const row = px.subarray(y * stride, (y + 1) * stride);
    const prev = y > 0 ? px.subarray((y - 1) * stride, y * stride) : null;
    for (let x = 0; x < stride; x++) {
      const a = x >= bpp ? row[x - bpp] : 0;
      const b = prev ? prev[x] : 0;
      const c = prev && x >= bpp ? prev[x - bpp] : 0;
      let v = src[x];
      if (filter === 1) v += a;
      else if (filter === 2) v += b;
      else if (filter === 3) v += (a + b) >> 1;
      else if (filter === 4) {
        const p = a + b - c;
        const pa = Math.abs(p - a);
        const pb = Math.abs(p - b);
        const pc = Math.abs(p - c);
        v += pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
      }
      row[x] = v & 0xff;
    }
  }
  return { w, h, bpp, px };
}

/// Lit-pixel statistics against the corner colour (the page/clear colour).
function imageStats(png) {
  const { w, h, bpp, px } = png;
  const bg = [px[0], px[1], px[2]];
  let lit = 0;
  const sum = [0, 0, 0];
  for (let i = 0; i < w * h; i++) {
    const o = i * bpp;
    const d = Math.abs(px[o] - bg[0]) + Math.abs(px[o + 1] - bg[1]) + Math.abs(px[o + 2] - bg[2]);
    if (d > 30) {
      lit++;
      sum[0] += px[o];
      sum[1] += px[o + 1];
      sum[2] += px[o + 2];
    }
  }
  const mean = lit ? sum.map((s) => s / lit) : [0, 0, 0];
  // Chromaticity (colour independent of brightness), for the colour-shift check.
  const total = mean[0] + mean[1] + mean[2] || 1;
  return { lit: lit / (w * h), bg, mean: mean.map(Math.round), chroma: mean.map((m) => m / total) };
}

/// Fraction of pixels that differ noticeably between two same-sized screenshots.
function changedFraction(a, b) {
  if (a.w !== b.w || a.h !== b.h) return 1;
  let changed = 0;
  for (let i = 0; i < a.w * a.h; i++) {
    const oa = i * a.bpp;
    const ob = i * b.bpp;
    const d =
      Math.abs(a.px[oa] - b.px[ob]) + Math.abs(a.px[oa + 1] - b.px[ob + 1]) + Math.abs(a.px[oa + 2] - b.px[ob + 2]);
    if (d > 24) changed++;
  }
  return changed / (a.w * a.h);
}

const chromaDistance = (a, b) => Math.hypot(a[0] - b[0], a[1] - b[1], a[2] - b[2]);

// ------------------------------------------------------------------------------ keys

/// CDP key event fields for a KeyboardEvent.code.
function keyInfo(code) {
  let m;
  if ((m = /^Digit(\d)$/.exec(code))) return { key: m[1], text: m[1], vk: 48 + Number(m[1]) };
  if ((m = /^Key([A-Z])$/.exec(code))) {
    const k = m[1].toLowerCase();
    return { key: k, text: k, vk: m[1].charCodeAt(0) };
  }
  const named = {
    Space: { key: " ", text: " ", vk: 32 },
    Enter: { key: "Enter", text: "\r", vk: 13 },
    Escape: { key: "Escape", vk: 27 },
    Tab: { key: "Tab", vk: 9 },
    ArrowLeft: { key: "ArrowLeft", vk: 37 },
    ArrowUp: { key: "ArrowUp", vk: 38 },
    ArrowRight: { key: "ArrowRight", vk: 39 },
    ArrowDown: { key: "ArrowDown", vk: 40 },
    Equal: { key: "=", text: "=", vk: 187 },
    Minus: { key: "-", text: "-", vk: 189 },
    Backquote: { key: "`", text: "`", vk: 192 },
  };
  if (named[code]) return named[code];
  throw new Error(`unknown key code ${code}`);
}

// ------------------------------------------------------------------------------ run

const failures = [];
const fail = (msg) => failures.push(msg);
const consoleLines = [];
const errors = [];
const warnings = [];
const exceptions = [];
const responses = new Map();
const failedRequests = [];

/// Format console arguments, applying `%c` (tracing-wasm colours each part of its lines).
function formatArgs(args) {
  const str = (a) => (a.type === "string" ? a.value : (a.description ?? JSON.stringify(a.value)));
  if (!args.length) return "";
  let first = str(args[0]);
  let rest = args.slice(1);
  if (args[0].type === "string" && first.includes("%c")) {
    const n = first.split("%c").length - 1;
    first = first.replaceAll("%c", "");
    rest = rest.slice(n);
  }
  return [first, ...rest.map(str)].join(" ");
}

function lineMatches(re) {
  return consoleLines.some((l) => re.test(l.text));
}

async function waitFor(pred, ms, what) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (pred()) return true;
    if (exceptions.length || errors.some((e) => /panicked at/.test(e))) break;
    await sleep(100);
  }
  if (!pred()) {
    fail(`timed out waiting for ${what}`);
    return false;
  }
  return true;
}

const started = Date.now();
const elapsed = () => ((Date.now() - started) / 1000).toFixed(1);
const log = (m) => console.error(`[${elapsed()}s] ${name}: ${m}`);
let chromeProc;
let cdp;
const summary = { name, url: opts.url, backend };
try {
  const { proc, port, stderr } = await launchChrome();
  chromeProc = proc;
  summary.chromeStderr = stderr;
  const version = await (await fetch(`http://127.0.0.1:${port}/json/version`)).json();
  summary.browser = version.Browser;
  const targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
  const page = targets.find((t) => t.type === "page");
  if (!page) throw new Error("no page target");
  cdp = await Cdp.connect(page.webSocketDebuggerUrl);
  cdp.on((method, p) => {
    if (method === "Runtime.consoleAPICalled") {
      const text = formatArgs(p.args);
      consoleLines.push({ t: elapsed(), level: p.type, text });
      // The engine logs at the console level of each event; `ERROR `/`WARN ` prefixes also
      // catch loggers that print everything with console.log.
      if (p.type === "error" || p.type === "assert" || /^ERROR /.test(text)) errors.push(text);
      else if (p.type === "warning" || /^WARN /.test(text)) warnings.push(text);
    } else if (method === "Runtime.exceptionThrown") {
      const d = p.exceptionDetails;
      const text = `${d.text} ${d.exception?.description ?? ""}`.trim();
      exceptions.push(text);
      consoleLines.push({ t: elapsed(), level: "exception", text });
    } else if (method === "Log.entryAdded") {
      const e = p.entry;
      const text = `[${e.source}] ${e.text}${e.url ? ` (${e.url})` : ""}`;
      consoleLines.push({ t: elapsed(), level: `browser-${e.level}`, text });
      if (e.level === "error") errors.push(text);
      if (e.level === "warning") warnings.push(text);
    } else if (method === "Network.responseReceived") {
      const r = p.response;
      responses.set(p.requestId, { url: r.url, status: r.status, mimeType: r.mimeType, headers: r.headers });
    } else if (method === "Network.loadingFailed") {
      if (!p.canceled) failedRequests.push({ requestId: p.requestId, error: p.errorText });
    }
  });
  await Promise.all([
    cdp.send("Runtime.enable"),
    cdp.send("Log.enable"),
    cdp.send("Network.enable"),
    cdp.send("Page.enable"),
  ]);
  await cdp.send("Network.setCacheDisabled", { cacheDisabled: true });
  await cdp.send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false });
  // Count the browser's animation frames (the engine draws one frame per rAF), and keep the
  // AudioContexts the page creates to report their state (suspended until a user gesture).
  await cdp.send("Page.addScriptToEvaluateOnNewDocument", {
    source: `window.__smokeFrames = 0;
      (function t() { window.__smokeFrames++; requestAnimationFrame(t); })();
      window.__smokeAudio = [];
      if (window.AudioContext) {
        const Base = window.AudioContext;
        window.AudioContext = class extends Base {
          constructor(...a) { super(...a); window.__smokeAudio.push(this); }
        };
      }`,
  });
  const audioStates = async () =>
    (await cdp.send("Runtime.evaluate", { expression: "window.__smokeAudio.map((c) => c.state)", returnByValue: true }))
      .result.value;
  log(`opening ${opts.url} (${summary.browser})`);
  await cdp.send("Page.navigate", { url: opts.url });

  // 1. The renderer exists, on the expected backend.
  const initRe = /space3d renderer initialised/;
  await waitFor(() => lineMatches(initRe), timeoutMs, "`space3d renderer initialised` in the console");
  const initLine = consoleLines.find((l) => initRe.test(l.text));
  if (initLine) {
    summary.renderer = initLine.text.replace(/^.*?space3d renderer initialised\s*/, "").replace(/\s*\n\s*/g, " ");
    const m = /backend\s*=\s*(\w+)/.exec(initLine.text);
    summary.reportedBackend = m?.[1];
    summary.rendererAfterSecs = Number(initLine.t);
    log(`renderer: ${summary.renderer}`);
    const isGl = summary.reportedBackend === "Gl";
    if (backend === "webgl2" && !isGl) fail(`expected the WebGL2 (Gl) backend, got ${summary.reportedBackend}`);
    if (backend === "webgpu" && summary.reportedBackend !== "BrowserWebGpu")
      fail(`expected the WebGPU (BrowserWebGpu) backend, got ${summary.reportedBackend}`);
  }
  summary.navigatorGpu = (
    await cdp.send("Runtime.evaluate", { expression: "typeof navigator.gpu", returnByValue: true })
  ).result.value;
  if (opts["ready-log"]) {
    await waitFor(() => lineMatches(new RegExp(opts["ready-log"])), timeoutMs, `/${opts["ready-log"]}/`);
  }

  // 2. Frames: let it run, then two screenshots half a second apart.
  const frames0 = (await cdp.send("Runtime.evaluate", { expression: "window.__smokeFrames", returnByValue: true }))
    .result.value;
  await sleep(settleMs);
  const shoot = async (i) => {
    const { data } = await cdp.send("Page.captureScreenshot", { format: "png" });
    const file = join(outDir, `${name}-${i}.png`);
    const buf = Buffer.from(data, "base64");
    writeFileSync(file, buf);
    return { file, png: decodePng(buf) };
  };
  const shot1 = await shoot(1);
  await sleep(500);
  const shot2 = await shoot(2);
  const frames1 = (await cdp.send("Runtime.evaluate", { expression: "window.__smokeFrames", returnByValue: true }))
    .result.value;
  summary.framesDuringSettle = frames1 - frames0;
  summary.fps = Math.round((frames1 - frames0) / ((settleMs + 500) / 1000));
  const s1 = imageStats(shot1.png);
  const s2 = imageStats(shot2.png);
  summary.screenshots = [shot1.file, shot2.file];
  summary.lit = [s1.lit, s2.lit].map((v) => +v.toFixed(4));
  summary.audioContexts = { beforeInput: await audioStates() };
  summary.animated = +changedFraction(shot1.png, shot2.png).toFixed(4);
  log(`frames ${summary.framesDuringSettle} (~${summary.fps}/s), lit ${summary.lit}, changed ${summary.animated}`);
  if (summary.framesDuringSettle < 10) fail(`only ${summary.framesDuringSettle} animation frames ran`);
  if (s1.lit < minLit || s2.lit < minLit) fail(`screen nearly blank (lit fraction ${summary.lit}, need ${minLit})`);
  if (summary.animated === 0) fail("two screenshots 0.5 s apart are identical (no frames drawn?)");

  // 3. Input.
  const inputStart = consoleLines.length;
  const cx = Math.round(width / 2);
  const cy = Math.round(height / 2);
  const hadInput = opts.click || opts.key.length || opts.wheel;
  if (hadInput) {
    // Pointer over the canvas first (focus follows the click; keys need a focused canvas).
    await cdp.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: cx, y: cy });
    if (opts.click) {
      await cdp.send("Input.dispatchMouseEvent", { type: "mousePressed", x: cx, y: cy, button: "left", clickCount: 1 });
      await sleep(80);
      await cdp.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: cx, y: cy, button: "left", clickCount: 1 });
      await sleep(150);
    } else {
      await cdp.send("Runtime.evaluate", { expression: "document.querySelector('canvas')?.focus()" });
    }
    for (const code of opts.key) {
      const k = keyInfo(code);
      const base = { code, key: k.key, windowsVirtualKeyCode: k.vk, nativeVirtualKeyCode: k.vk };
      await cdp.send("Input.dispatchKeyEvent", { type: k.text ? "keyDown" : "rawKeyDown", ...base, text: k.text });
      await sleep(80);
      await cdp.send("Input.dispatchKeyEvent", { type: "keyUp", ...base });
      await sleep(150);
    }
    if (opts.wheel) {
      await cdp.send("Input.dispatchMouseEvent", { type: "mouseWheel", x: cx, y: cy, deltaX: 0, deltaY: Number(opts.wheel) });
    }
    await sleep(300);
    summary.audioContexts.afterInput = await audioStates();
    for (const re of opts["expect-log"]) {
      const rx = new RegExp(re);
      await waitFor(() => consoleLines.slice(inputStart).some((l) => rx.test(l.text)), 10_000, `/${re}/ after the input`);
    }
    if (opts["expect-audio"]) {
      await waitFor(
        () => consoleLines.slice(inputStart).some((l) => /audio output running/.test(l.text)),
        5_000,
        "`audio output running` after the first gesture",
      );
      summary.audioContexts.afterInput = await audioStates();
      const { beforeInput, afterInput } = summary.audioContexts;
      if (!beforeInput.length) fail("the page created no AudioContext");
      if (beforeInput.some((st) => st !== "suspended"))
        fail(`AudioContext ${beforeInput} before any gesture (expected suspended: is the autoplay policy on?)`);
      if (!afterInput.length || afterInput.some((st) => st !== "running"))
        fail(`AudioContext ${afterInput} after the first gesture (expected running)`);
    }
    await sleep(800);
    const shot3 = await shoot(3);
    summary.screenshots.push(shot3.file);
    const s3 = imageStats(shot3.png);
    // Chromaticity distances: between the two shots before the input (animation alone), and
    // from the shot after it to the nearer of those two.
    summary.colorShift = {
      beforeInput: +chromaDistance(s1.chroma, s2.chroma).toFixed(4),
      afterInput: +Math.min(chromaDistance(s1.chroma, s3.chroma), chromaDistance(s2.chroma, s3.chroma)).toFixed(4),
      mean: [s1.mean, s2.mean, s3.mean],
    };
    log(`after input: lit ${s3.lit.toFixed(4)}, colour shift ${JSON.stringify(summary.colorShift)}`);
    if (s3.lit < minLit) fail(`screen nearly blank after the input (lit fraction ${s3.lit.toFixed(4)})`);
    if (opts["expect-color-shift"]) {
      const { beforeInput, afterInput } = summary.colorShift;
      if (!(afterInput > Math.max(0.04, 1.5 * beforeInput)))
        fail(`input did not change the colours (shift ${afterInput} after vs ${beforeInput} before)`);
    }
  }

  // 4. Network: the wasm MIME type, expected requests, no failures.
  await sleep(300);
  const resps = [...responses.values()];
  summary.requests = resps.map((r) => `${r.status} ${r.mimeType} ${r.url}`);
  const wasm = resps.filter((r) => r.url.endsWith(".wasm"));
  if (!wasm.length) fail("no .wasm was fetched");
  for (const r of wasm) {
    if (r.status !== 200) fail(`${r.url}: HTTP ${r.status}`);
    if (r.mimeType !== "application/wasm") fail(`${r.url}: MIME ${r.mimeType}, want application/wasm`);
  }
  // The browser's own /favicon.ico probe is not the page's business.
  const isFavicon = (text) => /\/favicon\.ico\b/.test(text);
  for (const r of resps) if (r.status >= 400 && !isFavicon(r.url)) fail(`${r.url}: HTTP ${r.status}`);
  for (const f of failedRequests) fail(`request failed: ${f.error}`);
  for (const re of opts["expect-request"]) {
    const rx = new RegExp(re);
    if (!resps.some((r) => rx.test(r.url) && r.status === 200)) fail(`no successful request matching /${re}/`);
  }

  // 5. Console: no exceptions, panics or errors (unless allowed).
  const allowed = opts["allow-error"].map((r) => new RegExp(r));
  for (const e of exceptions) fail(`uncaught exception: ${e}`);
  for (const e of errors) if (!allowed.some((r) => r.test(e)) && !isFavicon(e)) fail(`console error: ${e}`);
  summary.warnings = warnings;
  summary.errors = errors;
} catch (e) {
  fail(`smoke driver: ${e.stack ?? e}`);
} finally {
  writeFileSync(
    join(outDir, `${name}.console.log`),
    consoleLines.map((l) => `[${l.t}s] ${l.level}: ${l.text}`).join("\n") + "\n",
  );
  summary.chromeStderr = undefined;
  summary.failures = failures;
  summary.ok = failures.length === 0;
  writeFileSync(join(outDir, `${name}.json`), JSON.stringify(summary, null, 2) + "\n");
  console.log(JSON.stringify(summary, null, 2));
  if (!opts["keep-open"]) {
    try {
      cdp?.close();
    } catch {}
    chromeProc?.kill("SIGTERM");
    await sleep(500);
    if (chromeProc && chromeProc.exitCode === null) chromeProc.kill("SIGKILL");
  }
  log(summary.ok ? "PASS" : `FAIL (${failures.length})\n  - ${failures.join("\n  - ")}`);
  process.exit(summary.ok ? 0 : 1);
}
