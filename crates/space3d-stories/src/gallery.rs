//! Batch screenshotting into a static gallery: PNGs + `index.html` + `manifest.json`.

use crate::runner::{StoryError, StoryRunOptions, render_story};
use crate::{Knob, KnobKind, KnobValue, KnobValues, Story, Storybook};
use serde::{Deserialize, Serialize};
use space3d_test::report::{HTML_STYLE, html_escape};
use space3d_test::screenshot::sanitize_name;
use std::path::{Path, PathBuf};

/// What happened to one story in [`screenshot_all`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoryStatus {
    /// PNG written.
    Rendered,
    /// Not captured (`no-screenshot` tag, or no GPU).
    Skipped,
    /// The story panicked or could not be captured.
    Failed,
}

impl StoryStatus {
    fn as_str(self) -> &'static str {
        match self {
            StoryStatus::Rendered => "rendered",
            StoryStatus::Skipped => "skipped",
            StoryStatus::Failed => "failed",
        }
    }
}

/// One gallery entry (also an element of `manifest.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoryResult {
    /// Story id.
    pub id: String,
    /// Story group.
    pub group: String,
    /// Story name.
    pub name: String,
    /// Story description.
    pub description: String,
    /// Story tags.
    pub tags: Vec<String>,
    /// Capture size.
    pub size: (u32, u32),
    /// Ticks simulated before capture.
    pub ticks: u64,
    /// Knobs (with their defaults, which the capture used).
    pub knobs: Vec<Knob>,
    /// Outcome.
    pub status: StoryStatus,
    /// Skip reason or error.
    pub message: Option<String>,
    /// PNG path relative to the gallery directory (`/`-separated), when rendered.
    pub image: Option<String>,
    /// Render backend used.
    pub backend: Option<String>,
    /// Draw calls of the captured frame.
    pub draw_calls: Option<u32>,
    /// Wall time for setup + simulation + capture, in microseconds.
    pub duration_us: u64,
}

impl StoryResult {
    fn new(story: &Story, status: StoryStatus, message: Option<String>) -> Self {
        Self {
            id: story.id.clone(),
            group: story.group.clone(),
            name: story.name.clone(),
            description: story.description.clone(),
            tags: story.tags.clone(),
            size: story.size,
            ticks: story.ticks,
            knobs: story.knobs.clone(),
            status,
            message,
            image: None,
            backend: None,
            draw_calls: None,
            duration_us: 0,
        }
    }

    /// Absolute path of the PNG inside `gallery_dir`.
    pub fn image_path(&self, gallery_dir: impl AsRef<Path>) -> Option<PathBuf> {
        self.image.as_ref().map(|rel| gallery_dir.as_ref().join(rel))
    }
}

/// Contents of `manifest.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GalleryManifest {
    /// Generator name and version.
    pub generator: String,
    /// Adapter name, when a GPU was available.
    pub gpu: Option<String>,
    /// Backend name, when a GPU was available.
    pub backend: Option<String>,
    /// Filter the gallery was generated with.
    pub filter: Option<String>,
    /// Every selected story, in storybook order.
    pub stories: Vec<StoryResult>,
}

impl GalleryManifest {
    /// Read `<gallery_dir>/manifest.json`.
    pub fn load(gallery_dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(gallery_dir.as_ref().join("manifest.json"))?;
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }
}

/// Render every story selected by `filter` (see [`Storybook::select`]) with default knobs
/// and write a static gallery into `out_dir`:
///
/// * `images/<story id>.png` — one PNG per rendered story (id segments become directories);
/// * `index.html` — grouped by story group, each card listing its knob defaults;
/// * `manifest.json` — [`GalleryManifest`] for tools.
///
/// Stories tagged `no-screenshot` are listed but not rendered. Without a GPU every story is
/// [`StoryStatus::Skipped`] and the gallery is still written. Broken stories are reported as
/// [`StoryStatus::Failed`] without stopping the batch. Only gallery I/O errors are returned.
pub fn screenshot_all(
    book: &Storybook,
    out_dir: impl AsRef<Path>,
    filter: Option<&str>,
) -> std::io::Result<Vec<StoryResult>> {
    let out_dir = out_dir.as_ref();
    std::fs::create_dir_all(out_dir)?;
    let mut results = Vec::new();
    for story in book.select(filter) {
        let result = if !story.screenshot_enabled() {
            StoryResult::new(story, StoryStatus::Skipped, Some("tagged no-screenshot".to_string()))
        } else {
            match render_story(story, &KnobValues::default(), &StoryRunOptions::default()) {
                Ok(render) => {
                    let rel = format!("images/{}.png", sanitize_name(&story.id));
                    space3d_test::save_png(out_dir.join(&rel), &render.image).map_err(std::io::Error::other)?;
                    let mut r = StoryResult::new(story, StoryStatus::Rendered, None);
                    r.image = Some(rel);
                    r.backend = render.backend;
                    r.draw_calls = render.stats.map(|s| s.draw_calls);
                    r.duration_us = render.duration.as_micros() as u64;
                    r
                }
                Err(e @ StoryError::NoRenderer(_)) => {
                    StoryResult::new(story, StoryStatus::Skipped, Some(e.to_string()))
                }
                Err(e) => StoryResult::new(story, StoryStatus::Failed, Some(e.to_string())),
            }
        };
        tracing::info!(story = %result.id, status = result.status.as_str(), "storybook screenshot");
        results.push(result);
    }
    let gpu = space3d_test::gpu_info().ok();
    let manifest = GalleryManifest {
        generator: format!("space3d-stories {}", env!("CARGO_PKG_VERSION")),
        gpu: gpu.map(|g| g.name.clone()),
        backend: gpu.map(|g| g.backend.clone()),
        filter: filter.map(str::to_string),
        stories: results,
    };
    let json = serde_json::to_string_pretty(&manifest).map_err(std::io::Error::other)?;
    std::fs::write(out_dir.join("manifest.json"), json)?;
    std::fs::write(out_dir.join("index.html"), render_gallery_html(&manifest))?;
    Ok(manifest.stories)
}

