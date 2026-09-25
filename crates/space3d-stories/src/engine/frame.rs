//! Frame stories (`Engine/Frame/*`): HDR and tonemapping, render scale, multiple cameras, the LDR fallback, node phases, scene depth and overlay stages.
//!
//! Owner: F (graphics spec §3.6).

use crate::Story;

/// This area's stories, in registry order.
pub(crate) fn stories() -> Vec<Story> {
    Vec::new()
}
