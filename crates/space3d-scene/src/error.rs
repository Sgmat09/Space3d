/// Errors produced while saving, parsing or spawning scenes.
///
/// Messages are stored as strings so the error is `Clone` and can be kept in load statuses.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SceneError {
    /// The scene references a component name that is not in the [`crate::ComponentRegistry`].
    #[error("unknown component `{0}` (not registered in the ComponentRegistry)")]
    UnknownComponent(String),
    /// A component value could not be serialized or deserialized.
    #[error("component `{name}`: {message}")]
    Component {
        /// Registered component name.
        name: String,
        /// Underlying serde error.
        message: String,
    },
    /// RON (de)serialization failed.
    #[error("RON: {0}")]
    Ron(String),
    /// JSON (de)serialization failed.
    #[error("JSON: {0}")]
    Json(String),
    /// Reading the scene source failed.
    #[error("I/O: {0}")]
    Io(String),
    /// Operation not available on this platform (e.g. filesystem paths on the web).
    #[error("unsupported: {0}")]
    Unsupported(String),
}
