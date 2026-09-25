//! The built-in skybox node (`"space3d/skybox"`, Skybox phase at order 0), written against the
//! public node API. F2 registers a node that never has work; the materials area (PM3) draws
//! [`super::api::Skybox`] cameras' cubemaps with it.

use crate::node::{DrawContext, PhaseSlot, RenderNode, RenderNodes, RenderPhase, SlotMask, ViewPrepareContext};

/// Name of the built-in skybox node in [`RenderNodes`].
pub(crate) const NAME: &str = "space3d/skybox";

static SLOTS: [PhaseSlot; 1] = [PhaseSlot::new(RenderPhase::Skybox, 0)];

struct SkyboxNode;

impl RenderNode for SkyboxNode {
    fn slots(&self) -> &[PhaseSlot] {
        &SLOTS
    }

    fn prepare_view(&mut self, ctx: &mut ViewPrepareContext<'_>) -> SlotMask {
        let _ = ctx; // STUB(P-MAT): work iff the camera has a Skybox with a ready cubemap
        SlotMask::NONE
    }

    fn draw(&mut self, pass: &mut wgpu::RenderPass<'static>, ctx: &DrawContext<'_>) {
        let _ = (pass, ctx); // STUB(P-MAT)
    }
}

/// Register the node (unless the app replaced it under the same name).
pub(super) fn register(nodes: &mut RenderNodes) {
    if !nodes.contains(NAME) {
        nodes.insert(NAME, SkyboxNode);
    }
}
