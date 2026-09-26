//! Kinsoku (禁則処理): Chinese and Japanese text wraps by the line breaking rules of Space3d's
//! patched epaint (`third_party/epaint`, see `third_party/README.md` and `docs/ui.md`).
//!
//! Real paragraphs are laid out by egui with a CJK font at many widths and three sizes, and a
//! golden screenshot story (`UI/Text/Kinsoku`) draws them wrapped. The font is a subset of
//! Noto Sans JP (SIL OFL 1.1, `tests/fonts/README.md`) with the characters of these texts:
//! Japanese and Traditional Chinese render with real glyphs; characters only Simplified
//! Chinese uses are missing from it and render as a replacement glyph, which the rules don't
//! look at.
//!
//! The violations are checked against this file's own lists, not epaint's, so the test fails
//! whenever the kinsoku patch is lost (upstream egui fails it at most widths).

use space3d_core::prelude::*;
use space3d_render::Camera3d;
use space3d_stories::{Story, Storybook, storybook_tests};
use space3d_ui::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId, RichText};
use space3d_ui::{EguiContext, UiAppExt, UiPlugin};
use std::path::Path;
use std::sync::Arc;

/// Subset of Noto Sans JP 1.004 with the characters of this file (tests/fonts/README.md).
const CJK_FONT: &[u8] = include_bytes!("fonts/Space3dTestCJK-Regular.otf");

const JAPANESE: &[&str] = &[
    "まっさらなスタート：育成のみで、パイロット同士は攻撃できません。「重要」（注意）次へ！本当に？はい、そうです。",
    "ロッカーのキャッシュを確認してください。人々は時々、こゝろの中で名を呼んだ。コーヒー――それとも……紅茶？ェェッ！",
    "シールドボーナス、Shield Wall ブースター（+25%）、Shield Boost バフで合計が増えます。価格は￥1,200（税込）です。",
];

const TRADITIONAL_CHINESE: &[&str] = &[
    "星門還差三個部件：星門完成後掃描即停止，只按實際進行的掃描收費。你確定嗎？是的！（請注意）繼續吧，好的；完成。",
    "連接中斷……正在重新連接，請稍候。伺服器沒有回應……請確認網路。他說：「你好！」——然後就走了。",
    "帳號的其餘部分都會保留：信用點、Thulium、等級、榮譽、重置點數、永久加成、Skylab 和戰隊。",
];

/// From the SpaceCorps zh-CN catalog; some of these characters are not in the test font.
const SIMPLIFIED_CHINESE: &[&str] = &[
    "星门还差三个部件：星门完成后扫描即停止，只按实际进行的扫描收费。你确定吗？是的！（请注意）继续吧，好的；完成。",
    "按「确认」锁定（冻结）传送缓存：锁定后无法放入或取出物品，【注意】解锁后可再次修改。",
    "齐射随机造成该数值的 80–100%，可能暴击，并乘以弹药倍率。机库显示的是未计增益时 x1 弹药齐射的伤害。",
    "欢迎来到 SpaceCorps。本周末双倍经验值活动进行中。4-2 星区目前海盗活动频繁，请谨慎前往。",
];

/// May not start a line (JIS X 4051: closing brackets and quotes, punctuation, small kana,
/// the prolonged sound mark, iteration marks, postfixed signs, ellipses).
const NOT_FIRST: &str =
    "）］｝〕〉》」』】〙〗〟｠’”、。，．：；？！・ーぁぃぅぇぉっゃゅょゎァィゥェォッャュョヮヵヶ々ゝゞヽヾ〜～％…‥";
/// May not end a line (opening brackets and quotes, prefixed signs).
const NOT_LAST: &str = "（［｛〔〈《「『【〘〖〝｟‘“￥＄";
/// Pairs that stay together.
const INSEPARABLE: &[&str] = &["——", "――", "……", "‥‥"];

/// egui's default fonts with the CJK subset right after the Latin one, before the emoji and
/// icon fonts (which would otherwise draw the full-width ！？（）), as a game adds a CJK font.
fn cjk_fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".into(), Arc::new(FontData::from_static(CJK_FONT)));
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let faces = fonts.families.entry(family).or_default();
        faces.insert(faces.len().min(1), "cjk".into());
    }
    fonts
}

