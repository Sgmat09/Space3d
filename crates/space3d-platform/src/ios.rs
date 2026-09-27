//! iOS: the UIScene life cycle for winit 0.30's UIKit backend.
//!
//! Apps linked against the iOS 27 SDK must adopt the UIScene life cycle: UIKit stops them
//! with `EXC_BREAKPOINT` in `_UIApplicationEvaluateRuntimeIssueForNoSceneLifecycleAdoption`
//! when their first scene is created otherwise. winit 0.30 predates that: it calls
//! `UIApplicationMain` without an app delegate and creates its `UIWindow` with
//! `initWithFrame:`, which belongs to no scene (and a scene-based app shows only windows that
//! do). So:
//!
//! * the app's `Info.plist` declares a `UIApplicationSceneManifest` whose scene configuration
//!   names [`crate::IOS_SCENE_DELEGATE_CLASS`] (`space3d build --target ios` and the `space3d new`
//!   Xcode project write it);
//! * [`register_scene_delegate`] defines that Objective-C class before `UIApplicationMain`;
//! * [`attach_to_scene`] puts the game's window into the connected window scene — right away
//!   when the scene is already connected (the usual order: winit creates the window on
//!   `applicationDidBecomeActive`), else when the scene connects. UIKit may also disconnect a
//!   background scene to reclaim memory while the process lives on; when a new scene connects
//!   later, the game's windows move into it (the window keeps its view, and so the renderer's
//!   surface layer).
//!
//! UIKit still posts the application-level notifications winit listens to (did become active,
//! will resign active, did enter background, …) in a scene-based app, so `resumed` /
//! `suspended` work as before.

use objc2::rc::{Retained, Weak};
use objc2::runtime::NSObjectProtocol;
use objc2::{ClassType, MainThreadMarker, MainThreadOnly, define_class};
use objc2_foundation::NSObject;
use objc2_ui_kit::{
    UIApplication, UIResponder, UIScene, UISceneConnectionOptions, UISceneDelegate, UISceneSession, UIView, UIWindow,
    UIWindowScene, UIWindowSceneDelegate,
};
use std::cell::RefCell;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

thread_local! {
    /// The game's windows (winit owns them), to show in each window scene that connects.
    static WINDOWS: RefCell<Vec<Weak<UIWindow>>> = const { RefCell::new(Vec::new()) };
}

define_class!(
    // SAFETY: UIResponder has no subclassing requirements; no `Drop` impl.
    #[unsafe(super(UIResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "Space3dSceneDelegate"]
    struct SceneDelegate;

    unsafe impl NSObjectProtocol for SceneDelegate {}

    unsafe impl UISceneDelegate for SceneDelegate {
        #[unsafe(method(scene:willConnectToSession:options:))]
        fn scene_will_connect(&self, scene: &UIScene, _session: &UISceneSession, _options: &UISceneConnectionOptions) {
            let Some(scene) = scene.downcast_ref::<UIWindowScene>() else {
                return;
            };
            tracing::debug!("iOS window scene connected");
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            // Windows created before any scene, or left behind by a scene UIKit disconnected.
            for window in game_windows() {
                if !in_connected_scene(&window, mtm) {
                    show_in_scene(&window, scene);
                }
            }
        }

        #[unsafe(method(sceneDidDisconnect:))]
        fn scene_did_disconnect(&self, _scene: &UIScene) {
            tracing::debug!("iOS window scene disconnected; the game window moves to the next one that connects");
        }
    }

    unsafe impl UIWindowSceneDelegate for SceneDelegate {}
);

/// Define the [`crate::IOS_SCENE_DELEGATE_CLASS`] class with the Objective-C runtime. Call on the main
/// thread before winit runs `UIApplicationMain` (idempotent).
pub(crate) fn register_scene_delegate() {
    let class = SceneDelegate::class();
    debug_assert_eq!(class.name().to_str(), Ok(crate::IOS_SCENE_DELEGATE_CLASS));
}

/// Show winit's window in the app's window scene (now, or once the scene connects).
pub(crate) fn attach_to_scene(window: &winit::window::Window) {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::error!("iOS windows must be created on the main thread");
        return;
    };
    let Some(ui_window) = ui_window(window) else {
        tracing::error!("no UIWindow behind the winit window; it will not be shown");
        return;
    };
    WINDOWS.with_borrow_mut(|w| {
        w.retain(|w| w.load().is_some());
        w.push(Weak::from_retained(&ui_window));
    });
    if ui_window.windowScene().is_some() {
        return;
    }
    match connected_window_scene(mtm) {
        Some(scene) => show_in_scene(&ui_window, &scene),
        None => tracing::debug!("no window scene yet; the window is shown when it connects"),
    }
}

/// The game's live windows.
fn game_windows() -> Vec<Retained<UIWindow>> {
    WINDOWS.with_borrow_mut(|w| {
        w.retain(|w| w.load().is_some());
        w.iter().filter_map(Weak::load).collect()
    })
}

/// Whether `window` is in a window scene that is still connected.
fn in_connected_scene(window: &UIWindow, mtm: MainThreadMarker) -> bool {
    let Some(scene) = window.windowScene() else {
        return false;
    };
    let scene: &UIScene = &scene;
    UIApplication::sharedApplication(mtm)
        .connectedScenes()
        .iter()
        .any(|s| std::ptr::eq(&*s, scene))
}

fn ui_window(window: &winit::window::Window) -> Option<Retained<UIWindow>> {
    let RawWindowHandle::UiKit(handle) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    // SAFETY: winit's UIKit handle is the `UIView` of a live window, used on the main thread.
    let view: &UIView = unsafe { handle.ui_view.cast::<UIView>().as_ref() };
    view.window()
}

fn connected_window_scene(mtm: MainThreadMarker) -> Option<Retained<UIWindowScene>> {
    let app = UIApplication::sharedApplication(mtm);
    let scenes = app.connectedScenes();
    scenes.iter().find_map(|scene| scene.downcast::<UIWindowScene>().ok())
}

fn show_in_scene(window: &UIWindow, scene: &UIWindowScene) {
    let before = window.frame();
    window.setWindowScene(Some(scene));
    window.makeKeyAndVisible();
    let after = window.frame();
    tracing::debug!(
        "game window attached to the iOS window scene: frame {:?} -> {:?}",
        (before.size.width, before.size.height, before.origin.y),
        (after.size.width, after.size.height, after.origin.y)
    );
}
