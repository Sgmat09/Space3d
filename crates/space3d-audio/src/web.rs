//! Web Audio output that starts at the page's first user gesture.
//!
//! Browsers keep an `AudioContext` *suspended* until the page has had a user gesture (the
//! autoplay policy), ignore `resume()` before one, and log a warning ("The AudioContext was not
//! allowed to start") for each context created and each `resume()` tried before it. kira's cpal
//! backend creates and resumes the context once, at start-up — before any gesture, so the game
//! would stay silent and every page load would log those warnings.
//!
//! [`WebAudioBackend`] is the same backend (cpal's Web Audio host, re-exported by kira) with a
//! different start: while the page has had no gesture it keeps kira's renderer and creates
//! nothing. Listeners on `window` for the input events that give a page user activation
//! (`keydown`, `mousedown`, `pointerdown`, `pointerup`, `touchend`) then create the stream and
//! start the context *inside* the first gesture's event handler — what every browser allows,
//! Safari's stricter policy included. The listeners also resume a context the browser
//! suspended later (a phone call, the tab in the background) at the next gesture, and
//! [`WebAudioBackend::poll`] (every frame, from `sync_audio`) resumes it once
//! `navigator.userActivation.hasBeenActive` says a gesture happened. Sounds played before
//! audio starts are queued: kira's renderer processes them once the output runs.

use std::cell::RefCell;
use std::rc::Rc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use kira::backend::cpal::cpal;
use kira::backend::cpal::{CpalBackendSettings, Error};
use kira::backend::{Backend, Renderer};
use send_wrapper::SendWrapper;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::AudioContextState;

/// Frames between resume attempts in browsers without `navigator.userActivation`.
const RETRY_FRAMES: u32 = 60;

/// Input events that give the page user activation (HTML's "activation-triggering input
/// events": `pointerup`/`touchend` for touch, `pointerdown`/`mousedown` for a mouse, `keydown`).
const GESTURE_EVENTS: [&str; 5] = ["keydown", "mousedown", "pointerdown", "pointerup", "touchend"];

enum Output {
    Empty,
    /// Set up, not started by kira yet.
    Configured {
        device: cpal::Device,
        config: cpal::StreamConfig,
    },
    /// Started by kira, waiting for the page's first user gesture: no `AudioContext` yet.
    Pending {
        device: cpal::Device,
        config: cpal::StreamConfig,
        renderer: Box<Renderer>,
    },
    Started(cpal::Stream),
}

impl Output {
    /// Create the stream (and its `AudioContext`) if pending and start it; resume a started
    /// stream's context the browser keeps suspended.
    fn start_or_resume(&mut self) -> Result<(), Error> {
        match std::mem::replace(self, Output::Empty) {
            Output::Pending {
                device,
                config,
                mut renderer,
            } => {
                let channels = config.channels;
                let stream = device.build_output_stream(
                    config,
                    move |data: &mut [f32], _| {
                        renderer.on_start_processing();
                        renderer.process(data, channels);
                    },
                    |e| tracing::warn!("audio output error: {e}"),
                    None,
                )?;
                stream.play()?;
                *self = Output::Started(stream);
            }
            Output::Started(stream) => {
                let suspended = context_state(&stream) == Some(AudioContextState::Suspended);
                let resumed = if suspended { stream.play() } else { Ok(()) };
                *self = Output::Started(stream);
                resumed?;
            }
            other => *self = other,
        }
        Ok(())
    }
}

/// The state of a stream's `AudioContext`.
fn context_state(stream: &cpal::Stream) -> Option<AudioContextState> {
    // The Web Audio host's stream (the only host without cpal's `custom`/atomics hosts).
    #[allow(irrefutable_let_patterns)]
    let cpal::platform::StreamInner::WebAudio(web_stream) = stream.as_inner() else {
        return None;
    };
    Some(web_stream.audio_context().state())
}

/// kira backend on cpal's Web Audio host that starts at the page's first user gesture (see
/// the module docs).
pub(crate) struct WebAudioBackend {
    // wasm32 without atomics is single-threaded; `SendWrapper` makes the JS handles `Send`
    // (as kira's own backend does) and checks that they stay on this thread.
    inner: SendWrapper<Inner>,
}

struct Inner {
    /// Shared with the gesture listener.
    output: Rc<RefCell<Output>>,
    /// The gesture listener while it is registered on `window`.
    listener: Option<Closure<dyn FnMut()>>,
    /// Frames until the next resume attempt (a requested resume completes asynchronously).
    wait: u32,
    waiting_logged: bool,
    running: bool,
}

impl Backend for WebAudioBackend {
    type Settings = CpalBackendSettings;
    type Error = Error;