fn cjk_context() -> egui::Context {
    let ctx = egui::Context::default();
    ctx.set_fonts(cjk_fonts());
    // Fonts are installed at the start of a pass; nothing uploads its font atlas here.
    ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
    ctx
}

fn is_breakable(before: char, after: char) -> bool {
    let pair: String = [before, after].iter().collect();
    before.is_whitespace()
        || !(NOT_FIRST.contains(after) || NOT_LAST.contains(before) || INSEPARABLE.contains(&pair.as_str()))
}

/// The widest run of `text` that the rules keep on one line ("茶？ェェッ！", "%）、"): below
/// that width a break inside it cannot be avoided.
fn widest_unit(fonts: &mut egui::epaint::text::FontsView<'_>, text: &str, font: &FontId) -> f32 {
    let chars: Vec<char> = text.chars().collect();
    let mut widest = 0.0_f32;
    let mut start = 0;
    for i in 0..chars.len() {
        if chars.get(i + 1).is_none_or(|&next| is_breakable(chars[i], next)) {
            let unit: String = chars[start..=i].iter().collect();
            widest = widest.max(fonts.layout_no_wrap(unit, font.clone(), Color32::WHITE).size().x);
            start = i + 1;
        }
    }
    widest
}

fn rows(fonts: &mut egui::epaint::text::FontsView<'_>, text: &str, font: &FontId, width: f32) -> Vec<String> {
    let galley = fonts.layout(text.to_owned(), font.clone(), Color32::WHITE, width);
    galley.rows.iter().map(|row| row.text()).collect()
}

/// Japanese, Traditional and Simplified Chinese paragraphs, with Latin words and numbers, at 13,
/// 16 and 20 pt and every fourth width from the widest unbreakable run to 480 pt: no line
/// starts with a closing mark, none ends with an opening one, "——" and "……" stay whole, and no
/// character is lost.
#[test]
fn cjk_paragraphs_wrap_by_kinsoku_at_every_width() {
    let ctx = cjk_context();
    let mut bad = Vec::new();
    let mut layouts = 0;
    ctx.fonts_mut(|fonts| {
        for text in JAPANESE.iter().chain(TRADITIONAL_CHINESE).chain(SIMPLIFIED_CHINESE) {
            for size in [13.0, 16.0, 20.0] {
                let font = FontId::proportional(size);
                let min_width = widest_unit(fonts, text, &font).ceil() as u32 + 2;
                assert!(min_width < 8 * size as u32, "{size} pt: {min_width} for {text}");
                let narrow = rows(fonts, text, &font, min_width as f32);
                assert!(narrow.len() >= 4, "{size} pt at {min_width}: {narrow:?}");
                for width in (min_width..=480).step_by(4) {
                    let rows = rows(fonts, text, &font, width as f32);
                    layouts += 1;
                    assert_eq!(rows.concat(), *text, "{size} pt at {width}");
                    for (i, pair) in rows.windows(2).enumerate() {
                        let (Some(last), Some(first)) = (pair[0].chars().last(), pair[1].chars().next()) else {
                            continue;
                        };
                        if !is_breakable(last, first) {
                            bad.push(format!(
                                "{size} pt, {width}: {last:?} | {first:?} after row {i}: {rows:?}"
                            ));
                        }
                    }
                }
            }
        }
    });
    assert!(layouts > 2000, "{layouts} layouts");
    assert!(bad.is_empty(), "{} kinsoku violations:\n{}", bad.len(), bad.join("\n"));
}

/// The test font has every glyph of the Japanese and Traditional Chinese paragraphs (the story
/// below shows real glyphs, not replacement boxes).
#[test]
fn the_test_font_covers_japanese_and_traditional_chinese() {
    let ctx = cjk_context();
    ctx.fonts_mut(|fonts| {
        for text in JAPANESE.iter().chain(TRADITIONAL_CHINESE) {
            let missing: String = text
                .chars()
                .filter(|&c| !fonts.has_glyph(&FontId::proportional(16.0), c))
                .collect();
            assert!(missing.is_empty(), "missing {missing:?} in {text}");
        }
    });
}

