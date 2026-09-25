//! [`ReplicationRegistry`]: which components, which input type and which messages travel over
//! the network, how they are encoded, and how clients treat them.
//!
//! The registry is built identically on server and client by the game's shared protocol
//! plugin, frozen at the first `Startup` (registering later panics), and hashed into the
//! schema hash carried by the handshake. [`ComponentKind`]s are assigned by **sorted name**
//! at freeze time, so plugin registration order never matters. At most [`MAX_COMPONENTS`]
//! components (one `u64` presence mask per entity on the wire).
//!
//! `space3d_scene::ComponentRegistry` is deliberately *not* used on the wire (it goes through
//! `serde_json::Value`); it is used for room checkpoints (see the server crate).

use crate::input::NetInput;
use crate::mapping::{DecodeCtx, EncodeCtx};
use crate::messages::NetMessage;
use crate::transport::Lane;
use crate::wire::{DecodeError, Reader, Writer};
use crate::{PlayerId, Tick};
use serde::Serialize;
use serde::de::DeserializeOwned;
use space3d_core::glam::{Quat, Vec3};
use space3d_core::hecs::Component;
use space3d_core::{Entity, Transform, World};
use std::any::TypeId;

/// Maximum number of replicated component types (bits of the presence mask).
pub const MAX_COMPONENTS: usize = 64;

/// Index of a registered component (assigned by sorted name at freeze; wire-visible).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComponentKind(pub u16);

/// Binary codec for a component type. Implementations must be deterministic: encoding the
/// same value always yields the same bytes (change detection compares encoded bytes).
///
/// Component payloads are length-prefixed by the snapshot format, so `decode` receives a
/// [`Reader`] scoped to exactly this component's bytes and may consume all of it.
pub trait NetCodec<T>: Send + Sync + 'static {
    /// Append `value`. Entity references go through `ctx` (`Entity` → `NetId`).
    fn encode(&self, value: &T, w: &mut Writer, ctx: &EncodeCtx<'_>);
    /// Read a value. Unknown entity references → [`DecodeError::Unresolved`].
    fn decode(&self, r: &mut Reader<'_>, ctx: &mut DecodeCtx<'_>) -> Result<T, DecodeError>;
    /// Stable description folded into the schema hash (codec kind + parameters + version).
    fn schema_id(&self) -> String;
}

/// Postcard (serde) codec: works for any `Serialize + DeserializeOwned` type.
#[derive(Clone, Copy, Debug, Default)]
pub struct PostcardCodec;

impl<T: Serialize + DeserializeOwned + Send + Sync + 'static> NetCodec<T> for PostcardCodec {
    fn encode(&self, value: &T, w: &mut Writer, _ctx: &EncodeCtx<'_>) {
        // Serialization of plain data types does not fail; a failure is a programming error.
        let bytes = postcard::to_stdvec(value).expect("postcard encode");
        w.raw(&bytes);
    }

    fn decode(&self, r: &mut Reader<'_>, _ctx: &mut DecodeCtx<'_>) -> Result<T, DecodeError> {
        let rest = r.raw(r.remaining())?;
        crate::wire::postcard_exact(rest)
    }

    fn schema_id(&self) -> String {
        "postcard".into()
    }
}

/// Codec for components that hold entity references: convert to a wire form `R` (with
/// `NetId`s via [`EncodeCtx::net_id`]) and back (via [`DecodeCtx::entity`]). Mirrors
/// `space3d_scene::ComponentRegistry::register_mapped`.
pub struct MappedCodec<T, R> {
    /// Component → wire form.
    pub to_wire: fn(&T, &EncodeCtx<'_>) -> R,
    /// Wire form → component; `None` while a referenced entity is unknown (retried later).
    pub from_wire: fn(R, &mut DecodeCtx<'_>) -> Option<T>,
}

impl<T, R> NetCodec<T> for MappedCodec<T, R>
where
    T: Send + Sync + 'static,
    R: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    fn encode(&self, value: &T, w: &mut Writer, ctx: &EncodeCtx<'_>) {
        let wire = (self.to_wire)(value, ctx);
        let bytes = postcard::to_stdvec(&wire).expect("postcard encode");
        w.raw(&bytes);
    }

    fn decode(&self, r: &mut Reader<'_>, ctx: &mut DecodeCtx<'_>) -> Result<T, DecodeError> {
        let rest = r.raw(r.remaining())?;
        let wire: R = crate::wire::postcard_exact(rest)?;
        (self.from_wire)(wire, ctx).ok_or(DecodeError::Unresolved)
    }

    fn schema_id(&self) -> String {
        format!("mapped:{}", short_type_name::<R>())
    }
}

/// How the client applies a replicated component on **remote** (and, for the `Predict*`
/// modes, locally controlled) entities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientMode {
    /// Write as soon as the snapshot arrives.
    Apply,
    /// Write when the client's render clock reaches the snapshot tick (stays in sync with
    /// interpolated poses, e.g. health bars, animation state).
    Delayed,
    /// Buffer and interpolate at the render clock ([`Interpolate`] required).
    Interpolate,
    /// Predicted on the controlling client (rollback + replay on mismatch), `Apply` elsewhere.
    Predict,
    /// Predicted on the controlling client, interpolated on everyone else (typical `Transform`).
    PredictAndInterpolate,
}

/// Who receives a component.
#[derive(Clone, Copy, Debug)]
pub enum Audience {
    /// Everyone who sees the entity.
    All,
    /// Only the owner (`Owner`/`ControlledBy` player), e.g. ammo, inventory.
    Owner,
    /// Everyone except the owner (e.g. state the owner predicts differently).
    ExceptOwner,
    /// Only players on the entity's `NetTeam`.
    Team,
    /// Game rule: `(world, entity, viewer) -> visible`.
    Custom(fn(&World, Entity, PlayerId) -> bool),
}

/// When a changed component is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendRule {
    /// Whenever its encoded bytes change (default).
    OnChange,
    /// Every send tick while the entity is in scope (rarely needed).
    Always,
    /// At most every `n` ticks (cheap for slow-changing state).
    Interval(u16),
}

