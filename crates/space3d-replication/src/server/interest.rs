//! Interest management: which entities are relevant to which client.
//!
//! Rooms are the hard partition (separate `World`s). Inside a room: `InterestPolicy::RoomWide`
//! or a spatial grid queried around each client's `InterestSource`s (enter at `radius`, leave
//! at `radius × hysteresis`). Precedence: own entities (always) > `Visibility` override >
//! `NetRelevance` > `VisibilityRule`s > spatial. Children inherit their root's relevance so
//! `Parent` never dangles. Component-level `Audience` is applied later, per component.

use super::change::{ChangeCache, EntityRecord};
use super::{InterestPolicy, Visibility};
use crate::{FastMap, NetId, NetRelevance, PlayerId};
use space3d_core::glam::Vec3;
use space3d_core::{Entity, World};

/// Game relevancy rule (fog of war, team vision). Evaluated only for spatial candidates.
pub trait VisibilityRule: Send + Sync + 'static {
    /// Whether `viewer` may see `entity`.
    fn visible(&self, world: &World, viewer: PlayerId, entity: Entity) -> bool;
}

impl<F: Fn(&World, PlayerId, Entity) -> bool + Send + Sync + 'static> VisibilityRule for F {
    fn visible(&self, world: &World, viewer: PlayerId, entity: Entity) -> bool {
        self(world, viewer, entity)
    }
}

/// Uniform 2D (XZ) hash grid of record keys (`NetId` values), updated incrementally.
#[derive(Debug, Default)]
pub struct SpatialGrid {
    cell: f32,
    cells: crate::FastMap<(i32, i32), smallvec::SmallVec<[u32; 8]>>,
}

impl SpatialGrid {
    /// Grid with `cell` metres per cell.
    pub fn new(cell: f32) -> Self {
        Self {
            cell,
            cells: Default::default(),
        }
    }

    fn cell_size(&self) -> f32 {
        if self.cell > 0.0 { self.cell } else { 32.0 }
    }

    /// Cell of a position.
    pub fn cell_of(&self, p: Vec3) -> (i32, i32) {
        let c = self.cell_size();
        ((p.x / c).floor() as i32, (p.z / c).floor() as i32)
    }

    /// Change the cell size (rebuilds nothing: call before inserting).
    pub fn set_cell(&mut self, cell: f32) {
        if (cell - self.cell).abs() > f32::EPSILON && self.cells.is_empty() {
            self.cell = cell;
        }
    }

    /// Move / insert a record.
    pub fn update(&mut self, record: u32, old: Option<Vec3>, new: Option<Vec3>) {
        let old_cell = old.filter(|p| p.is_finite()).map(|p| self.cell_of(p));
        let new_cell = new.filter(|p| p.is_finite()).map(|p| self.cell_of(p));
        if old_cell == new_cell {
            return;
        }
        if let Some(c) = old_cell
            && let Some(v) = self.cells.get_mut(&c)
        {
            v.retain(|r| *r != record);
            if v.is_empty() {
                self.cells.remove(&c);
            }
        }
        if let Some(c) = new_cell {
            let v = self.cells.entry(c).or_default();
            if !v.contains(&record) {
                v.push(record);
            }
        }
    }

    /// Records in cells overlapping the circle (caller filters by exact distance). Output is
    /// sorted and deduplicated.
    pub fn query(&self, center: Vec3, radius: f32, out: &mut Vec<u32>) {
        if !center.is_finite() || !radius.is_finite() {
            return;
        }
        let c = self.cell_size();
        let (x0, z0) = (
            ((center.x - radius) / c).floor() as i32,
            ((center.z - radius) / c).floor() as i32,
        );
        let (x1, z1) = (
            ((center.x + radius) / c).floor() as i32,
            ((center.z + radius) / c).floor() as i32,
        );
        let start = out.len();
        if ((x1 - x0 + 1) as i64) * ((z1 - z0 + 1) as i64) > self.cells.len() as i64 {
            for ((x, z), v) in &self.cells {
                if (x0..=x1).contains(x) && (z0..=z1).contains(z) {
                    out.extend_from_slice(v);
                }
            }
        } else {
            for x in x0..=x1 {
                for z in z0..=z1 {
                    if let Some(v) = self.cells.get(&(x, z)) {
                        out.extend_from_slice(v);
                    }
                }
            }
        }
        out[start..].sort_unstable();
        let mut tail = out.split_off(start);
        tail.dedup();
        out.extend(tail);
    }
}

/// XZ distance.
pub(crate) fn dist_xz(a: Vec3, b: Vec3) -> f32 {
    let d = a - b;
    (d.x * d.x + d.z * d.z).sqrt()
}

/// Inputs to the relevance decision for one client.
pub(crate) struct Viewer<'a> {
    pub(crate) player: PlayerId,
    /// `(position, radius)` of the client's interest sources.
    pub(crate) sources: &'a [(Vec3, f32)],
    /// Currently visible `NetId`s (hysteresis).
    pub(crate) current: &'a dyn Fn(NetId) -> bool,
}