fn slug(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("g-{}", s.trim_matches('-'))
}

fn knob_value_html(knob: &Knob) -> String {
    match (&knob.default, &knob.kind) {
        (KnobValue::Color(c), _) => {
            let srgb = c.map(|x| (space3d_render::linear_to_srgb(x.clamp(0.0, 1.0)) * 255.0).round() as u8);
            format!(
                "<span class=\"swatch\" style=\"background:rgba({},{},{},{:.3})\"></span>{}",
                srgb[0],
                srgb[1],
                srgb[2],
                c[3].clamp(0.0, 1.0),
                html_escape(&knob.default.to_string())
            )
        }
        (_, KnobKind::Choice { options }) => {
            let current = knob.format_value(&knob.default);
            let all = options.join(" | ");
            format!(
                "{} <span class=\"muted\">({})</span>",
                html_escape(&current),
                html_escape(&all)
            )
        }
        _ => html_escape(&knob.format_value(&knob.default)),
    }
}

fn knob_range_html(kind: &KnobKind) -> String {
    match kind {
        KnobKind::Float { min, max } => format!("{min} – {max}"),
        KnobKind::Int { min, max } => format!("{min} – {max}"),
        _ => String::new(),
    }
}

const GALLERY_STYLE: &str = r#"
.story img, .placeholder { width:100%; }
.placeholder { aspect-ratio:4/3; display:flex; align-items:center; justify-content:center; text-align:center;
  border:1px dashed var(--line); border-radius:6px; color:var(--muted); padding:12px; }
.story h3 { font-size:15px; margin:10px 0 2px; word-break:break-word; }
.story p { margin:6px 0; }
.meta { font-size:12px; color:var(--muted); }
.tag { display:inline-block; font-size:11px; border:1px solid var(--line); border-radius:999px; padding:0 7px; margin:2px 4px 0 0; }
.swatch { display:inline-block; width:12px; height:12px; border-radius:3px; border:1px solid var(--line); margin-right:6px; vertical-align:-2px; }
.knobs { width:100%; table-layout:fixed; } .knobs td, .knobs th { overflow-wrap:anywhere; vertical-align:top; }
.knobs th { font-weight:600; width:32%; }
"#;