/// Values that can be interpolated between two snapshots.
pub trait Interpolate: Clone + Send + Sync + 'static {
    /// Value at fraction `t` (0..=1) between `a` and `b`.
    fn interpolate(a: &Self, b: &Self, t: f32) -> Self;
    /// Value `t` intervals past `last` (continuing from `prev`), if extrapolation makes sense.
    fn extrapolate(prev: &Self, last: &Self, t: f32) -> Option<Self> {
        let _ = (prev, last, t);
        None
    }
}

impl Interpolate for f32 {
    fn interpolate(a: &Self, b: &Self, t: f32) -> Self {
        a + (b - a) * t
    }
    fn extrapolate(prev: &Self, last: &Self, t: f32) -> Option<Self> {
        Some(last + (last - prev) * t)
    }
}

impl Interpolate for Vec3 {
    fn interpolate(a: &Self, b: &Self, t: f32) -> Self {
        a.lerp(*b, t)
    }
    fn extrapolate(prev: &Self, last: &Self, t: f32) -> Option<Self> {
        Some(*last + (*last - *prev) * t)
    }
}

impl Interpolate for Quat {
    fn interpolate(a: &Self, b: &Self, t: f32) -> Self {
        a.slerp(*b, t)
    }
}

impl Interpolate for Transform {
    fn interpolate(a: &Self, b: &Self, t: f32) -> Self {
        Transform {
            translation: a.translation.lerp(b.translation, t),
            rotation: a.rotation.slerp(b.rotation, t),
            scale: a.scale.lerp(b.scale, t),
        }
    }
    /// Continues the translation linearly; rotation and scale hold.
    fn extrapolate(prev: &Self, last: &Self, t: f32) -> Option<Self> {
        Some(Transform {
            translation: last.translation + (last.translation - prev.translation) * t,
            ..*last
        })
    }
}

/// Extrapolation function (`prev`, `last`, intervals past `last`).
pub type ExtrapolateFn<T> = fn(&T, &T, f32) -> Option<T>;

/// Per-component replication rule (builder).
pub struct ComponentRule<T> {
    pub(crate) codec: Box<dyn NetCodec<T>>,
    pub(crate) mode: ClientMode,
    pub(crate) interpolate: Option<fn(&T, &T, f32) -> T>,
    pub(crate) extrapolate: Option<ExtrapolateFn<T>>,
    pub(crate) audience: Audience,
    pub(crate) send: SendRule,
    pub(crate) priority: f32,
    pub(crate) quantize_authoritative: bool,
    pub(crate) rollback_when: Option<fn(&T, &T) -> bool>,
}

impl<T: Component + Clone> ComponentRule<T> {
    /// Rule with an explicit codec; defaults: [`ClientMode::Apply`], [`Audience::All`],
    /// [`SendRule::OnChange`], priority 1, no authoritative quantization.
    pub fn new(codec: impl NetCodec<T>) -> Self {
        Self {
            codec: Box::new(codec),
            mode: ClientMode::Apply,
            interpolate: None,
            extrapolate: None,
            audience: Audience::All,
            send: SendRule::OnChange,
            priority: 1.0,
            quantize_authoritative: false,
            rollback_when: None,
        }
    }

    /// Postcard codec.
    pub fn serde() -> Self
    where
        T: Serialize + DeserializeOwned,
    {
        Self::new(PostcardCodec)
    }

    /// Entity-reference mapping codec (see [`MappedCodec`]).
    pub fn mapped<R>(to_wire: fn(&T, &EncodeCtx<'_>) -> R, from_wire: fn(R, &mut DecodeCtx<'_>) -> Option<T>) -> Self
    where
        R: Serialize + DeserializeOwned + Send + Sync + 'static,
    {
        Self::new(MappedCodec { to_wire, from_wire })
    }

    /// [`ClientMode::Delayed`].
    pub fn delayed(mut self) -> Self {
        self.mode = ClientMode::Delayed;
        self
    }

    /// [`ClientMode::Interpolate`].
    pub fn interpolate(mut self) -> Self
    where
        T: Interpolate,
    {
        self.mode = ClientMode::Interpolate;
        self.interpolate = Some(T::interpolate);
        self.extrapolate = Some(T::extrapolate);
        self
    }

    /// [`ClientMode::Predict`]; also turns on `quantize_authoritative`.
    pub fn predict(mut self) -> Self
    where
        T: PartialEq,
    {
        self.mode = ClientMode::Predict;
        self.quantize_authoritative = true;
        self
    }

    /// [`ClientMode::PredictAndInterpolate`]; also turns on `quantize_authoritative`.
    pub fn predict_and_interpolate(mut self) -> Self
    where
        T: Interpolate + PartialEq,
    {
        self.mode = ClientMode::PredictAndInterpolate;
        self.interpolate = Some(T::interpolate);
        self.extrapolate = Some(T::extrapolate);
        self.quantize_authoritative = true;
        self
    }

    /// Who receives it.
    pub fn audience(mut self, audience: Audience) -> Self {
        self.audience = audience;
        self
    }

    /// When it is sent.
    pub fn send(mut self, rule: SendRule) -> Self {
        self.send = rule;
        self
    }

    /// Priority weight for bandwidth budgeting (default 1).
    pub fn priority(mut self, weight: f32) -> Self {
        self.priority = weight;
        self
    }

    /// Server (and predicting client) round their own value through the codec every tick, so
    /// both sides simulate from identical quantized values.
    pub fn quantize_authoritative(mut self, yes: bool) -> Self {
        self.quantize_authoritative = yes;
        self
    }

    /// Misprediction test (default: encoded bytes differ).
    pub fn rollback_when(mut self, differs: fn(&T, &T) -> bool) -> Self {
        self.rollback_when = Some(differs);
        self
    }
}

/// Message direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Server events (`NetServer::send` → `Events<FromServer<M>>`).
    ServerToClient,
    /// Client commands (`NetClient::send` → `Events<FromClient<M>>`).
    ClientToServer,
    /// Both.
    Both,
}

/// When a client releases a received server message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventTiming {
    /// As soon as it arrives.
    Immediate,
    /// When the render clock reaches the message tick (effects line up with interpolation).
    Interpolated,
}

