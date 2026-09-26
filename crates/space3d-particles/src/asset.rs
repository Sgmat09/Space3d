//! `.pfx.ron` effects through the asset server (feature `assets`): [`ParticleEffectLoader`]
//! decodes them, [`crate::ParticleEffects::add_asset`] registers one (its handle is valid at
//! once; emitters wait until it has loaded), and a system keeps the registered effects in step
//! with their assets — hot reloads apply like `ParticleEffects::set` (render parameters and
//! curves next frame, spawn parameters to new particles, a changed structure restarts running
//! emitters while their old particles drain).

use crate::effect::{ParticleEffect, SpriteShape, SpriteTarget, SpriteTexture};
use crate::registry::{EffectHandle, ParticleEffects};
use space3d_assets::{AssetError, AssetEvent, AssetLoader, AssetServer, Assets, Handle, LoadContext, LoadState};
use space3d_core::{EventReader, Events, ImageData, SystemCtx};
use space3d_render::{RenderAssets, TextureHandle};

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

/// A texture an effect names by asset path ([`SpriteTexture::Path`]): loading, uploaded, or
/// failed.
enum PathTexture {
    Loading(Handle<ImageData>),
    Ready(TextureHandle),
    Failed,
}

/// Textures of `SpriteTexture::Path`s, by path and color space (distortion layers read their
/// texture as linear offsets).
#[derive(Default)]
pub(crate) struct EffectTextures {
    textures: Vec<((String, bool), PathTexture)>,
    /// `ParticleEffects::version` when every path was last resolved.
    seen: Option<u64>,
}

impl EffectTextures {
    fn loading(&self) -> bool {
        self.textures.iter().any(|(_, t)| matches!(t, PathTexture::Loading(_)))
    }
}

/// Every `SpriteTexture::Path` of `effect`: `(path, linear)` — sprite layers (linear for
/// distortion layers), ribbons and beams.
fn texture_paths(effect: &ParticleEffect) -> impl Iterator<Item = (&str, bool)> {
    let layers = effect.layers.iter().filter_map(|l| match &l.render.shape {
        SpriteShape::Texture(SpriteTexture::Path(p)) => {
            Some((p.as_str(), matches!(l.render.target, SpriteTarget::Distortion { .. })))
        }
        _ => None,
    });
    let ribbons = effect.ribbons.iter().filter_map(|r| match &r.texture {
        Some(SpriteTexture::Path(p)) => Some((p.as_str(), false)),
        _ => None,
    });
    let beams = effect.beams.iter().filter_map(|b| match &b.texture {
        Some(SpriteTexture::Path(p)) => Some((p.as_str(), false)),
        _ => None,
    });
    layers.chain(ribbons).chain(beams)
}

/// Resolve the textures effects name by asset path (`Stage::Update`, feature `assets`): each
/// path is loaded through the asset server (`ImageData`, the server's root or `source://path`),
/// uploaded to `RenderAssets` (sRGB; linear for distortion layers) and swapped into the effect
/// as `SpriteTexture::Handle` (like `ParticleEffects::set`: the next frame draws it). Until then
/// the layer draws untextured; a path that fails to load is warned once and stays untextured.
/// Hot reloads of a `.pfx.ron` that bring the path back resolve it again from the cache. Does
/// nothing (and allocates nothing) once every path is resolved and the effects are unchanged.
pub(crate) fn resolve_texture_paths(ctx: &mut SystemCtx) {
    let res = &mut *ctx.resources;
    let Some(version) = res.get::<ParticleEffects>().map(ParticleEffects::version) else {
        return;
    };
    if !res.contains::<AssetServer>() || !res.contains::<RenderAssets>() {
        return;
    }
    let textures = res.init::<EffectTextures>();
    if textures.seen == Some(version) && !textures.loading() {
        return;
    }
    res.scope::<ParticleEffects, _>(|res, effects| {
        res.scope::<EffectTextures, _>(|res, cache| {
            // 1. Start loading new paths.
            let mut rewrite: Vec<EffectHandle> = Vec::new();
            for (h, effect) in effects.iter() {
                let mut any = false;
                for (path, linear) in texture_paths(effect) {
                    any = true;
                    let key = (path.to_string(), linear);
                    if !cache.textures.iter().any(|(k, _)| *k == key) {
                        let Some(server) = res.get_mut::<AssetServer>() else {
                            return;
                        };
                        let handle: Handle<ImageData> = server.load(path);
                        cache.textures.push((key, PathTexture::Loading(handle)));
                    }
                }
                if any {
                    rewrite.push(h);
                }
            }
            // 2. Upload the loaded ones.
            for (key, tex) in &mut cache.textures {
                let PathTexture::Loading(handle) = tex else {
                    continue;
                };
                let loaded = res.get::<Assets<ImageData>>().and_then(|a| a.get(handle.id())).cloned();
                if let Some(image) = loaded {
                    let Some(assets) = res.get_mut::<RenderAssets>() else {
                        return;
                    };
                    let t = if key.1 {
                        assets.add_image_linear(image)
                    } else {
                        assets.add_image(image)
                    };
                    *tex = PathTexture::Ready(t);
                } else if let Some(LoadState::Failed(err)) = res.get::<AssetServer>().map(|s| s.load_state(handle.id())) {
                    tracing::warn!(path = %key.0, "particle texture failed to load ({err}): the layer draws untextured");
                    *tex = PathTexture::Failed;
                }
            }
            // 3. Swap the uploaded textures into their effects.
            let ready = |path: &str, linear: bool| {
                cache.textures.iter().find_map(|((p, l), t)| match t {
                    PathTexture::Ready(h) if p == path && *l == linear => Some(*h),
                    _ => None,
                })
            };
            for h in rewrite {
                let Some(effect) = effects.get(h) else {
                    continue;
                };
                if !texture_paths(effect).any(|(p, l)| ready(p, l).is_some()) {
                    continue;
                }
                let mut resolved = effect.clone();
                for layer in &mut resolved.layers {
                    let linear = matches!(layer.render.target, SpriteTarget::Distortion { .. });
                    if let SpriteShape::Texture(SpriteTexture::Path(p)) = &layer.render.shape
                        && let Some(t) = ready(p, linear)
                    {
                        layer.render.shape = SpriteShape::Texture(SpriteTexture::Handle(t));
                    }
                }
                for tex in resolved
                    .ribbons
                    .iter_mut()
                    .map(|r| &mut r.texture)
                    .chain(resolved.beams.iter_mut().map(|b| &mut b.texture))
                {
                    if let Some(SpriteTexture::Path(p)) = tex
                        && let Some(t) = ready(p, false)
                    {
                        *tex = Some(SpriteTexture::Handle(t));
                    }
                }
                if let Err(err) = effects.set(h, resolved) {
                    tracing::warn!("particle effect with resolved textures rejected: {err}");
                }
            }
            cache.seen = Some(effects.version());
        });
    });
}
