use space3d_core::MeshData;

/// MikkTSpace tangents `[x, y, z, w]` (w = ±1 bitangent sign) for normal mapping: splits
/// vertices whose per-corner tangents disagree (mirrored UVs), so it returns the (possibly
/// re-indexed) mesh with the tangents. ([`super::compute_tangents`] is Lengyel's per-vertex
/// average, not MikkTSpace.)
///
/// Until the MikkTSpace implementation lands (materials milestone PM2), this returns the mesh
/// unchanged and no tangents — which renderers treat as "derive tangents per pixel".
pub fn compute_mikktspace_tangents(mesh: &MeshData) -> (MeshData, Vec<[f32; 4]>) {
    (mesh.clone(), Vec::new()) // STUB(P-MAT)
}