    fn setup(settings: Self::Settings, _internal_buffer_size: usize) -> Result<(Self, u32), Self::Error> {
        let device = match settings.device {
            Some(device) => device,
            None => cpal::default_host()
                .default_output_device()
                .ok_or(Error::NoDefaultOutputDevice)?,
        };
        // No `AudioContext` needed: the Web Audio host picks a standard rate.
        let config = match settings.config {
            Some(config) => config,
            None => device.default_output_config()?.config(),
        };
        let sample_rate = config.sample_rate;
        Ok((
            Self {
                inner: SendWrapper::new(Inner {
                    output: Rc::new(RefCell::new(Output::Configured { device, config })),
                    listener: None,
                    wait: 0,
                    waiting_logged: false,
                    running: false,
                }),
            },
            sample_rate,
        ))
    }

    fn start(&mut self, renderer: Renderer) -> Result<(), Self::Error> {
        let inner = &mut *self.inner;
        {
            let mut output = inner.output.borrow_mut();
            let Output::Configured { device, config } = std::mem::replace(&mut *output, Output::Empty) else {
                panic!("the audio backend was started twice");
            };
            *output = Output::Pending {
                device,
                config,
                renderer: Box::new(renderer),
            };
            // Without a gesture yet: nothing now (`poll` says so once logging runs).
            if has_been_active() != Some(false) {
                // The page already had a gesture (or the browser cannot tell: the context may
                // start suspended, and the next gesture resumes it).
                output.start_or_resume()?;
            }
        }
        inner.listen_for_gestures();
        Ok(())
    }
}

/// `navigator.userActivation.hasBeenActive`: the page has had a user gesture. `None` where the
/// browser lacks the API (Chrome < 72, Firefox < 120, Safari < 16.4).
fn has_been_active() -> Option<bool> {
    let navigator = web_sys::window()?.navigator();
    let activation = js_sys::Reflect::get(&navigator, &"userActivation".into()).ok()?;
    if activation.is_undefined() {
        return None;
    }
    Some(navigator.user_activation().has_been_active())
}

impl Inner {
    /// Start or resume the output from inside each user gesture's event handler.
    fn listen_for_gestures(&mut self) {
        let Some(window) = web_sys::window() else {
            return;
        };
        let output = Rc::clone(&self.output);
        let listener = Closure::<dyn FnMut()>::new(move || {
            // A touch's `pointerdown` does not activate the page (its `pointerup` does): a
            // context created or resumed then would stay suspended and make the browser warn.
            if has_been_active() == Some(false) {
                return;
            }
            if let Ok(mut output) = output.try_borrow_mut()
                && let Err(e) = output.start_or_resume()
            {
                tracing::warn!("cannot start the audio output: {e}");
            }
        });
        for event in GESTURE_EVENTS {
            // Capture phase: before the canvas' own handlers, which may stop propagation.
            if let Err(e) =
                window.add_event_listener_with_callback_and_bool(event, listener.as_ref().unchecked_ref(), true)
            {
                tracing::warn!("cannot listen for {event} to start the audio output: {e:?}");
            }
        }
        self.listener = Some(listener);
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The listener holds the output: unregister it so the stream closes with the backend.
        if let (Some(listener), Some(window)) = (self.listener.take(), web_sys::window()) {
            for event in GESTURE_EVENTS {
                let _ =
                    window.remove_event_listener_with_callback_and_bool(event, listener.as_ref().unchecked_ref(), true);
            }
        }
    }
}

impl WebAudioBackend {
    /// Start the output once the page had a user gesture the listeners did not see, and resume
    /// a context the browser keeps suspended (in browsers without `navigator.userActivation`:
    /// about once a second). Called every frame; cheap (one property read) while audio runs.
    pub(crate) fn poll(&mut self) {
        let inner = &mut *self.inner;
        let Ok(mut output) = inner.output.try_borrow_mut() else {
            return;
        };
        let state = match &*output {
            Output::Pending { .. } => None,
            Output::Started(stream) => context_state(stream),
            Output::Empty | Output::Configured { .. } => return,
        };
        if state == Some(AudioContextState::Running) {
            if !inner.running {
                inner.running = true;
                tracing::info!("audio output running");
            }
            return;
        }
        if state.is_some() && state != Some(AudioContextState::Suspended) {
            return; // closed
        }
        // Pending, or suspended: before the first gesture, or again (a phone call, the tab in
        // the background).
        inner.running = false;
        if inner.wait > 0 {
            inner.wait -= 1;
            return;
        }
        let activated = has_been_active();
        if activated == Some(false) {
            if !std::mem::replace(&mut inner.waiting_logged, true) {
                tracing::info!("audio output starts at the page's first user gesture (browser autoplay policy)");
            }
            return;
        }
        if let Err(e) = output.start_or_resume() {
            tracing::warn!("cannot start the audio output: {e}");
        }
        // `resume()` completes asynchronously; without the activation API, retry rarely
        // (each refused attempt makes the browser log a warning).
        inner.wait = if activated.is_some() { 10 } else { RETRY_FRAMES };
    }
}
