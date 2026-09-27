//! Web Audio output that starts after the page's first user gesture.
//!
//! Browsers create an `AudioContext` *suspended* until the page has had a user gesture (the
//! autoplay policy) and ignore `resume()` before one. kira's cpal backend resumes the context
//! only once, when the stream starts — before any gesture, so the game would stay silent.
//! [`WebAudioBackend`] is the same backend (cpal's Web Audio host, re-exported by kira) that
//! keeps the stream: [`WebAudioBackend::poll`] (every frame, from `sync_audio`) resumes the
//! context once `navigator.userActivation.hasBeenActive` says the page had a gesture. Sounds
//! started before that are scheduled on the context's clock, which does not advance while it
//! is suspended: they play once audio starts.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use kira::backend::cpal::cpal;
use kira::backend::cpal::{CpalBackendSettings, Error};
use kira::backend::{Backend, Renderer};
use send_wrapper::SendWrapper;
use web_sys::AudioContextState;

/// Frames between resume attempts in browsers without `navigator.userActivation`.
const RETRY_FRAMES: u32 = 60;

enum State {
    Empty,
    Uninitialized {
        device: cpal::Device,
        config: cpal::StreamConfig,
    },
    Started(cpal::Stream),
}

/// kira backend on cpal's Web Audio host that resumes its `AudioContext` after a user gesture
/// (see the module docs).
pub(crate) struct WebAudioBackend {
    // wasm32 without atomics is single-threaded; `SendWrapper` makes the JS handles `Send`
    // (as kira's own backend does) and checks that they stay on this thread.
    state: SendWrapper<State>,
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
        let config = match settings.config {
            Some(config) => config,
            None => device.default_output_config()?.config(),
        };
        let sample_rate = config.sample_rate;
        Ok((
            Self {
                state: SendWrapper::new(State::Uninitialized { device, config }),
                wait: 0,
                waiting_logged: false,
                running: false,
            },
            sample_rate,
        ))
    }

    fn start(&mut self, mut renderer: Renderer) -> Result<(), Self::Error> {
        let State::Uninitialized { device, config } = std::mem::replace(&mut *self.state, State::Empty) else {
            panic!("the audio backend was started twice");
        };
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
        // Starts the context when the page already had a gesture; otherwise `poll` does later.
        stream.play()?;
        *self.state = State::Started(stream);
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

impl WebAudioBackend {
    /// Resume the `AudioContext` when the browser keeps it suspended and the page has had a
    /// user gesture (in browsers without `navigator.userActivation`: about once a second).
    /// Called every frame; cheap (one property read) while audio runs.
    pub(crate) fn poll(&mut self) {
        let State::Started(stream) = &*self.state else {
            return;
        };
        // The Web Audio host's stream (the only host without cpal's `custom`/atomics hosts).
        #[allow(irrefutable_let_patterns)]
        let cpal::platform::StreamInner::WebAudio(web_stream) = stream.as_inner() else {
            return;
        };
        let state = web_stream.audio_context().state();
        if state != AudioContextState::Suspended {
            if state == AudioContextState::Running && !self.running {
                self.running = true;
                tracing::info!("audio output running");
            }
            return;
        }
        // Suspended: before the first gesture, or again (a phone call, the tab in background).
        self.running = false;
        if self.wait > 0 {
            self.wait -= 1;
            return;
        }
        match has_been_active() {
            Some(false) => {
                if !std::mem::replace(&mut self.waiting_logged, true) {
                    tracing::info!(
                        "audio output suspended until the page's first user gesture (browser autoplay policy)"
                    );
                }
            }
            activated => {
                if let Err(e) = stream.play() {
                    tracing::warn!("cannot resume the audio output: {e}");
                }
                // `resume()` completes asynchronously; without the activation API, retry
                // rarely (each refused attempt makes the browser log a warning).
                self.wait = if activated.is_some() { 10 } else { RETRY_FRAMES };
            }
        }
    }
}
