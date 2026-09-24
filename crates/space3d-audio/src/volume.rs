//! Backend-independent volume math: linear ⇄ dB, mixer buses and fades.
use kira::{Decibels, Easing, Tween};
use std::time::Duration;

/// Level (dB) at and below which kira treats audio as silent.
pub const SILENCE_DB: f32 = -60.0;

/// Linear amplitude (1.0 = unchanged) → decibels, clamped to [`SILENCE_DB`].
/// Zero, negative and NaN amplitudes map to silence.
pub fn amplitude_to_db(amplitude: f32) -> f32 {
    if amplitude.is_nan() || amplitude <= 0.0 {
        return SILENCE_DB;
    }
    (20.0 * amplitude.log10()).max(SILENCE_DB)
}

/// Decibels → linear amplitude (anything at or below [`SILENCE_DB`] is `0.0`).
pub fn db_to_amplitude(db: f32) -> f32 {
    Decibels(db).as_amplitude()
}

/// Mixer bus. `Music` and `Sfx` are sub-tracks of `Master`; spatial emitters route to `Sfx`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Bus {
    /// The main output track.
    Master,
    /// Background music.
    Music,
    /// Sound effects (default for one-shots and all spatial emitters).
    #[default]
    Sfx,
}

/// Linear volume per bus plus a global mute. Tracked even when audio is disabled so
/// settings UIs keep working.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixerLevels {
    /// Master volume (linear).
    pub master: f32,
    /// Music bus volume (linear, before master).
    pub music: f32,
    /// Sfx bus volume (linear, before master).
    pub sfx: f32,
    /// Global mute.
    pub muted: bool,
}

impl Default for MixerLevels {
    fn default() -> Self {
        Self {
            master: 1.0,
            music: 1.0,
            sfx: 1.0,
            muted: false,
        }
    }
}

fn sanitize(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.max(0.0) }
}

impl MixerLevels {
    /// Volume of one bus on its own (not multiplied by master).
    pub fn get(&self, bus: Bus) -> f32 {
        match bus {
            Bus::Master => self.master,
            Bus::Music => self.music,
            Bus::Sfx => self.sfx,
        }
    }

    /// Set one bus volume; negative/NaN values clamp to 0.
    pub fn set(&mut self, bus: Bus, volume: f32) {
        let v = sanitize(volume);
        match bus {
            Bus::Master => self.master = v,
            Bus::Music => self.music = v,
            Bus::Sfx => self.sfx = v,
        }
    }

    /// Audible gain of a bus: its level times master, or 0 when muted.
    pub fn effective(&self, bus: Bus) -> f32 {
        if self.muted {
            return 0.0;
        }
        match bus {
            Bus::Master => self.master,
            Bus::Music => self.master * self.music,
            Bus::Sfx => self.master * self.sfx,
        }
    }

    /// Audible gain of a sound with its own `volume` playing on `bus`.
    pub fn effective_sound(&self, bus: Bus, volume: f32) -> f32 {
        self.effective(bus) * sanitize(volume)
    }

    /// Master level as kira decibels (silence when muted).
    pub(crate) fn master_db(&self) -> f32 {
        if self.muted {
            SILENCE_DB
        } else {
            amplitude_to_db(self.master)
        }
    }
}

/// Smooth transition used for volume changes, fades, pauses and stops.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fade {
    /// Length of the transition.
    pub duration: Duration,
    /// Curve of the transition.
    pub easing: Easing,
}

impl Fade {
    /// Practically instant (10 ms, kira's click-free default).
    pub const IMMEDIATE: Self = Self {
        duration: Duration::from_millis(10),
        easing: Easing::Linear,
    };

    /// Linear fade over `duration`.
    pub const fn new(duration: Duration) -> Self {
        Self {
            duration,
            easing: Easing::Linear,
        }
    }

    /// Linear fade over `secs` seconds.
    pub fn secs(secs: f32) -> Self {
        Self::new(Duration::from_secs_f32(secs.max(0.0)))
    }

    /// Replace the easing curve.
    pub const fn with_easing(mut self, easing: Easing) -> Self {
        self.easing = easing;
        self
    }

    pub(crate) fn tween(self) -> Tween {
        Tween {
            duration: self.duration.max(Duration::from_millis(1)),
            easing: self.easing,
            ..Default::default()
        }
    }
}

impl Default for Fade {
    fn default() -> Self {
        Self::IMMEDIATE
    }
}

impl From<Duration> for Fade {
    fn from(d: Duration) -> Self {
        Self::new(d)
    }
}

