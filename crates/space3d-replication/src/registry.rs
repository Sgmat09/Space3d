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

use crate::PlayerId;
use crate::input::NetInput;
use crate::mapping::{DecodeCtx, EncodeCtx};
use crate::messages::NetMessage;
use crate::transport::Lane;
use crate::wire::{DecodeError, Reader, Writer};
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
        postcard::from_bytes(rest).map_err(|e| DecodeError::Postcard(e.to_string()))
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
        todo!()
    }

    fn decode(&self, r: &mut Reader<'_>, ctx: &mut DecodeCtx<'_>) -> Result<T, DecodeError> {
        todo!()
    }

    fn schema_id(&self) -> String {
        format!("mapped:{}", std::any::type_name::<R>())
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
}

impl Interpolate for Vec3 {
    fn interpolate(a: &Self, b: &Self, t: f32) -> Self {
        a.lerp(*b, t)
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
}

/// Per-component replication rule (builder).
pub struct ComponentRule<T> {
    pub(crate) codec: Box<dyn NetCodec<T>>,
    pub(crate) mode: ClientMode,
    pub(crate) interpolate: Option<fn(&T, &T, f32) -> T>,
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

/// Type-erased registered component (encode/decode/insert/remove/compare/interpolate
/// operations). The replication builder defines the erased operation set.
pub(crate) struct ComponentEntry {
    pub(crate) name: &'static str,
    pub(crate) type_id: TypeId,
    pub(crate) kind: Option<ComponentKind>,
}

/// The network schema (resource). See the module docs.
#[derive(Default)]
pub struct ReplicationRegistry {
    components: Vec<ComponentEntry>,
    input: Option<(&'static str, TypeId)>,
    messages: Vec<(&'static str, TypeId, MessageConfig)>,
    frozen: bool,
    schema_hash: u64,
}

impl ReplicationRegistry {
    /// Register a component. Panics if frozen, if `name` or `T` is already registered, or
    /// beyond [`MAX_COMPONENTS`].
    pub fn register_component<T: Component + Clone>(
        &mut self,
        name: &'static str,
        rule: ComponentRule<T>,
    ) -> &mut Self {
        todo!()
    }

    /// Register the input type (exactly one per game). Panics if frozen or already set.
    pub fn register_input<I: NetInput>(&mut self, name: &'static str) -> &mut Self {
        todo!()
    }

    /// Register a message type. Panics if frozen or duplicated.
    pub fn register_message<M: NetMessage>(&mut self, name: &'static str, config: MessageConfig) -> &mut Self {
        todo!()
    }

    /// Assign kinds by sorted name and compute the schema hash (FNV-1a 64 over
    /// `PROTOCOL_VERSION`, each component `name|schema_id|mode|audience`, the input name and
    /// each message `name|lane|direction`). Idempotent. Called by the replication plugins at
    /// the first `Startup`.
    pub fn freeze(&mut self) {
        todo!()
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
        todo!()
    }

    /// Number of registered components.
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    /// Kind of component type `T` (after freeze).
    pub fn kind_of<T: 'static>(&self) -> Option<ComponentKind> {
        todo!()
    }

    /// Kind by registered name (after freeze).
    pub fn kind_by_name(&self, name: &str) -> Option<ComponentKind> {
        todo!()
    }

    /// Description of a registered component.
    pub fn component_info(&self, kind: ComponentKind) -> Option<ComponentInfo> {
        todo!()
    }

    /// Registered input name, if any.
    pub fn input_name(&self) -> Option<&'static str> {
        self.input.map(|(n, _)| n)
    }
}
