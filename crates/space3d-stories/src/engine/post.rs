//! Post-processing stories (`Engine/Post/*`): bloom, auto exposure, FXAA and lens effects, color grading, distortion, tonemappers.
//!
//! Owner: P-POST (graphics spec §3.6).

use crate::Story;

/// This area's stories, in registry order.
pub(crate) fn stories() -> Vec<Story> {
    Vec::new()
}
