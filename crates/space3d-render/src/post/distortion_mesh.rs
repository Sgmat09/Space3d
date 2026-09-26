//! The built-in `"space3d/distortion-meshes"` node: draws entities with a
//! [`super::api::DistortionMaterial`] into the distortion buffer of every post camera that sees
//! them, through the public node API (graphics spec §1.8). F2 registers a node that never has
//! work; the post area implements it in PO5.

use crate::node::{DrawContext, PhaseSlot, RenderNode, RenderNodes, RenderPhase, SlotMask, ViewPrepareContext};

/// Name of the built-in node in [`RenderNodes`].
pub(crate) const NAME: &str = "space3d/distortion-meshes";

static SLOTS: [PhaseSlot; 1] = [PhaseSlot::new(RenderPhase::Distortion, 0)];

struct DistortionMeshes;

impl RenderNode for DistortionMeshes {
    fn slots(&self) -> &[PhaseSlot] {
        &SLOTS
    }

    fn prepare_view(&mut self, ctx: &mut ViewPrepareContext<'_>) -> SlotMask {
        let _ = ctx; // STUB(P-POST): work iff a visible DistortionMaterial mesh exists
        SlotMask::NONE
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        let _ = (pass, ctx); // STUB(P-POST)
    }
}

/// Register the node (unless the app replaced it under the same name).
pub(super) fn register(nodes: &mut RenderNodes) {
    if !nodes.contains(NAME) {
        nodes.insert(NAME, DistortionMeshes);
    }
}