/// Per-message-type settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MessageConfig {
    /// Delivery lane.
    pub lane: Lane,
    /// Direction(s).
    pub direction: Direction,
    /// Largest accepted encoded size (default 1 KiB); larger inbound → strike.
    pub max_bytes: u32,
    /// Inbound rate limit per client (messages/s; 0 = unlimited) and burst.
    pub rate_per_sec: u32,
    /// Token-bucket burst.
    pub burst: u32,
    /// Client release timing for server messages.
    pub timing: EventTiming,
}

impl MessageConfig {
    /// Reliable-ordered lane.
    pub fn reliable(direction: Direction) -> Self {
        Self {
            lane: Lane::Reliable,
            direction,
            max_bytes: 1024,
            rate_per_sec: 30,
            burst: 60,
            timing: EventTiming::Immediate,
        }
    }

    /// Unreliable (sequenced, latest-wins) lane.
    pub fn unreliable(direction: Direction) -> Self {
        Self {
            lane: Lane::Unreliable,
            ..Self::reliable(direction)
        }
    }

    /// Set the size cap.
    pub fn max_bytes(mut self, bytes: u32) -> Self {
        self.max_bytes = bytes;
        self
    }

    /// Set the inbound rate limit.
    pub fn rate_limit(mut self, per_sec: u32, burst: u32) -> Self {
        self.rate_per_sec = per_sec;
        self.burst = burst;
        self
    }

    /// Set release timing.
    pub fn timing(mut self, timing: EventTiming) -> Self {
        self.timing = timing;
        self
    }
}

/// Read-only description of a registered component.
#[derive(Clone, Debug)]
pub struct ComponentInfo {
    /// Registered name.
    pub name: &'static str,
    /// Assigned kind (after freeze).
    pub kind: ComponentKind,
    /// Client mode.
    pub mode: ClientMode,
    /// Audience.
    pub audience: Audience,
    /// Send rule.
    pub send: SendRule,
    /// Priority weight.
    pub priority: f32,
    /// Codec schema id.
    pub schema_id: String,
}

impl ClientMode {
    /// Predicted on the controlling client.
    pub fn is_predicted(self) -> bool {
        matches!(self, ClientMode::Predict | ClientMode::PredictAndInterpolate)
    }

    /// Interpolated on non-predicting clients.
    pub fn is_interpolated(self) -> bool {
        matches!(self, ClientMode::Interpolate | ClientMode::PredictAndInterpolate)
    }
}

impl Audience {
    fn schema_str(&self) -> &'static str {
        match self {
            Audience::All => "all",
            Audience::Owner => "owner",
            Audience::ExceptOwner => "except_owner",
            Audience::Team => "team",
            Audience::Custom(_) => "custom",
        }
    }
}

/// Short, stable-enough type name for schema ids (path segments stripped).
fn short_type_name<T: ?Sized>() -> String {
    let full = std::any::type_name::<T>();
    let mut out = String::with_capacity(full.len());
    let mut seg = String::new();
    for c in full.chars() {
        if c.is_alphanumeric() || c == '_' || c == ':' {
            seg.push(c);
        } else {
            out.push_str(seg.rsplit("::").next().unwrap_or(""));
            seg.clear();
            out.push(c);
        }
    }
    out.push_str(seg.rsplit("::").next().unwrap_or(""));
    out
}

/// Client-side interpolation buffer of component `T` (lives on the entity).
#[cfg(feature = "client")]
pub(crate) struct InterpBuffer<T: Clone>(pub crate::client::interpolation::SnapshotBuffer<T>);

/// Client-side queue of `Delayed` values of component `T` (lives on the entity).
#[cfg(feature = "client")]
pub(crate) struct DelayedQueue<T>(pub std::collections::VecDeque<(Tick, T)>);