/// Compute the relevant set (ascending `NetId`s) for one viewer.
pub(crate) fn relevant_set(
    cache: &ChangeCache,
    policy: InterestPolicy,
    viewer: &Viewer<'_>,
    overrides: &FastMap<(PlayerId, Entity), Visibility>,
    rules: &[Box<dyn VisibilityRule>],
    world: &World,
) -> Vec<NetId> {
    // Candidates for spatial policies come from the grid; everything else is room-wide.
    let mut spatial_hits: Option<Vec<u32>> = None;
    if let InterestPolicy::Spatial { hysteresis, .. } = policy {
        let mut hits = Vec::new();
        for (p, r) in viewer.sources {
            cache.grid.query(*p, r * hysteresis.max(1.0), &mut hits);
        }
        hits.sort_unstable();
        hits.dedup();
        spatial_hits = Some(hits);
    }
    let mut root_decision: FastMap<NetId, bool> = FastMap::default();
    let mut out = Vec::new();
    for rec in &cache.records {
        let root = cache.get(rec.root).unwrap_or(rec);
        let visible = match root_decision.get(&root.net_id) {
            Some(v) => *v,
            None => {
                let v = decide(root, policy, viewer, overrides, rules, world, spatial_hits.as_deref());
                root_decision.insert(root.net_id, v);
                v
            }
        };
        // A child's own override still applies (hide a weapon, show a marker).
        let visible = match overrides.get(&(viewer.player, rec.entity)) {
            Some(Visibility::Visible) => true,
            Some(Visibility::Hidden) => false,
            _ => visible || rec.is_owned_by(viewer.player),
        };
        if visible {
            out.push(rec.net_id);
        }
    }
    out
}

fn decide(
    rec: &EntityRecord,
    policy: InterestPolicy,
    viewer: &Viewer<'_>,
    overrides: &FastMap<(PlayerId, Entity), Visibility>,
    rules: &[Box<dyn VisibilityRule>],
    world: &World,
    spatial_hits: Option<&[u32]>,
) -> bool {
    if rec.is_owned_by(viewer.player) {
        return true;
    }
    match overrides.get(&(viewer.player, rec.entity)) {
        Some(Visibility::Visible) => return true,
        Some(Visibility::Hidden) => return false,
        _ => {}
    }
    let (default_radius, hysteresis) = match policy {
        InterestPolicy::RoomWide => (None, 1.0),
        InterestPolicy::Spatial { radius, hysteresis, .. } => (Some(radius), hysteresis.max(1.0)),
    };
    // `None` = no distance test (room-wide); `Some(None)` = the viewer's radius.
    let spatial: Option<Option<f32>> = match rec.relevance {
        Some(NetRelevance::Global) => return true,
        Some(NetRelevance::OwnerOnly) | Some(NetRelevance::Manual) => return false,
        Some(NetRelevance::Spatial { radius: Some(r) }) => Some(Some(r)),
        Some(NetRelevance::Spatial { radius: None }) | None => default_radius.map(|_| None),
    };
    if !rules.iter().all(|r| r.visible(world, viewer.player, rec.entity)) {
        return false;
    }
    let Some(override_radius) = spatial else {
        return true; // room-wide
    };
    let Some(pos) = rec.pos else {
        return true; // unpositioned entities are room-wide
    };
    if override_radius.is_none()
        && let Some(hits) = spatial_hits
        && hits.binary_search(&rec.net_id.0).is_err()
    {
        return false;
    }
    let inside = (viewer.current)(rec.net_id);
    viewer.sources.iter().any(|(p, src_radius)| {
        let r = override_radius.unwrap_or(if *src_radius > 0.0 {
            *src_radius
        } else {
            default_radius.unwrap_or(0.0)
        });
        let limit = if inside { r * hysteresis } else { r };
        dist_xz(*p, pos) <= limit
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_update_and_query() {
        let mut g = SpatialGrid::new(10.0);
        g.update(1, None, Some(Vec3::new(1.0, 0.0, 1.0)));
        g.update(2, None, Some(Vec3::new(25.0, 0.0, 1.0)));
        g.update(3, None, Some(Vec3::new(-35.0, 5.0, -35.0)));
        let mut out = Vec::new();
        g.query(Vec3::ZERO, 5.0, &mut out);
        assert_eq!(out, vec![1]);
        out.clear();
        // Cells x ∈ [1, 3], z ∈ [-1, 0]: record 1 sits in cell (0, 0), outside.
        g.query(Vec3::new(20.0, 0.0, 0.0), 10.0, &mut out);
        assert_eq!(out, vec![2]);
        out.clear();
        g.query(Vec3::new(12.0, 0.0, 0.0), 11.0, &mut out);
        assert_eq!(out, vec![1, 2]);
        g.update(2, Some(Vec3::new(25.0, 0.0, 1.0)), Some(Vec3::new(-31.0, 0.0, -31.0)));
        out.clear();
        g.query(Vec3::new(-33.0, 0.0, -33.0), 3.0, &mut out);
        assert_eq!(out, vec![2, 3]);
        g.update(3, Some(Vec3::new(-35.0, 5.0, -35.0)), None);
        out.clear();
        g.query(Vec3::new(-33.0, 0.0, -33.0), 3.0, &mut out);
        assert_eq!(out, vec![2]);
        // Huge radius falls back to scanning cells.
        out.clear();
        g.query(Vec3::ZERO, 1e6, &mut out);
        assert_eq!(out, vec![1, 2]);
    }
}