/// Static HTML gallery for a manifest (what [`screenshot_all`] writes to `index.html`).
pub fn render_gallery_html(manifest: &GalleryManifest) -> String {
    let mut groups: Vec<&str> = Vec::new();
    for s in &manifest.stories {
        if !groups.contains(&s.group.as_str()) {
            groups.push(&s.group);
        }
    }
    let count = |st: StoryStatus| manifest.stories.iter().filter(|s| s.status == st).count();
    let mut h = String::new();
    h.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>Space3d storybook</title><style>");
    h.push_str(HTML_STYLE);
    h.push_str(GALLERY_STYLE);
    h.push_str("</style></head><body><main><h1>Space3d storybook</h1>");
    h.push_str(&format!(
        "<div class=\"muted\">{} stories · {} rendered · {} skipped · {} failed{}{}</div>",
        manifest.stories.len(),
        count(StoryStatus::Rendered),
        count(StoryStatus::Skipped),
        count(StoryStatus::Failed),
        manifest
            .gpu
            .as_ref()
            .map(|g| format!(
                " · {} ({})",
                html_escape(g),
                html_escape(manifest.backend.as_deref().unwrap_or("?"))
            ))
            .unwrap_or_default(),
        manifest
            .filter
            .as_ref()
            .map(|f| format!(" · filter “{}”", html_escape(f)))
            .unwrap_or_default(),
    ));
    if groups.len() > 1 {
        h.push_str("<nav class=\"summary\">");
        for g in &groups {
            let label = if g.is_empty() { "(ungrouped)" } else { g };
            h.push_str(&format!("<a href=\"#{}\">{}</a>", slug(g), html_escape(label)));
        }
        h.push_str("</nav>");
    }
    for g in &groups {
        let label = if g.is_empty() { "(ungrouped)" } else { g };
        h.push_str(&format!(
            "<h2 id=\"{}\">{}</h2><div class=\"grid\">",
            slug(g),
            html_escape(label)
        ));
        for s in manifest.stories.iter().filter(|s| s.group == *g) {
            h.push_str(&format!(
                "<section class=\"card story\" id=\"{}\">",
                html_escape(&sanitize_name(&s.id))
            ));
            match &s.image {
                Some(src) => h.push_str(&format!(
                    "<a href=\"{src}\"><img loading=\"lazy\" src=\"{src}\" width=\"{w}\" height=\"{hh}\" alt=\"{alt}\"></a>",
                    src = html_escape(src),
                    w = s.size.0,
                    hh = s.size.1,
                    alt = html_escape(&s.id)
                )),
                None => h.push_str(&format!(
                    "<div class=\"placeholder\"><span><span class=\"badge {st}\">{st}</span><br>{msg}</span></div>",
                    st = s.status.as_str(),
                    msg = html_escape(s.message.as_deref().unwrap_or(""))
                )),
            }
            h.push_str(&format!(
                "<h3>{}</h3><div class=\"meta\">{} · {}×{} · {} tick{}{}{}</div>",
                html_escape(&s.name),
                html_escape(&s.id),
                s.size.0,
                s.size.1,
                s.ticks,
                if s.ticks == 1 { "" } else { "s" },
                s.draw_calls
                    .map(|d| format!(" · {d} draw call{}", if d == 1 { "" } else { "s" }))
                    .unwrap_or_default(),
                if s.duration_us > 0 {
                    format!(" · {:.1} ms", s.duration_us as f64 / 1000.0)
                } else {
                    String::new()
                }
            ));
            if s.status == StoryStatus::Failed
                && let Some(m) = &s.message
            {
                h.push_str(&format!("<pre>{}</pre>", html_escape(m)));
            }
            if !s.description.is_empty() {
                h.push_str(&format!("<p>{}</p>", html_escape(&s.description)));
            }
            if !s.tags.is_empty() {
                h.push_str("<div>");
                for t in &s.tags {
                    h.push_str(&format!("<span class=\"tag\">{}</span>", html_escape(t)));
                }
                h.push_str("</div>");
            }
            if !s.knobs.is_empty() {
                h.push_str("<table class=\"knobs\"><tr><th>knob</th><th>default</th></tr>");
                for k in &s.knobs {
                    let range = knob_range_html(&k.kind);
                    h.push_str(&format!(
                        "<tr><th>{} <span class=\"muted\">{}</span></th><td>{}{}</td></tr>",
                        html_escape(&k.name),
                        k.kind.type_name(),
                        knob_value_html(k),
                        if range.is_empty() {
                            String::new()
                        } else {
                            format!(" <span class=\"muted\">[{}]</span>", html_escape(&range))
                        }
                    ));
                }
                h.push_str("</table>");
            }
            h.push_str("</section>");
        }
        h.push_str("</div>");
    }
    h.push_str("</main></body></html>\n");
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_groups_escapes_and_lists_knobs() {
        let story = Story::new("UI/<Button>", |_, _| {})
            .description("a & b")
            .knob_color("tint", [1.0, 0.0, 0.0, 1.0])
            .knob_choice("mode", &["Fast", "Pretty"], 1)
            .knob_f32("scale", 1.0, 0.5..=2.0)
            .tag("ui");
        let other = Story::new("Scene/Cube", |_, _| {});
        let mut rendered = StoryResult::new(&story, StoryStatus::Rendered, None);
        rendered.image = Some("images/UI/_Button_.png".into());
        let manifest = GalleryManifest {
            generator: "test".into(),
            gpu: Some("Fake GPU".into()),
            backend: Some("metal".into()),
            filter: None,
            stories: vec![
                rendered,
                StoryResult::new(&other, StoryStatus::Failed, Some("story panicked: <oops>".into())),
            ],
        };
        let html = render_gallery_html(&manifest);
        assert!(html.contains("&lt;Button&gt;") && html.contains("a &amp; b"), "{html}");
        assert!(html.contains("id=\"g-ui\"") && html.contains("id=\"g-scene\""));
        assert!(html.contains("background:rgba(255,0,0,1.000)"));
        assert!(html.contains("Pretty <span class=\"muted\">(Fast | Pretty)</span>"));
        assert!(html.contains("[0.5 – 2]"));
        assert!(html.contains("src=\"images/UI/_Button_.png\""));
        assert!(html.contains("&lt;oops&gt;"));
        assert!(html.contains("2 stories · 1 rendered · 0 skipped · 1 failed · Fake GPU (metal)"));
        let json = serde_json::to_string(&manifest).unwrap();
        assert_eq!(serde_json::from_str::<GalleryManifest>(&json).unwrap(), manifest);
    }
}