/// Type-erased operations of one registered component.
pub(crate) trait ComponentOps: Send + Sync + 'static {
    fn contains(&self, world: &World, e: Entity) -> bool;
    /// Encode the entity's value (`false` if the entity lacks the component).
    fn encode(&self, world: &World, e: Entity, w: &mut Writer, ctx: &EncodeCtx<'_>) -> bool;
    /// Decode `bytes` (all of them) and insert / overwrite the component.
    fn apply(&self, world: &mut World, e: Entity, bytes: &[u8], ctx: &mut DecodeCtx<'_>) -> Result<(), DecodeError>;
    /// Decode only (validation, comparisons).
    #[cfg_attr(not(feature = "client"), allow(dead_code))]
    fn check(&self, bytes: &[u8], ctx: &mut DecodeCtx<'_>) -> Result<(), DecodeError>;
    /// Remove the component (and any client buffers).
    #[cfg_attr(not(feature = "client"), allow(dead_code))]
    fn remove(&self, world: &mut World, e: Entity);
    /// Round the value through the codec and write it back; the encoding of the written-back
    /// value is left in `out`. `false` if absent or not decodable (value untouched).
    fn quantize(
        &self,
        world: &mut World,
        e: Entity,
        enc: &EncodeCtx<'_>,
        dec: &mut DecodeCtx<'_>,
        out: &mut Writer,
    ) -> bool;
    /// Misprediction test between two encodings (`rollback_when` or byte inequality).
    #[cfg_attr(not(feature = "client"), allow(dead_code))]
    fn differs(&self, a: &[u8], b: &[u8], ctx: &mut DecodeCtx<'_>) -> bool;
    /// Buffer an interpolation sample. `deferred`: the server's budget held this entity back
    /// (the gap since the previous sample is not rest; see `SnapshotBuffer::push_sample`).
    #[cfg(feature = "client")]
    #[allow(clippy::too_many_arguments)]
    fn push_sample(
        &self,
        world: &mut World,
        e: Entity,
        tick: Tick,
        bytes: &[u8],
        ctx: &mut DecodeCtx<'_>,
        send_interval: u8,
        deferred: bool,
    ) -> Result<(), DecodeError>;
    #[cfg(feature = "client")]
    /// Write interpolated values at `render_tick`. Extrapolation past the newest sample (at most
    /// `max_extrapolate_ticks`) only happens for entities present in the newest snapshot
    /// (`newest_snapshot`): an entity missing from newer snapshots did not change (or was
    /// deferred by the budget), so it holds its last value instead of overshooting.
    fn interpolate_all(&self, world: &mut World, render_tick: f64, max_extrapolate_ticks: f64, newest_snapshot: Tick);
    /// Newest buffered (interpolated / delayed) value, encoded (harness convergence checks).
    #[cfg(feature = "harness")]
    fn newest_sample(&self, world: &World, e: Entity, w: &mut Writer, ctx: &EncodeCtx<'_>) -> Option<Tick>;
    #[cfg(feature = "client")]
    fn push_delayed(
        &self,
        world: &mut World,
        e: Entity,
        tick: Tick,
        bytes: &[u8],
        ctx: &mut DecodeCtx<'_>,
    ) -> Result<(), DecodeError>;
    #[cfg(feature = "client")]
    fn release_delayed_all(&self, world: &mut World, render_tick: f64);
    #[cfg(feature = "client")]
    fn clear_buffers(&self, world: &mut World, e: Entity);
}

struct TypedComponent<T: Component + Clone> {
    rule: ComponentRule<T>,
}

impl<T: Component + Clone> TypedComponent<T> {
    fn decode(&self, bytes: &[u8], ctx: &mut DecodeCtx<'_>) -> Result<T, DecodeError> {
        let mut r = Reader::new(bytes);
        let v = self.rule.codec.decode(&mut r, ctx)?;
        if ctx.has_unresolved() {
            return Err(DecodeError::Unresolved);
        }
        Ok(v)
    }
}

impl<T: Component + Clone> ComponentOps for TypedComponent<T> {
    fn contains(&self, world: &World, e: Entity) -> bool {
        world.satisfies::<&T>(e)
    }

    fn encode(&self, world: &World, e: Entity, w: &mut Writer, ctx: &EncodeCtx<'_>) -> bool {
        match world.get::<&T>(e) {
            Ok(v) => {
                self.rule.codec.encode(&v, w, ctx);
                true
            }
            Err(_) => false,
        }
    }

    fn apply(&self, world: &mut World, e: Entity, bytes: &[u8], ctx: &mut DecodeCtx<'_>) -> Result<(), DecodeError> {
        let v = self.decode(bytes, ctx)?;
        if let Ok(mut slot) = world.get::<&mut T>(e) {
            *slot = v;
            return Ok(());
        }
        world
            .insert_one(e, v)
            .map_err(|_| DecodeError::Invalid("entity despawned"))
    }

    fn check(&self, bytes: &[u8], ctx: &mut DecodeCtx<'_>) -> Result<(), DecodeError> {
        self.decode(bytes, ctx).map(|_| ())
    }

    fn remove(&self, world: &mut World, e: Entity) {
        let _ = world.remove_one::<T>(e);
        #[cfg(feature = "client")]
        self.clear_buffers(world, e);
    }

    fn quantize(
        &self,
        world: &mut World,
        e: Entity,
        enc: &EncodeCtx<'_>,
        dec: &mut DecodeCtx<'_>,
        out: &mut Writer,
    ) -> bool {
        let Ok(mut slot) = world.get::<&mut T>(e) else {
            return false;
        };
        out.clear();
        self.rule.codec.encode(&slot, out, enc);
        dec.reset();
        let mut r = Reader::new(out.as_slice());
        let Ok(v) = self.rule.codec.decode(&mut r, dec) else {
            return false;
        };
        if dec.has_unresolved() {
            return false;
        }
        out.clear();
        self.rule.codec.encode(&v, out, enc);
        *slot = v;
        true
    }

    fn differs(&self, a: &[u8], b: &[u8], ctx: &mut DecodeCtx<'_>) -> bool {
        match self.rule.rollback_when {
            None => a != b,
            Some(f) => {
                ctx.reset();
                let va = self.decode(a, ctx);
                ctx.reset();
                let vb = self.decode(b, ctx);
                match (va, vb) {
                    (Ok(va), Ok(vb)) => f(&va, &vb),
                    _ => a != b,
                }
            }
        }
    }

    #[cfg(feature = "client")]
    fn push_sample(
        &self,
        world: &mut World,
        e: Entity,
        tick: Tick,
        bytes: &[u8],
        ctx: &mut DecodeCtx<'_>,
        send_interval: u8,
        deferred: bool,
    ) -> Result<(), DecodeError> {
        let v = self.decode(bytes, ctx)?;
        if let Ok(mut buf) = world.get::<&mut InterpBuffer<T>>(e) {
            buf.0.push_sample(tick, v, send_interval, deferred);
            return Ok(());
        }
        let mut buf = crate::client::interpolation::SnapshotBuffer::default();
        buf.push_sample(tick, v.clone(), send_interval, deferred);
        let has_value = world.satisfies::<&T>(e);
        let res = if has_value {
            world.insert_one(e, InterpBuffer(buf))
        } else {
            world.insert(e, (InterpBuffer(buf), v))
        };
        res.map_err(|_| DecodeError::Invalid("entity despawned"))
    }

    #[cfg(feature = "client")]
    fn interpolate_all(&self, world: &mut World, render_tick: f64, max_extrapolate_ticks: f64, newest_snapshot: Tick) {
        let Some(lerp) = self.rule.interpolate else {
            return;
        };
        let extrapolate = self.rule.extrapolate;
        for (buf, value) in world
            .query_mut::<(&mut InterpBuffer<T>, &mut T)>()
            .without::<&crate::Predicted>()
        {
            let current = buf.0.newest().is_some_and(|(t, _)| *t >= newest_snapshot);
            let max_ext = if current { max_extrapolate_ticks } else { 0.0 };
            // Entities the budget updates less often than every snapshot are drawn further
            // in the past (their own observed update gap), so they stay between two samples.
            let render = render_tick - buf.0.lag();
            if let Some(v) = buf.0.sample_with(render, lerp, extrapolate, max_ext) {
                *value = v;
            }
            // Keep one sample before the render tick for the next lerp.
            buf.0.prune(Tick(render.max(0.0).floor() as u32));
        }
    }

    /// Newest buffered (interpolated / delayed) value, encoded (harness convergence checks).
    #[cfg(feature = "harness")]
    fn newest_sample(&self, world: &World, e: Entity, w: &mut Writer, ctx: &EncodeCtx<'_>) -> Option<Tick> {
        if let Ok(buf) = world.get::<&InterpBuffer<T>>(e)
            && let Some((tick, v)) = buf.0.newest()
        {
            self.rule.codec.encode(v, w, ctx);
            return Some(*tick);
        }
        if let Ok(q) = world.get::<&DelayedQueue<T>>(e)
            && let Some((tick, v)) = q.0.back()
        {
            self.rule.codec.encode(v, w, ctx);
            return Some(*tick);
        }
        None
    }

    #[cfg(feature = "client")]
    fn push_delayed(
        &self,
        world: &mut World,
        e: Entity,
        tick: Tick,
        bytes: &[u8],
        ctx: &mut DecodeCtx<'_>,
    ) -> Result<(), DecodeError> {
        let v = self.decode(bytes, ctx)?;
        if let Ok(mut q) = world.get::<&mut DelayedQueue<T>>(e) {
            if q.0.back().is_none_or(|(t, _)| *t < tick) {
                q.0.push_back((tick, v));
                while q.0.len() > 64 {
                    q.0.pop_front();
                }
            }
            return Ok(());
        }
        let has_value = world.satisfies::<&T>(e);
        let q = DelayedQueue(std::collections::VecDeque::from([(tick, v.clone())]));
        let res = if has_value {
            world.insert_one(e, q)
        } else {
            // First value: show it immediately (the entity would be incomplete otherwise).
            world.insert(e, (q, v))
        };
        res.map_err(|_| DecodeError::Invalid("entity despawned"))
    }

    #[cfg(feature = "client")]
    fn release_delayed_all(&self, world: &mut World, render_tick: f64) {
        for (q, value) in world.query_mut::<(&mut DelayedQueue<T>, &mut T)>() {
            while let Some((tick, _)) = q.0.front() {
                if f64::from(tick.0) > render_tick {
                    break;
                }
                let (_, v) = q.0.pop_front().expect("front");
                *value = v;
            }
        }
    }

    #[cfg(feature = "client")]
    fn clear_buffers(&self, world: &mut World, e: Entity) {
        let _ = world.remove_one::<InterpBuffer<T>>(e);
        let _ = world.remove_one::<DelayedQueue<T>>(e);
    }
}

/// Type-erased registered component.
pub(crate) struct ComponentEntry {
    pub(crate) name: &'static str,
    pub(crate) type_id: TypeId,
    pub(crate) kind: Option<ComponentKind>,
    pub(crate) mode: ClientMode,
    pub(crate) audience: Audience,
    pub(crate) send: SendRule,
    pub(crate) priority: f32,
    pub(crate) quantize_authoritative: bool,
    pub(crate) schema_id: String,
    pub(crate) ops: Box<dyn ComponentOps>,
}

impl ComponentEntry {
    /// Kind bit in presence masks (after freeze).
    pub(crate) fn bit(&self) -> u64 {
        1u64 << self.kind.expect("registry frozen").0
    }
}

/// Type-erased decoded input value.
pub(crate) type InputValue = Box<dyn std::any::Any + Send + Sync>;

/// Type-erased operations on the registered input type.
pub(crate) trait InputOps: Send + Sync + 'static {
    /// Decode (exact) and sanitize. `Ok((value, true))` when sanitize passed; `Ok((neutral, false))`
    /// when sanitize rejected it (strike).
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    fn decode(&self, bytes: &[u8]) -> Result<(InputValue, bool), DecodeError>;
    fn neutral(&self) -> InputValue;
    #[cfg_attr(not(any(feature = "server", feature = "client")), allow(dead_code))]
    fn repeat(&self, v: &InputValue) -> InputValue;
    fn clone_value(&self, v: &InputValue) -> InputValue;
    #[cfg_attr(not(feature = "client"), allow(dead_code))]
    fn encode(&self, v: &InputValue) -> Vec<u8>;
    /// Insert/overwrite `PlayerInput<I>` on `e`.
    fn write(&self, world: &mut World, e: Entity, v: &InputValue);
    /// Client: sample the next input (`InputSampler<I>` resource, or `Default`).
    #[cfg(feature = "client")]
    fn sample(&self, ctx: &mut space3d_core::SystemCtx) -> InputValue;
}

struct TypedInput<I>(std::marker::PhantomData<fn() -> I>);

impl<I: NetInput> InputOps for TypedInput<I> {
    fn decode(&self, bytes: &[u8]) -> Result<(InputValue, bool), DecodeError> {
        let mut v: I = crate::wire::postcard_exact(bytes)?;
        match v.sanitize() {
            Ok(()) => Ok((Box::new(v), true)),
            Err(_) => Ok((Box::new(I::neutral()), false)),
        }
    }

    fn neutral(&self) -> InputValue {
        Box::new(I::neutral())
    }

    fn repeat(&self, v: &InputValue) -> InputValue {
        match v.downcast_ref::<I>() {
            Some(i) => Box::new(i.repeat()),
            None => self.neutral(),
        }
    }

    fn clone_value(&self, v: &InputValue) -> InputValue {
        match v.downcast_ref::<I>() {
            Some(i) => Box::new(i.clone()),
            None => self.neutral(),
        }
    }

    fn encode(&self, v: &InputValue) -> Vec<u8> {
        let i = v.downcast_ref::<I>().cloned().unwrap_or_default();
        postcard::to_stdvec(&i).expect("postcard encode")
    }

    fn write(&self, world: &mut World, e: Entity, v: &InputValue) {
        let i = v.downcast_ref::<I>().cloned().unwrap_or_else(I::neutral);
        if let Ok(mut slot) = world.get::<&mut crate::PlayerInput<I>>(e) {
            slot.0 = i;
            return;
        }
        let _ = world.insert_one(e, crate::PlayerInput(i));
    }

    #[cfg(feature = "client")]
    fn sample(&self, ctx: &mut space3d_core::SystemCtx) -> InputValue {
        if !ctx.resources.contains::<crate::InputSampler<I>>() {
            return Box::new(I::default());
        }
        let world = &mut *ctx.world;
        let mut i = ctx.resources.scope::<crate::InputSampler<I>, _>(|res, sampler| {
            let mut inner = space3d_core::SystemCtx { world, resources: res };
            sampler.sample(&mut inner)
        });
        // The server sanitizes too; doing it here keeps prediction identical.
        if i.sanitize().is_err() {
            i = I::neutral();
        }
        Box::new(i)
    }
}

/// Registered input type.
pub(crate) struct InputEntry {
    pub(crate) name: &'static str,
    pub(crate) type_id: TypeId,
    pub(crate) ops: Box<dyn InputOps>,
}

/// Why an inbound message was refused (strike).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "server"), allow(dead_code))]
pub(crate) enum MessageReject {
    Decode(DecodeError),
    Invalid(&'static str),
}

/// Type-erased operations of one message type.
pub(crate) trait MessageOps: Send + Sync + 'static {
    /// Server: decode, validate, push `Events<FromClient<M>>`.
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    fn deliver_from_client(
        &self,
        resources: &mut space3d_core::Resources,
        bytes: &[u8],
        client: crate::NetClientId,
        player: PlayerId,
        tick: Tick,
    ) -> Result<(), MessageReject>;
    /// Client: decode, push `Events<FromServer<M>>`.
    #[cfg_attr(not(feature = "client"), allow(dead_code))]
    fn deliver_from_server(
        &self,
        resources: &mut space3d_core::Resources,
        bytes: &[u8],
        tick: Tick,
    ) -> Result<(), DecodeError>;
}

struct TypedMessage<M>(std::marker::PhantomData<fn() -> M>);

impl<M: NetMessage> MessageOps for TypedMessage<M> {
    fn deliver_from_client(
        &self,
        resources: &mut space3d_core::Resources,
        bytes: &[u8],
        client: crate::NetClientId,
        player: PlayerId,
        tick: Tick,
    ) -> Result<(), MessageReject> {
        let message: M = crate::wire::postcard_exact(bytes).map_err(MessageReject::Decode)?;
        message.validate().map_err(|e| MessageReject::Invalid(e.0))?;
        resources
            .init::<space3d_core::Events<crate::FromClient<M>>>()
            .send(crate::FromClient {
                client,
                player,
                tick,
                message,
            });
        Ok(())
    }

    fn deliver_from_server(
        &self,
        resources: &mut space3d_core::Resources,
        bytes: &[u8],
        tick: Tick,
    ) -> Result<(), DecodeError> {
        let message: M = crate::wire::postcard_exact(bytes)?;
        resources
            .init::<space3d_core::Events<crate::FromServer<M>>>()
            .send(crate::FromServer { tick, message });
        Ok(())
    }
}

/// Registered message type.
pub(crate) struct MessageEntry {
    pub(crate) name: &'static str,
    pub(crate) type_id: TypeId,
    pub(crate) config: MessageConfig,
    pub(crate) kind: u16,
    pub(crate) ops: Box<dyn MessageOps>,
}

impl MessageEntry {
    /// Whether clients may send it.
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn client_may_send(&self) -> bool {
        matches!(self.config.direction, Direction::ClientToServer | Direction::Both)
    }

    /// Whether the server may send it.
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn server_may_send(&self) -> bool {
        matches!(self.config.direction, Direction::ServerToClient | Direction::Both)
    }
}

/// The network schema (resource). See the module docs.
#[derive(Default)]
pub struct ReplicationRegistry {
    components: Vec<ComponentEntry>,
    input: Option<InputEntry>,
    messages: Vec<MessageEntry>,
    frozen: bool,
    schema_hash: u64,
}

impl std::fmt::Debug for ReplicationRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplicationRegistry")
            .field(
                "components",
                &self.components.iter().map(|c| c.name).collect::<Vec<_>>(),
            )
            .field("input", &self.input_name())
            .field("messages", &self.messages.iter().map(|m| m.name).collect::<Vec<_>>())
            .field("frozen", &self.frozen)
            .finish()
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a 64 over `bytes`, continuing from `h`.
pub(crate) fn fnv1a(mut h: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

impl ReplicationRegistry {
    fn assert_open(&self, what: &str, name: &str) {
        assert!(
            !self.frozen,
            "ReplicationRegistry: cannot register {what} {name:?} after the registry froze (first Startup)"
        );
    }

    /// Register a component. Panics if frozen, if `name` or `T` is already registered, or
    /// beyond [`MAX_COMPONENTS`].
    pub fn register_component<T: Component + Clone>(
        &mut self,
        name: &'static str,
        rule: ComponentRule<T>,
    ) -> &mut Self {
        self.assert_open("component", name);
        assert!(
            !self.components.iter().any(|c| c.name == name),
            "ReplicationRegistry: component name {name:?} registered twice"
        );
        assert!(
            !self.components.iter().any(|c| c.type_id == TypeId::of::<T>()),
            "ReplicationRegistry: component type {} registered twice",
            std::any::type_name::<T>()
        );
        assert!(
            self.components.len() < MAX_COMPONENTS,
            "ReplicationRegistry: at most {MAX_COMPONENTS} replicated components"
        );
        assert!(
            !rule.mode.is_interpolated() || rule.interpolate.is_some(),
            "ReplicationRegistry: {name:?} is interpolated without an interpolation function"
        );
        self.components.push(ComponentEntry {
            name,
            type_id: TypeId::of::<T>(),
            kind: None,
            mode: rule.mode,
            audience: rule.audience,
            send: rule.send,
            priority: rule.priority,
            quantize_authoritative: rule.quantize_authoritative,
            schema_id: rule.codec.schema_id(),
            ops: Box::new(TypedComponent { rule }),
        });
        self
    }

    /// Register the input type (exactly one per game). Panics if frozen or already set.
    pub fn register_input<I: NetInput>(&mut self, name: &'static str) -> &mut Self {
        self.assert_open("input", name);
        assert!(
            self.input.is_none(),
            "ReplicationRegistry: the input type is already registered as {:?}",
            self.input_name()
        );
        self.input = Some(InputEntry {
            name,
            type_id: TypeId::of::<I>(),
            ops: Box::new(TypedInput::<I>(std::marker::PhantomData)),
        });
        self
    }

    /// Register a message type. Panics if frozen or duplicated.
    pub fn register_message<M: NetMessage>(&mut self, name: &'static str, config: MessageConfig) -> &mut Self {
        self.assert_open("message", name);
        assert!(
            !self
                .messages
                .iter()
                .any(|m| m.name == name || m.type_id == TypeId::of::<M>()),
            "ReplicationRegistry: message {name:?} ({}) registered twice",
            std::any::type_name::<M>()
        );
        assert!(self.messages.len() < usize::from(u16::MAX), "too many message types");
        self.messages.push(MessageEntry {
            name,
            type_id: TypeId::of::<M>(),
            config,
            kind: 0,
            ops: Box::new(TypedMessage::<M>(std::marker::PhantomData)),
        });
        self
    }

    /// Assign kinds by sorted name and compute the schema hash (FNV-1a 64 over
    /// `PROTOCOL_VERSION`, each component `name|schema_id|mode|audience`, the input name and
    /// each message `name|lane|direction`). Idempotent. Called by the replication plugins at
    /// the first `Startup`.
    pub fn freeze(&mut self) {
        if self.frozen {
            return;
        }
        self.components.sort_by(|a, b| a.name.cmp(b.name));
        for (i, c) in self.components.iter_mut().enumerate() {
            c.kind = Some(ComponentKind(i as u16));
        }
        self.messages.sort_by(|a, b| a.name.cmp(b.name));
        for (i, m) in self.messages.iter_mut().enumerate() {
            m.kind = i as u16;
        }
        self.frozen = true;
        let mut h = fnv1a(FNV_OFFSET, &crate::protocol::PROTOCOL_VERSION.to_le_bytes());
        for line in self.schema_lines() {
            h = fnv1a(h, line.as_bytes());
            h = fnv1a(h, b"\n");
        }
        self.schema_hash = h;
    }

    fn schema_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for c in &self.components {
            out.push(format!(
                "component|{}|{}|{:?}|{}",
                c.name,
                c.schema_id,
                c.mode,
                c.audience.schema_str()
            ));
        }
        out.push(format!("input|{}", self.input_name().unwrap_or("")));
        for m in &self.messages {
            out.push(format!(
                "message|{}|{:?}|{:?}",
                m.name, m.config.lane, m.config.direction
            ));
        }
        out
    }

    /// Whether [`Self::freeze`] ran.
    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// Schema hash carried in `Hello`. Panics if not frozen.
    pub fn schema_hash(&self) -> u64 {
        assert!(self.frozen, "ReplicationRegistry::schema_hash before freeze");
        self.schema_hash
    }

    /// Human-readable schema lines (admin `GET /admin/v1/schema`, mismatch debugging).
    pub fn manifest(&self) -> Vec<String> {
        let mut out = vec![format!("protocol|{}", crate::protocol::PROTOCOL_VERSION)];
        out.extend(self.schema_lines());
        if self.frozen {
            out.push(format!("schema_hash|{:016x}", self.schema_hash));
        } else {
            out.push("schema_hash|<not frozen>".into());
        }
        out
    }

    /// Number of registered components.
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    /// Kind of component type `T` (after freeze).
    pub fn kind_of<T: 'static>(&self) -> Option<ComponentKind> {
        self.components
            .iter()
            .find(|c| c.type_id == TypeId::of::<T>())
            .and_then(|c| c.kind)
    }

    /// Kind by registered name (after freeze).
    pub fn kind_by_name(&self, name: &str) -> Option<ComponentKind> {
        self.components.iter().find(|c| c.name == name).and_then(|c| c.kind)
    }

    /// Description of a registered component.
    pub fn component_info(&self, kind: ComponentKind) -> Option<ComponentInfo> {
        let c = self.components.get(usize::from(kind.0))?;
        Some(ComponentInfo {
            name: c.name,
            kind: c.kind?,
            mode: c.mode,
            audience: c.audience,
            send: c.send,
            priority: c.priority,
            schema_id: c.schema_id.clone(),
        })
    }

    /// Registered input name, if any.
    pub fn input_name(&self) -> Option<&'static str> {
        self.input.as_ref().map(|i| i.name)
    }

    /// Whether `I` is the registered input type (an `InputSampler<J>` for any other `J` is
    /// never called).
    pub fn input_is<I: 'static>(&self) -> bool {
        self.input.as_ref().is_some_and(|i| i.type_id == TypeId::of::<I>())
    }

    /// Registered message names in kind order (after freeze).
    pub fn message_names(&self) -> Vec<&'static str> {
        self.messages.iter().map(|m| m.name).collect()
    }

    /// Kind + config of message type `M` (after freeze).
    pub fn message_kind<M: 'static>(&self) -> Option<(u16, MessageConfig)> {
        if !self.frozen {
            return None;
        }
        self.messages
            .iter()
            .find(|m| m.type_id == TypeId::of::<M>())
            .map(|m| (m.kind, m.config))
    }

    pub(crate) fn components(&self) -> &[ComponentEntry] {
        &self.components
    }

    pub(crate) fn component(&self, kind: u16) -> Option<&ComponentEntry> {
        self.components.get(usize::from(kind))
    }

    pub(crate) fn input(&self) -> Option<&InputEntry> {
        self.input.as_ref()
    }

    pub(crate) fn message(&self, kind: u16) -> Option<&MessageEntry> {
        self.messages.get(usize::from(kind))
    }

    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn message_by_type(&self, type_id: TypeId) -> Option<&MessageEntry> {
        self.messages.iter().find(|m| m.type_id == type_id)
    }

    /// Mask of all registered kinds with a predicted client mode.
    pub(crate) fn predicted_mask(&self) -> u64 {
        self.components
            .iter()
            .filter(|c| c.mode.is_predicted())
            .fold(0, |m, c| m | c.bit())
    }

    /// Mask of all kinds interpolated on non-predicting clients.
    #[cfg_attr(not(feature = "client"), allow(dead_code))]
    pub(crate) fn interpolated_mask(&self) -> u64 {
        self.components
            .iter()
            .filter(|c| c.mode.is_interpolated())
            .fold(0, |m, c| m | c.bit())
    }

    /// Mask of all kinds with authoritative quantization.
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn quantized_mask(&self) -> u64 {
        self.components
            .iter()
            .filter(|c| c.quantize_authoritative)
            .fold(0, |m, c| m | c.bit())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::{EntityMap, NetIdMap};
    use serde::Deserialize;

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct Health(u16);
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct Ammo(u8);
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Move {
        x: i8,
    }
    impl NetInput for Move {}
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct Chat(String);
    impl NetMessage for Chat {}
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Target(Entity);

    fn build(order: bool) -> ReplicationRegistry {
        let mut r = ReplicationRegistry::default();
        if order {
            r.register_component::<Health>("t.health", ComponentRule::serde());
            r.register_component::<Ammo>("t.ammo", ComponentRule::serde().audience(Audience::Owner));
        } else {
            r.register_component::<Ammo>("t.ammo", ComponentRule::serde().audience(Audience::Owner));
            r.register_component::<Health>("t.health", ComponentRule::serde());
        }
        r.register_input::<Move>("t.move");
        r.register_message::<Chat>("t.chat", MessageConfig::reliable(Direction::Both));
        r.freeze();
        r
    }

    #[test]
    fn schema_hash_is_order_independent() {
        let a = build(true);
        let b = build(false);
        assert_eq!(a.schema_hash(), b.schema_hash());
        assert_eq!(a.kind_by_name("t.ammo"), Some(ComponentKind(0)));
        assert_eq!(a.kind_of::<Health>(), Some(ComponentKind(1)));
        assert_eq!(b.kind_of::<Health>(), Some(ComponentKind(1)));
        assert_eq!(a.message_kind::<Chat>().map(|m| m.0), Some(0));
        assert!(a.manifest().iter().any(|l| l.starts_with("schema_hash|")));
    }

    #[test]
    fn schema_hash_changes_with_the_schema() {
        let a = build(true);
        let mut c = ReplicationRegistry::default();
        c.register_component::<Health>("t.health", ComponentRule::serde());
        c.register_component::<Ammo>("t.ammo", ComponentRule::serde()); // audience differs
        c.register_input::<Move>("t.move");
        c.register_message::<Chat>("t.chat", MessageConfig::reliable(Direction::Both));
        c.freeze();
        assert_ne!(a.schema_hash(), c.schema_hash());
    }

    #[test]
    #[should_panic(expected = "registered twice")]
    fn duplicate_name_panics() {
        let mut r = ReplicationRegistry::default();
        r.register_component::<Health>("x", ComponentRule::serde());
        r.register_component::<Ammo>("x", ComponentRule::serde());
    }

    #[test]
    #[should_panic(expected = "registered twice")]
    fn duplicate_type_panics() {
        let mut r = ReplicationRegistry::default();
        r.register_component::<Health>("x", ComponentRule::serde());
        r.register_component::<Health>("y", ComponentRule::serde());
    }

    #[test]
    #[should_panic(expected = "after the registry froze")]
    fn late_registration_panics() {
        let mut r = build(true);
        r.register_component::<Target>("late", ComponentRule::mapped(|_, _| 0u32, |_, _| None));
    }

    #[test]
    fn erased_component_ops() {
        let r = build(true);
        let mut world = World::new();
        let e = world.spawn((Health(7),));
        let ids = NetIdMap::default();
        let h = r.component(r.kind_of::<Health>().unwrap().0).unwrap();
        let mut w = Writer::new();
        assert!(h.ops.encode(&world, e, &mut w, &EncodeCtx::new(&ids)));
        let bytes = w.into_vec();
        let other = world.spawn(());
        let map = EntityMap::default();
        h.ops
            .apply(&mut world, other, &bytes, &mut DecodeCtx::new(&map))
            .unwrap();
        assert_eq!(*world.get::<&Health>(other).unwrap(), Health(7));
        assert!(h.ops.contains(&world, other));
        h.ops.remove(&mut world, other);
        assert!(!h.ops.contains(&world, other));
        assert!(h.ops.differs(&bytes, &[8], &mut DecodeCtx::empty()));
        // Input ops.
        assert!(r.input_is::<Move>() && !r.input_is::<Health>());
        let input = r.input().unwrap();
        let (v, ok) = input.ops.decode(&postcard::to_stdvec(&Move { x: 3 }).unwrap()).unwrap();
        assert!(ok);
        input.ops.write(&mut world, e, &v);
        assert_eq!(world.get::<&crate::PlayerInput<Move>>(e).unwrap().0, Move { x: 3 });
        assert!(input.ops.decode(&[0x01, 0x02, 0x03]).is_err());
    }

    #[test]
    fn mapped_codec_resolves_through_maps() {
        let mut r = ReplicationRegistry::default();
        r.register_component::<Target>(
            "t.target",
            ComponentRule::mapped(
                |t: &Target, ctx: &EncodeCtx<'_>| ctx.net_id(t.0).map(|n| n.0),
                |id: Option<u32>, ctx: &mut DecodeCtx<'_>| ctx.entity(crate::NetId(id?)).map(Target),
            ),
        );
        r.freeze();
        let mut world = World::new();
        let target = world.spawn(());
        let holder = world.spawn((Target(target),));
        let mut ids = NetIdMap::default();
        let nid = ids.allocate(target);
        let c = r.component(0).unwrap();
        let mut w = Writer::new();
        c.ops.encode(&world, holder, &mut w, &EncodeCtx::new(&ids));
        let bytes = w.into_vec();
        // Client that does not know the target yet: unresolved.
        let mut cw = World::new();
        let ch = cw.spawn(());
        let mut map = EntityMap::default();
        assert_eq!(
            c.ops.apply(&mut cw, ch, &bytes, &mut DecodeCtx::new(&map)),
            Err(DecodeError::Unresolved)
        );
        let ct = cw.spawn(());
        map.insert(nid, ct);
        c.ops.apply(&mut cw, ch, &bytes, &mut DecodeCtx::new(&map)).unwrap();
        assert_eq!(cw.get::<&Target>(ch).unwrap().0, ct);
    }
}
