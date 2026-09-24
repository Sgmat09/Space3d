//! Error type shared by sources, loaders and the asset server.

use std::fmt::Display;
use thiserror::Error;

/// Anything that can go wrong while locating, reading or decoding an asset.
///
/// Cheap to clone so it can live inside [`crate::LoadState::Failed`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AssetError {
    /// The source has no file at this path.
    #[error("asset not found: {0}")]
    NotFound(String),
    /// Reading failed for another reason (permissions, network, ...).
    #[error("i/o error reading '{path}': {message}")]
    Io {
        /// Path that failed.
        path: String,
        /// Human-readable cause.
        message: String,
    },
    /// No registered loader handles this extension for the requested asset type.
    #[error("no loader registered for extension '{extension}' producing `{type_name}`")]
    NoLoader {
        /// Lower-cased file extension (may be empty).
        extension: String,
        /// Requested asset type.
        type_name: &'static str,
    },
    /// The bytes were read but could not be decoded.
    #[error("failed to decode '{path}': {message}")]
    Decode {
        /// Path of the asset being decoded.
        path: String,
        /// Human-readable cause.
        message: String,
    },
    /// A `name://` prefix referenced a source that was never registered.
    #[error("unknown asset source '{0}'")]
    UnknownSource(String),
    /// The operation is not available on this platform (e.g. filesystem on the web).
    #[error("unsupported on this platform: {0}")]
    Unsupported(String),
    /// The path is malformed or escapes the asset root.
    #[error("invalid asset path '{0}'")]
    InvalidPath(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

impl AssetError {
    /// Shorthand for [`AssetError::Decode`].
    pub fn decode(path: impl Display, err: impl Display) -> Self {
        Self::Decode {
            path: path.to_string(),
            message: err.to_string(),
        }
    }

    /// Shorthand for [`AssetError::Io`].
    pub fn io(path: impl Display, err: impl Display) -> Self {
        Self::Io {
            path: path.to_string(),
            message: err.to_string(),
        }
    }
}
