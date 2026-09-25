//! Opt-in: `AndroidBackend`'s JNI code against open-android-models' **real** `OamJni` and
//! Kotlin `BridgeEngine`, in a desktop JVM started in-process (no Android device needed).
//!
//! This exercises everything the Android backend does on a device except ML Kit: class lookup
//! through the context's class loader, `RegisterNatives` for `nativeDeliver`, `OamJni.create /
//! send / destroy`, string conversion both ways, and delivery on the JVM's threads. The JVM
//! runs with `-Xcheck:jni`. `android.content.Context` and `android.util.Log` are small stubs
//! (`tests/jvm`) compiled with `javac`.
//!
//! ```sh
//! export JAVA_HOME=/path/to/jdk-21          # javac + libjvm
//! (cd open-android-models && ./gradlew -q --no-configuration-cache \
//!     -I $SPACE3D/crates/space3d-ai/tests/jvm/print-classpath.init.gradle.kts \
//!     :oam-bridge:jar :oam-mlkit:bundleLibRuntimeToDirDebug :oam-bridge:printOamJniClasspath) > /tmp/oam-cp
//! OAM_ANDROID_CLASSPATH=$(tail -1 /tmp/oam-cp) \
//!     cargo test -p space3d-ai --features android-jvm --test android_jvm -- --nocapture
//! ```

#![cfg(feature = "android-jvm")]

mod common;

use common::{Harness, run_all, run_until};
use jni::objects::JObject;
use jni::refs::Global;
use jni::vm::{InitArgsBuilder, JavaVM};
use jni::{JNIVersion, jni_sig, jni_str};
use serde_json::json;
use space3d_ai::prelude::*;
use space3d_ai::{AiBackend, AiStatus, AndroidBackend, BackendKind};
use space3d_core::prelude::*;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

struct Jvm {
    vm: JavaVM,
    context: Global<JObject<'static>>,
}

/// Compiles the stubs and starts the JVM once per process; `None` when not configured.
fn jvm() -> Option<&'static Jvm> {
    static JVM: OnceLock<Option<Jvm>> = OnceLock::new();
    JVM.get_or_init(|| {
        let Ok(classpath) = std::env::var("OAM_ANDROID_CLASSPATH") else {
            eprintln!("skipped: set OAM_ANDROID_CLASSPATH (see the header of tests/android_jvm.rs)");
            return None;
        };
        let stubs = compile_stubs();
        let separator = if cfg!(windows) { ";" } else { ":" };
        let args = InitArgsBuilder::new()
            .version(JNIVersion::V21)
            .option(format!("-Djava.class.path={}{separator}{classpath}", stubs.display()))
            .option("-Xcheck:jni")
            .build()
            .expect("JVM arguments");
        let vm = JavaVM::new(args).expect("start the JVM (JAVA_HOME must point to a JDK)");
        let context = vm
            .attach_current_thread(|env| -> jni::errors::Result<_> {
                let class = env.load_class(jni_str!("android.content.Context"))?;
                let context = env.new_object(class, jni_sig!("()V"), &[])?;
                env.new_global_ref(context)
            })
            .expect("create the stub Context");
        Some(Jvm { vm, context })
    })
    .as_ref()
}

fn compile_stubs() -> PathBuf {
    let sources = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/jvm");
    let out = std::env::temp_dir().join(format!("space3d-ai-jvm-stubs-{}", std::process::id()));
    let javac = std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into());
    let status = Command::new(javac)
        .arg("-d")
        .arg(&out)
        .arg(sources.join("android/content/Context.java"))
        .arg(sources.join("android/util/Log.java"))
        .arg(sources.join("space3d/test/BootOnlyContext.java"))
        .status()
        .expect("run javac");
    assert!(status.success(), "javac failed");
    out
}

fn backend(jvm: &Jvm) -> AndroidBackend {
    // SAFETY: the VM and the global Context reference live for the whole test process.
    unsafe { AndroidBackend::from_raw(jvm.vm.get_raw().cast(), jvm.context.as_raw().cast()) }.expect("AndroidBackend")
}

#[test]
fn oam_jni_passes_the_shared_protocol_scenarios() {
    let Some(jvm) = jvm() else { return };
    run_all(|| Box::new(backend(jvm)));
}

#[test]
fn oam_jni_identifies_itself_and_reports_the_missing_model() {
    let Some(jvm) = jvm() else { return };
    let mut h = Harness::new(Box::new(backend(jvm)));
    assert_eq!(h.backend.kind(), BackendKind::Android);
    let init = h.result("initialize", json!({"protocolVersion": "1.0"}));
    assert_eq!(init["server"]["name"], "open-android-models");
    eprintln!("model on a desktop JVM (no ML Kit): {}", init["model"]);
    assert_eq!(init["model"]["available"], false);
    h.result("session/create", json!({"session": "nano"}));
    let turn = h.call("session/respond", json!({"session": "nano", "prompt": "Hi"}));
    assert_eq!(common::error_name(&turn), Some("model_unavailable"));
}

