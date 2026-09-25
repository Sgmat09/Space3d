# On-device AI (`space3d-ai`)

`space3d-ai` gives games agent sessions with **real tool calls into the engine**, NPC dialogue with streamed lines,
enum-constrained decisions and schema-shaped content, on the models built into the player's device:

- **Apple Foundation Models** (macOS, iOS and visionOS 27) through [open-apple-models].
- **Gemini Nano** (Android AICore devices, via the ML Kit GenAI Prompt API) through [open-android-models].

Both libraries speak the same JSON-RPC 2.0 bridge protocol, v1.0 (open-apple-models `docs/PROTOCOL.md`). The engine
implements the protocol once and the **model backend is injected** per platform:

| Backend | Where | Transport |
|---|---|---|
| `AppleBackend` | Apple platforms, with the `apple-ffi` feature | open-apple-models' C ABI (`oam_bridge_create/send/destroy`) |
| `AndroidBackend` | Android, in a Gradle-built host app ([ai-android.md](ai-android.md)) | JNI into `com.spacecorps.oam.jni.OamJni` |
| `ScriptedBackend` | everywhere, including `wasm32` | pure Rust: plays scripted models in-process |
| `UnsupportedBackend` | fallback | every request fails with `AiErrorCode::Unsupported` |

Enable it with the facade feature `ai` (`space3d::ai`, common types in the prelude), or depend on `space3d-ai` directly.
The `ai-apple-ffi` feature also links Apple's bridge.

## Frame model: nothing blocks

```text
game system ── Ai::talk(...) ─────────▶ backend.send(line)       (returns at once with a Task)
                                        bridge threads / JVM / scripted engine
Stage::PreUpdate  ai::pump ◀─── Inbox ◀─ delivered lines           responses → Tasks
                                                                   notifications → Events<AiEvent>
                                                                   tool/call → queue
Stage::Update     ai::run_tools          your AiTool handlers run with &mut SystemCtx, reply
```

- Every `Ai` method sends one request and returns a `space3d_tasks::Task<Result<T, AiError>>`. Poll it from a system
  or hand it to `AsyncQueue::track`. The bridge handles requests in arrival order, so `create_npc` followed by
  `talk` needs no waiting.
- Streaming turns (on by default) emit `AiEvent`s: session text deltas, NPC emotions and line deltas, and tool calls
  starting and completing. Every event of a request arrives before its response.