/// Guard against losing the patch silently: when the root `[patch.crates-io]` stops applying
/// (egui moved to a version the vendored copy is not, or the entry was dropped) cargo only
/// warns and builds crates.io's epaint, without kinsoku. `Cargo.lock` records a path package
/// without a `source`, and an unused patch under `[[patch.unused]]`.
/// Fix: `python3 scripts/update-epaint.py` (third_party/README.md).
#[test]
fn epaint_resolves_to_the_vendored_kinsoku_copy() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let lock: toml::Table = std::fs::read_to_string(root.join("Cargo.lock"))
        .expect("Cargo.lock")
        .parse()
        .expect("Cargo.lock is TOML");
    let packages = lock["package"].as_array().expect("[[package]]");
    let find = |name: &str| -> Vec<&toml::Table> {
        packages
            .iter()
            .filter_map(toml::Value::as_table)
            .filter(|p| p["name"].as_str() == Some(name))
            .collect()
    };
    let epaint = find("epaint");
    let egui = find("egui");
    assert_eq!(epaint.len(), 1, "one epaint in Cargo.lock: {epaint:?}");
    assert_eq!(egui.len(), 1, "one egui in Cargo.lock: {egui:?}");
    let (epaint, egui) = (epaint[0], egui[0]);
    assert!(
        epaint.get("source").is_none(),
        "epaint {} comes from {} instead of third_party/epaint: the kinsoku patch is not applied. \
         Run `python3 scripts/update-epaint.py` (third_party/README.md).",
        epaint["version"],
        epaint["source"]
    );
    assert_eq!(
        epaint["version"], egui["version"],
        "the vendored epaint must be egui's version: run `python3 scripts/update-epaint.py`"
    );
    let unused = lock
        .get("patch")
        .and_then(|p| p.get("unused"))
        .and_then(toml::Value::as_array)
        .map(|u| u.iter().filter(|p| p["name"].as_str() == Some("epaint")).count())
        .unwrap_or(0);
    assert_eq!(unused, 0, "Cargo.lock lists the epaint patch as unused");
    let vendored: toml::Table = std::fs::read_to_string(root.join("third_party/epaint/Cargo.toml"))
        .expect("third_party/epaint/Cargo.toml")
        .parse()
        .expect("TOML");
    assert_eq!(vendored["package"]["version"], epaint["version"]);
}

// ---- Golden screenshot story ------------------------------------------------------------

/// Column widths of the story (points): narrow, medium, wide.
const COLUMNS: [f32; 3] = [132.0, 188.0, 252.0];

fn kinsoku_story() -> Story {
    Story::new("UI/Text/Kinsoku", |app, _| {
        app.add_plugin(UiPlugin::deterministic());
        app.resources.expect::<EguiContext>().ctx().set_fonts(cjk_fonts());
        app.world.spawn((
            Transform::from_xyz(0.0, 0.0, 3.0),
            Camera3d::default().with_clear_color(Vec4::new(0.012, 0.015, 0.024, 1.0)),
        ));
        app.add_ui_system(|ctx, _| {
            let mut x = 12.0;
            for (i, width) in COLUMNS.into_iter().enumerate() {
                egui::Area::new(egui::Id::new(("kinsoku", i)))
                    .fixed_pos([x, 10.0])
                    .show(ctx, |ui| {
                        ui.set_width(width);
                        ui.label(RichText::new(format!("{width} pt")).size(11.0).weak());
                        for text in [JAPANESE[0], TRADITIONAL_CHINESE[1], JAPANESE[2]] {
                            egui::Frame::new()
                                .stroke(egui::Stroke::new(1.0, Color32::from_gray(70)))
                                .inner_margin(4.0)
                                .show(ui, |ui| {
                                    ui.set_width(width - 8.0);
                                    ui.label(RichText::new(text).size(14.0).color(Color32::from_gray(230)));
                                });
                            ui.add_space(4.0);
                        }
                    });
                x += width + 14.0;
            }
        });
    })
    .description(
        "Japanese and Traditional Chinese wrapped at three widths by the kinsoku rules of the patched \
         epaint: no line starts with 、。，」）？！ー… or small kana, none ends with 「（【.",
    )
    .size(640, 440)
    .ticks(3)
    .tolerance(0.01)
}

fn ui_storybook() -> Storybook {
    let mut book = Storybook::new();
    book.add(kinsoku_story());
    book
}

// `crates/space3d-ui/tests/__screenshots__/UI/Text/Kinsoku.png`.
storybook_tests!(ui_storybook);