#[test]
fn non_ascii_text_crosses_jni_both_ways() {
    let Some(jvm) = jvm() else { return };
    let mut h = Harness::new(Box::new(backend(jvm)));
    let text = "Grüße, 旅人! Dragons 🐉 ahead — ¿listo? 👍🏽";
    h.result(
        "session/create",
        json!({"session": "s", "model": common::scripted(json!([{"template": "{prompt} / {prompt}"}]))}),
    );
    let result = h.result(
        "session/respond",
        json!({"session": "s", "prompt": text, "stream": true}),
    );
    assert_eq!(result["text"], format!("{text} / {text}"));
    // The same text round-trips through a tool call's arguments and output.
    h.respond_to_tools(|params| Some(json!({"output": params["call"]["arguments"]["say"]})));
    h.result("session/create", json!({"session": "t", "tools": [{"name": "echo", "description": "Echo.",
            "parameters": {"type": "object", "properties": {"say": {"type": "string"}}, "required": ["say"]}}],
        "model": common::scripted(json!([{"toolCalls": [{"name": "echo", "arguments": {"say": text}}]}, {"template": "{toolOutput}"}]))}));
    assert_eq!(
        h.result("session/respond", json!({"session": "t", "prompt": "?"}))["text"],
        text
    );
}

#[test]
fn engine_tools_run_over_jni() {
    let Some(jvm) = jvm() else { return };
    struct Gold(u32);
    let mut app = App::new();
    app.insert_resource(Gold(60));
    app.add_plugin(
        AiPlugin::new(BackendChoice::backend(backend(jvm))).with_tool(
            AiTool::new("pay", "Take gold from the player.")
                .parameters(
                    json!({"type": "object", "properties": {"amount": {"type": "integer"}}, "required": ["amount"]}),
                )
                .handler(|ctx, call| {
                    let amount = call.i64("amount").unwrap_or(0) as u32;
                    let gold = ctx.resources.expect_mut::<Gold>();
                    gold.0 = gold.0.saturating_sub(amount);
                    json!({"paid": amount, "left": gold.0})
                }),
        ),
    );
    let script = Script::new()
        .step(ScriptStep::tool_call("pay", json!({"amount": 45})))
        .step(ScriptStep::npc_reply(
            "Pleasure doing business.",
            Emotion::Happy,
            ["Bye."],
            true,
        ));
    let ai = app.resources.expect_mut::<Ai>();
    let mut created = ai.create_npc(
        "gorm",
        NpcConfig::new(Persona::new("Gorm")).grounding_tool("pay").model(script),
    );
    let mut turn = ai.talk("gorm", "Here you go.");
    assert_eq!(run_until(&mut app, &mut created).expect("npc/create").tools, ["pay"]);
    let turn = run_until(&mut app, &mut turn).expect("npc/talk");
    assert_eq!(turn.line, "Pleasure doing business.");
    assert_eq!(app.resources.expect::<Gold>().0, 15);
    assert_eq!(
        turn.tool_calls[0].output,
        ToolOutput::Json(json!({"paid": 45, "left": 15}))
    );
    assert!(
        matches!(app.resources.expect::<Ai>().status(), AiStatus::Ready(info) if info.server.name == "open-android-models")
    );
}

#[test]
fn a_missing_aar_is_reported_not_fatal() {
    let Some(jvm) = jvm() else { return };
    // A context whose class loader cannot see OamJni, as in an app built without the AAR.
    let foreign = jvm
        .vm
        .attach_current_thread(|env| -> jni::errors::Result<_> {
            let class = env.load_class(jni_str!("space3d.test.BootOnlyContext"))?;
            let object = env.new_object(class, jni_sig!("()V"), &[])?;
            env.new_global_ref(object)
        })
        .unwrap();
    // SAFETY: as in `backend`.
    let backend = unsafe { AndroidBackend::from_raw(jvm.vm.get_raw().cast(), foreign.as_raw().cast()) }.unwrap();
    let error = backend.start(space3d_ai::Inbox::new()).unwrap_err();
    eprintln!("{error}");
    assert!(error.message.contains("OamJni"), "{error}");
    let mut app = App::new();
    // SAFETY: as in `backend`.
    let backend = unsafe { AndroidBackend::from_raw(jvm.vm.get_raw().cast(), foreign.as_raw().cast()) }.unwrap();
    app.add_plugin(AiPlugin::new(BackendChoice::backend(backend)));
    assert_eq!(app.resources.expect::<Ai>().backend_kind(), BackendKind::Unsupported);
}