- Model `tool/call`s run **your Rust closures on the engine thread**, inside `Stage::Update`, with full ECS access.
- Backends deliver lines from their own threads (the Apple bridge's callback thread, the JVM delivery thread) into a
  lock-protected `Inbox`. Engine state is only touched by the two systems.

## Quick start

```rust
use serde_json::json;
use space3d::prelude::*;

struct Stock(u32);

fn build(app: &mut App) {
    app.add_plugin(DefaultPlugins::new());
    app.insert_resource(Stock(3));
    app.add_plugin(
        // The platform's backend where one is built in (Apple with `ai-apple-ffi`, Android),
        // a scripted model elsewhere (web, Linux, Windows, CI).
        AiPlugin::new(BackendChoice::auto_or(ScriptedBackend::new())).with_tool(
            AiTool::new("check_inventory", "How many of an item are in stock, and its price in gold.")
                .parameters(json!({"type": "object", "properties": {"item": {"type": "string"}}, "required": ["item"]}))
                .handler(|ctx, call| {
                    let item = call.string("item").map_err(|e| e.to_string())?;
                    Ok::<_, String>(json!({"item": item, "stock": ctx.resources.expect::<Stock>().0, "price_gold": 45}))
                }),
        ),
    );
    app.add_system(Stage::Startup, |ctx| {
        let ai = ctx.resources.expect_mut::<Ai>();
        let persona = Persona::new("Gorm").role("the village blacksmith").personality("Gruff but fair.");
        let _ = ai.create_npc("gorm", NpcConfig::new(persona).grounding_tool("check_inventory"));
        let turn = ai.talk("gorm", "Got any iron swords?");
        ctx.resources.expect_mut::<AsyncQueue>().track(turn, |_ctx, result| match result {
            Ok(turn) => println!("Gorm ({}): {}", turn.emotion, turn.line),
            Err(error) if error.is_unavailable() => println!("No on-device model: show scripted content"),
            Err(error) => println!("Gorm is silent: {error}"),
        });
    });
    // Typewriter effect: read the streamed deltas.
    let mut reader = EventReader::default();
    app.add_system(Stage::Update, move |ctx| {
        for event in reader.read(ctx.resources.expect::<Events<AiEvent>>()) {
            if let AiEvent::Npc { event: NpcEvent::LineDelta(delta), .. } = event {
                print!("{delta}");
            }
        }
    });
}
```

`examples/ai_tavern` is a complete game: Mira the innkeeper, whose `check_menu` and `take_order` tools read and write
ECS entities and resources, with streamed lines, emotion-tinted visuals and an egui dialogue window.

## The `Ai` resource

| Method | Protocol | Result |
|---|---|---|
| `availability()` | `model/availability` | `ModelAvailability` |
| `create_session(id, SessionConfig)` | `session/create` | `Created` |
| `respond(session, prompt)` / `respond_with(…, RespondOptions)` | `session/respond` | `AgentResponse` (text, structured, tool calls, usage, steps) |
| `cancel_session`, `reset_session`, `delete_session`, `set_context_note` | `session/*` | |
| `create_npc(id, NpcConfig)` | `npc/create` | `Created` (includes built-in world and memory tools) |
| `talk(npc, line)` / `talk_with(…, TalkOptions)` | `npc/talk` | `DialogueTurn` (line, emotion, player options, ends conversation, relationship, fallback) |
| `bark(npc, situation)` | `npc/bark` | `String` |
| `cancel_npc`, `reset_npc`, `delete_npc` | `npc/*` | |
| `decide(DecisionRequest)` | `decision/decide` | `Decision` (option id, reasoning, confidence, fallback) |
| `generate_content(ContentRequest)` | `content/generate` | `serde_json::Value` matching the schema |
| `create_world`, `world_get`, `world_set`, `world_merge` | `world/*` | versions / values |
| `request(method, params)`, `notify(method, params)` | anything else | raw `Value` |
| `shutdown()` | `shutdown` | later requests fail with `shut_down` |

`status()` becomes `AiStatus::Ready(ServerInfo)` once the bridge answers `initialize` (`AiPlugin` sends it at startup),
or `AiStatus::Unavailable(error)` when there is no usable backend. `last_request_id()` matches `AiEvent`s and tool calls
to the request that caused them. Types mirror the libraries: `Persona`, `Emotion` (lenient parsing, as in Swift and
Kotlin), `NpcOptions`, `NpcMemory`, `ToolChoice` (`Auto`, `None`, `Required`, `Explicit`, `Tool(name)`), `ToolCall`,
`ToolOutput`, `ToolRecord`, `TokenUsage`, `ModelStep`.

## Tools

```rust
AiTool::new("open_gate", "Open a named gate. Returns whether it opened.")
    .parameters(json!({"type": "object", "properties": {"gate": {"type": "string"}}, "required": ["gate"]}))
    .timeout_seconds(30.0)
    .handler(|ctx: &mut SystemCtx, call: &ToolInvocation| { /* … */ })
```

- Register tools with `AiPlugin::with_tool` or `Ai::register_tool`. Sessions, NPCs, decisions and content requests
  name the tools they use (`SessionConfig::tool("open_gate")`). An unregistered name fails the request locally
  with `invalid_params`.
- A handler returns anything convertible into a `ToolReply`: a `ToolOutput`, a `String` or `&str` (text), a
  `serde_json::Value` (JSON), or a `Result<_, impl Display>`. Errors become error outputs the model reads as
  `Error: …` and may recover from; a tool error never fails the turn.
- `ToolInvocation` dereferences to the `ToolCall` (`call.string("item")`, `call.i64("n")`, `call.decode::<T>()`).
  It also carries the `owner` (`Session`, `Npc` or `Request`), the `request` id and extra context such as `index`
  for `decision/decideMany`.
- Handlers run inside the frame, so keep them quick. For anything longer (an animation, a network call) keep
  `call.responder()` and return `ToolReply::Deferred`, then call `responder.reply(output)` later from any system or
  thread. If nobody replies, the bridge times the call out (default 120 s, per tool `timeout_seconds`, `0` = never).
  It then sends `tool/cancel`: `responder.is_cancelled()` becomes true, late replies are dropped, and
  `AiEvent::ToolCancelled` is emitted.
- Unit-test a tool without a model: `tool.invoke(&mut ctx, ToolCall { … })` runs its handler directly.

## Events

`Events<AiEvent>` (sent in `Stage::PreUpdate`, readable for two frames with an `EventReader`):

| Event | From |
|---|---|
| `Session { session, request, event: TurnEvent }` | `session/event`: `ModelStep`, `Text { delta, text, is_reset }`, `Partial`, `ToolCallStarted`, `ToolCallCompleted` |
| `Npc { npc, request, event: NpcEvent }` | `npc/event`: `Emotion` (sent early), `LineDelta`, `LineReset`, tool calls |
| `ToolCancelled { rpc_id, call_id, owner, request, reason }` | `tool/cancel` |
| `WorldChanged(WorldChange)` | `world/changed` (after `request("world/subscribe", …)`) |
| `Ready(ServerInfo)` / `Unavailable(AiError)` | the `initialize` answer |
| `Notification { method, params }` | extension notifications (`quest/event`, …) |
| `BackendError(AiError)` | a line that could not be parsed |

For a typewriter effect, append `LineDelta` text and replace it on `LineReset`. By the time the task resolves, the
deltas and resets add up exactly to `DialogueTurn::line`.

## Choosing the backend

```rust
AiPlugin::auto()                                                // BackendChoice::Auto
AiPlugin::new(BackendChoice::auto_or(ScriptedBackend::new()))   // Auto, else scripted
AiPlugin::scripted(ScriptedBackend::new())                      // always scripted (tests, CI)
AiPlugin::new(BackendChoice::backend(MyTransport::new()))       // any `impl AiBackend`
```

`Auto` resolves to `AppleBackend` on Apple targets built with `apple-ffi`. On Android it resolves to
`AndroidBackend::from_android_context()`, which uses the context that android-activity publishes through
ndk-context. Everywhere else it resolves to `UnsupportedBackend`, with a reason. A backend that fails to start, for
example an Android app without the open-android-models AAR, is replaced by an `UnsupportedBackend` that explains why.

`auto_or`'s fallback is used only where no platform backend is built in: the web, Windows, Linux, and Apple
targets without `apple-ffi`. On a Mac built with `ai-apple-ffi`, or on any Android device, the platform backend is
used even when the device has no model (Apple Intelligence off, no AICore), and a platform backend that fails to
start becomes an `UnsupportedBackend`, not the fallback. Requests then fail with `model_unavailable` or
`unsupported`. `AiError::is_unavailable()` covers both: show scripted content then.

Custom transports implement `AiBackend`: `kind()`, `start(inbox)`, `send(line)`, and optionally `poll()` and
`shutdown()`.
Deliver every line the bridge writes to `inbox.deliver(line)`, from any thread, in order.

## Scripted models

A `Script` is sent as a request's `model` (`{"type": "scripted", "steps": […]}`), so the **same script plays on every
backend**: Apple's bridge, Android's bridge and the pure-Rust `ScriptedBackend`. Each model inference plays the next
step (protocol §10):

```rust
let script = Script::new()
    .step(ScriptStep::tool_call("check_inventory", json!({"item": "iron sword"})))
    .step(ScriptStep::npc_reply("Three swords, lad.", Emotion::Proud, ["Buy one"], false));
ai.create_npc("gorm", NpcConfig::new(persona).model(script));
```

Steps: `text` (streamed in about `chunks` pieces), `tool_call` / `tool_calls` (one round), `json` (NPC replies,
decisions, content, schema turns), `template` (`{prompt}`, `{toolOutput}`, `{toolOutputs}`), `error(code)` (e.g.
`GuardrailViolation` to test fallbacks), and `.delayed(ms)`.

`ScriptedBackend` implements the protocol core in Rust with no threads, so it runs on `wasm32`:

- Methods: `initialize`, `ping`, `model/availability`, `shutdown`, `session/*` (except `transcript` and `compact`),
  `npc/create`, `talk`, `bark`, `cancel`, `reset`, `delete` and `list`, `decision/decide` and `decideMany`,
  `content/generate`, and `world/*` with subscriptions. NPCs get the built-in `read_world_state`,
  `update_world_state`, `remember_fact` and `change_relationship` tools. Other methods answer `-32601`.
- These behave as on the real bridges: streaming, `tool/call` round trips with timeouts and `tool/cancel`,
  per-session and per-NPC ordering, cancellation that leaves no trace, NPC `automatic` / `structured` / `text` reply
  formats with the plain-text retry and fallback lines, decision fallbacks, and the error codes.
- The `"system"` model is unavailable (`model_unavailable`) unless configured:
  - `with_system_script(script)` gives each session or NPC its own copy of the script.
  - `with_system_model(|request| step)` decides every step from a `ModelRequest`: kind, prompt, tool outputs so far,
    enabled tools. Use it to ship keyword-driven NPCs on the web, through exactly the code path the on-device
    models use. The tavern's scripted Mira works this way.
- `with_pacing(duration)` streams pieces at a typewriter pace. Delays, pacing and tool timeouts advance in
  `AiBackend::poll`, once per frame.

## Apple (macOS, iOS, visionOS 27)

Requires OS 27 and, for the system model, Apple Intelligence. Scripted models work without it.

**macOS.** Build the dylib, then build with the feature:

```sh
OAM=/path/to/open-apple-models
swift build --package-path $OAM -c release --product OpenAppleModelsFFI
export OAM_APPLE_LIB_DIR=$OAM/.build/release          # folder with libOpenAppleModelsFFI.dylib
# then, in the Space3d checkout:
cargo run -p ai_tavern --features apple               # the example (space3d/ai-apple-ffi)
cargo test -p space3d-ai --features apple-ffi         # includes tests/apple_live.rs
```

- `build.rs` links `OpenAppleModelsFFI` from `OAM_APPLE_LIB_DIR` (`OAM_APPLE_LIB_KIND=static` for a `.a`). With the
  feature on but no variable, it builds anyway, prints a build warning with instructions, and `AppleBackend`
  reports `Unsupported`. `--all-features` builds therefore still work.
- The dylib's install name is `@rpath/libOpenAppleModelsFFI.dylib`. `space3d-ai`'s own tests get an rpath
  automatically. Your binary needs one too: add `println!("cargo::rustc-link-arg=-Wl,-rpath,{dir}")` in its
  `build.rs` (as `examples/ai_tavern/build.rs` does), or ship the dylib in `Contents/Frameworks` of the app bundle
  with `@executable_path/../Frameworks`.
- Without the feature, or without Apple, `AppleBackend` compiles everywhere and reports `Unsupported`.

**iOS and visionOS.** These need Xcode 27. Space3d builds the game as a Rust `staticlib` that the Xcode project
links (`docs/platforms.md`). Build it with `--features apple-ffi` (`ai-apple-ffi` on the facade) and **without**
`OAM_APPLE_LIB_DIR`: `build.rs` then only enables the backend, and the `oam_bridge_*` symbols are resolved when
Xcode links the app. Link open-apple-models in one of two ways:

1. **Static XCFramework**: one binary, nothing to embed.
   1. In open-apple-models, build the `OpenAppleModelsFFI` scheme as a static framework for device and simulator:

      ```sh
      for destination in "generic/platform=iOS" "generic/platform=iOS Simulator"; do
        xcodebuild build -scheme OpenAppleModelsFFI -destination "$destination" -configuration Release \
          -derivedDataPath .build/xcode MACH_O_TYPE=staticlib SKIP_INSTALL=NO
      done
      ```

      `MACH_O_TYPE=staticlib` turns `PackageFrameworks/OpenAppleModelsFFI.framework` into a static framework that
      contains the whole library (the bridge, the game layer and the core), not only the C ABI target.
   2. Package the slices:

      ```sh
      xcodebuild -create-xcframework \
        -framework .build/xcode/Build/Products/Release-iphoneos/PackageFrameworks/OpenAppleModelsFFI.framework \
        -framework .build/xcode/Build/Products/Release-iphonesimulator/PackageFrameworks/OpenAppleModelsFFI.framework \
        -output OpenAppleModelsFFI.xcframework
      ```

   3. Add the XCFramework to the app target's *Frameworks, Libraries, and Embedded Content* as **Do Not Embed**.
      Its objects auto-link `FoundationModels` and the Swift runtime, which ships with the OS. If your link setup
      turns auto-linking off, link `FoundationModels.framework` and add `$(SDKROOT)/usr/lib/swift` to the library
      search paths.
2. **Dynamic framework**: the `OpenAppleModelsFFI.xcframework` from open-apple-models' `bindings/README.md`, added
   as **Embed & Sign**.

Either way, set the app's minimum deployment target to **27.0**. The library links FoundationModels strongly, so
with a lower minimum the app crashes at launch on older systems.

What has been checked: steps 1 and 2 above, with Xcode 27. A Rust `staticlib` using `AppleBackend`, built with
`apple-ffi` and without `OAM_APPLE_LIB_DIR`, links with `clang` against the device and the simulator framework from
step 1. In the iOS 27 simulator it started the bridge and answered `initialize`. It has not yet been checked in an
Xcode app project, on a device, or with the on-device model on iOS. The dynamic framework and visionOS have not been
tried.

## Android

Gemini Nano runs on AICore devices (Pixel 9 and later, Galaxy S26, Z Flip8/Fold8, …) while the app is the top
foreground app. `AndroidBackend` reaches open-android-models' `OamJni` over JNI. The host app must be built with
Gradle: the open-android-models AAR and ML Kit bring code and manifest entries that `cargo apk` cannot merge. See
**[ai-android.md](ai-android.md)** for the Gradle host (a working template is `examples/ai_tavern/android`). Read
Google's [GenAI terms](https://developers.google.com/ml-kit/genai-terms) before shipping. Among other things, they
restrict services likely to be used by under-18s.

## Web

There is no on-device model in browsers. Use `BackendChoice::auto_or(ScriptedBackend::…)`. The scripted backend is
pure Rust and single-threaded, and its timers run on the frame's `poll`.

## Errors

`AiError { code, message, rpc_code, data }` mirrors the protocol's error table (§8). Branch on `code`:
`ModelUnavailable`, `GuardrailViolation`, `Refusal`, `ContextSizeExceeded`, `RateLimited` (see
`retry_after_seconds()`), `Cancelled`, `SessionNotFound`, `NpcNotFound`, `InvalidParams` and so on. There are also
three local codes:

- `Unsupported`: there is no backend.
- `Transport`: a send failed.
- `Protocol`: a malformed line.

Helpers: `is_unavailable()` and `is_blocked()` (a guardrail or refusal: show a fallback line). NPCs answer blocked
turns with fallback lines by default (`fallback_on_guardrail`). Decisions return their `fallback` option.

## Testing

```sh
cargo test -p space3d-ai                     # unit, protocol (scripted), engine-level and doc tests
cargo test -p ai_tavern                      # the tavern's demo conversation, headless
OAM_APPLE_LIB_DIR=… cargo test -p space3d-ai --features apple-ffi --test apple_live -- --nocapture
OAM_APPLE_LIVE_MODEL=1 OAM_APPLE_LIB_DIR=… cargo test -p space3d-ai --features apple-ffi --test apple_live
OAM_ANDROID_CLASSPATH=… cargo test -p space3d-ai --features android-jvm --test android_jvm   # real OamJni, desktop JVM
cargo check -p space3d-ai --target wasm32-unknown-unknown
cargo ndk -t arm64-v8a check -p space3d-ai   # NDK 30; needs the aarch64-linux-android rustup target
```

`tests/common` holds eight protocol scenarios ported from open-apple-models' bridge tests: core messages, client tool
round trips, tool errors and timeouts, cancellation, NPCs and fallbacks, decisions, content, and world state. They
run against `ScriptedBackend` always. They also run against Apple's real bridge (`apple_live`) and against
open-android-models' real `OamJni` and `BridgeEngine` in a desktop JVM (`android_jvm`,
[ai-android.md](ai-android.md#testing-without-a-device)). So one set of expectations covers all three backends.

## Limitations

- `ScriptedBackend` does not implement:
  - `session/transcript` or `session/compact`
  - `npc/state`, `npc/restore` or `npc/update`
  - `schema/validate` or `tools/validate`
  - `history` on `session/create`, which is accepted with a warning
- `ScriptedBackend` differs from the real bridges in details:
  - It validates only the structure of schemas.
  - Its token counts are estimates.
  - Its call ids are generated.
- `serde_json::Value` does not keep key order, so `content/generate`'s schema-order keys are unordered in Rust.
  Decode into structs.
- The Android backend has not run on a device: no Gemini Nano device or emulator was available. It has been
  compiled and packaged into an APK, and its JNI code has been tested against the real `OamJni` in a desktop JVM.
- iOS has only been checked in the simulator, outside an Xcode app project, with the static XCFramework
  (see [iOS and visionOS](#apple-macos-ios-visionos-27)). It has not run on a device.

[open-apple-models]: https://github.com/SpaceCorps/open-apple-models
[open-android-models]: https://github.com/SpaceCorps/open-android-models