/// Parameters for playing a sound.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayParams {
    /// Bus to play on (ignored for emitter sounds, which use their spatial track under `Sfx`).
    pub bus: Bus,
    /// Linear volume of this sound.
    pub volume: f32,
    /// Stereo pan, -1 (left) ..= 1 (right).
    pub pan: f32,
    /// Playback rate (1 = normal; 2 = double speed, one octave up).
    pub pitch: f64,
    /// Loop the whole sound until stopped.
    pub looping: bool,
    /// Fade in from silence.
    pub fade_in: Option<Duration>,
}

impl Default for PlayParams {
    fn default() -> Self {
        Self {
            bus: Bus::Sfx,
            volume: 1.0,
            pan: 0.0,
            pitch: 1.0,
            looping: false,
            fade_in: None,
        }
    }
}

impl PlayParams {
    /// One-shot on the sfx bus.
    pub fn sfx() -> Self {
        Self::default()
    }

    /// Looping on the music bus.
    pub fn music() -> Self {
        Self {
            bus: Bus::Music,
            looping: true,
            ..Self::default()
        }
    }

    /// Choose the bus.
    pub fn on(mut self, bus: Bus) -> Self {
        self.bus = bus;
        self
    }

    /// Linear volume.
    pub fn volume(mut self, volume: f32) -> Self {
        self.volume = volume;
        self
    }

    /// Stereo pan, -1 ..= 1.
    pub fn pan(mut self, pan: f32) -> Self {
        self.pan = pan;
        self
    }

    /// Playback rate.
    pub fn pitch(mut self, pitch: f64) -> Self {
        self.pitch = pitch;
        self
    }

    /// Loop until stopped.
    pub fn looped(mut self) -> Self {
        self.looping = true;
        self
    }

    /// Fade in over `d`.
    pub fn fade_in(mut self, d: Duration) -> Self {
        self.fade_in = Some(d);
        self
    }

    /// Apply these parameters to (a cheap clone of) `data`.
    pub(crate) fn apply(
        &self,
        data: &kira::sound::static_sound::StaticSoundData,
    ) -> kira::sound::static_sound::StaticSoundData {
        let mut d = data
            .volume(amplitude_to_db(self.volume))
            .panning(if self.pan.is_nan() {
                0.0
            } else {
                self.pan.clamp(-1.0, 1.0)
            })
            .playback_rate(self.pitch);
        if self.looping {
            d = d.loop_region(..);
        }
        if let Some(f) = self.fade_in {
            d = d.fade_in_tween(Fade::new(f).tween());
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_conversions() {
        assert_eq!(amplitude_to_db(1.0), 0.0);
        assert!((amplitude_to_db(0.5) + 6.0206).abs() < 1e-3);
        assert!((amplitude_to_db(2.0) - 6.0206).abs() < 1e-3);
        assert_eq!(amplitude_to_db(0.0), SILENCE_DB);
        assert_eq!(amplitude_to_db(-1.0), SILENCE_DB);
        assert_eq!(amplitude_to_db(f32::NAN), SILENCE_DB);
        assert_eq!(amplitude_to_db(1e-9), SILENCE_DB);
        assert_eq!(db_to_amplitude(SILENCE_DB), 0.0);
        assert_eq!(db_to_amplitude(0.0), 1.0);
        for a in [0.01f32, 0.1, 0.25, 0.5, 0.9, 1.0, 1.5, 4.0] {
            assert!(
                (db_to_amplitude(amplitude_to_db(a)) - a).abs() < 1e-4 * a.max(1.0),
                "{a}"
            );
        }
    }

    #[test]
    fn bus_levels_multiply_and_mute() {
        let mut m = MixerLevels::default();
        m.set(Bus::Master, 0.5);
        m.set(Bus::Sfx, 0.5);
        m.set(Bus::Music, -3.0);
        assert_eq!(m.get(Bus::Music), 0.0, "negative volumes clamp to 0");
        assert_eq!(m.effective(Bus::Sfx), 0.25);
        assert_eq!(m.effective(Bus::Master), 0.5);
        assert_eq!(m.effective_sound(Bus::Sfx, 0.5), 0.125);
        m.muted = true;
        assert_eq!(m.effective(Bus::Sfx), 0.0);
        assert_eq!(m.master_db(), SILENCE_DB);
        m.muted = false;
        m.set(Bus::Master, f32::NAN);
        assert_eq!(m.effective(Bus::Master), 0.0);
    }

    #[test]
    fn fades_and_params() {
        assert_eq!(
            Fade::from(Duration::from_secs(2)).tween().duration,
            Duration::from_secs(2)
        );
        assert_eq!(
            Fade::new(Duration::ZERO).tween().duration,
            Duration::from_millis(1),
            "never zero-length"
        );
        let p = PlayParams::music().volume(0.5).pan(-0.3).pitch(1.5);
        assert!(p.looping && p.bus == Bus::Music);
        assert_eq!(PlayParams::sfx().bus, Bus::Sfx);
    }
}
