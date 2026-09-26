//! `.pfx.ron` effects through the asset server (feature `assets`): [`ParticleEffectLoader`]
//! decodes them, [`crate::ParticleEffects::add_asset`] registers one (its handle is valid at
//! once; emitters wait until it has loaded), and a system keeps the registered effects in step
//! with their assets — hot reloads apply like `ParticleEffects::set` (render parameters and
//! curves next frame, spawn parameters to new particles, a changed structure restarts running
//! emitters while their old particles drain).

use crate::effect::ParticleEffect;
use crate::registry::ParticleEffects;
use space3d_assets::{AssetError, AssetEvent, AssetLoader, AssetServer, Assets, LoadContext, LoadState};
use space3d_core::{EventReader, Events, SystemCtx};

/// Decodes `.pfx.ron` files into [`ParticleEffect`]s (registered by `ParticlesPlugin` when an
/// `AssetServer` exists).
#[derive(Clone, Copy, Debug, Default)]
pub struct ParticleEffectLoader;

impl AssetLoader for ParticleEffectLoader {
    type Asset = ParticleEffect;

    fn extensions(&self) -> &[&str] {
        &["pfx.ron"]
    }

    fn load(&self, bytes: &[u8], ctx: &LoadContext) -> Result<ParticleEffect, AssetError> {
        let decode = |message: String| AssetError::Decode {
            path: ctx.path().to_string(),
            message,
        };
        let src = std::str::from_utf8(bytes).map_err(|e| decode(e.to_string()))?;
        ParticleEffect::from_ron(src).map_err(|e| decode(e.to_string()))
    }
}

/// Event cursor of [`sync_effect_assets`].
#[derive(Default)]
pub(crate) struct EffectAssetEvents(EventReader);

/// Apply loaded and reloaded effect assets to their registered effects (`Stage::Update`).
/// Allocates nothing without effect assets.
pub(crate) fn sync_effect_assets(ctx: &mut SystemCtx) {
    let res = &mut *ctx.resources;
    if res.get::<ParticleEffects>().is_none_or(|e| e.asset_links.is_empty())
        || !res.contains::<Assets<ParticleEffect>>()
    {
        return;
    }
    res.init::<EffectAssetEvents>();
    res.scope::<ParticleEffects, _>(|res, effects| {
        res.scope::<EffectAssetEvents, _>(|res, reader| {
            let Some(assets) = res.get::<Assets<ParticleEffect>>() else {
                return;
            };
            // Reloads (and first loads) announced since the last run.
            if let Some(events) = res.get::<Events<AssetEvent<ParticleEffect>>>() {
                for ev in reader.0.read(events) {
                    if !matches!(ev, AssetEvent::Created { .. } | AssetEvent::Modified { .. }) {
                        continue;
                    }
                    let id = ev.id();
                    for i in 0..effects.asset_links.len() {
                        let (asset, h) = &effects.asset_links[i];
                        if asset.id() == id
                            && let Some(data) = assets.get(id)
                        {
                            let h = *h;
                            apply(effects, h, data);
                        }
                    }
                }
            }
            // Effects still waiting: their asset may have loaded before they were registered,
            // or failed.
            let server = res.get::<AssetServer>();
            for i in 0..effects.asset_links.len() {
                let (id, h) = (effects.asset_links[i].0.id(), effects.asset_links[i].1);
                if !effects.is_pending(h) {
                    continue;
                }
                if let Some(data) = assets.get(id) {
                    apply(effects, h, data);
                } else if let Some(LoadState::Failed(err)) = server.map(|s| s.load_state(id)) {
                    tracing::warn!("particle effect asset failed to load ({err}): its emitters play nothing");
                    effects.fail_pending(h);
                }
            }
        });
    });
}

fn apply(effects: &mut ParticleEffects, h: crate::EffectHandle, data: &ParticleEffect) {
    if let Err(err) = effects.set(h, data.clone()) {
        tracing::warn!(effect = %data.name, "particle effect asset rejected: {err}");
    }
}
