//! Kinsoku shori (禁則処理): where a line of Chinese or Japanese text may break.
//!
//! **Space3d patch**, not part of upstream epaint: see `third_party/README.md` in the Space3d
//! repository. Derived from the kinsoku patch the `SpaceCorps` game carried in its own copy
//! of epaint (game commits b8af874 and e531f10).
//!
//! egui breaks a row at a space, after a Han character or kana, before one, and as a fallback
//! at a dash, after ASCII punctuation or anywhere. Its only CJK rule was a short list of marks
//! that may not follow a Han character or kana at the start of a line, so a Chinese or
//! Japanese line could start with "，" "？" "）" "…" or, through the fallbacks, with "、" after
//! a Latin word or a small kana ("ロ" | "ッ"), and end with "（" or "「".
//!
//! [`line_break`] rules on every position between two characters before egui records it as a
//! break candidate:
//!
//! * [`Break::Prohibited`]: no break here. A run of characters kinsoku keeps together moves to
//!   the next row whole when it fits there; one wider than a row (a run of marks, a word and
//!   its "、" in a very narrow column) still breaks where the row is full, as upstream egui
//!   breaks a word wider than the row — but not inside "——" or "……" when the pair fits a row,
//!   nor inside a character and its combining mark or variation selector.
//! * [`Break::Allowed`]: a word boundary, also where egui had none: after CJK punctuation
//!   before a Latin word or a number ("、" | "Thulium", "。" | "4"), before an opening bracket
//!   that follows a Latin word or a mark ("Skylab" | "「", "！" | "（").
//! * [`Break::Upstream`]: egui's own rules decide. This is the answer for every pair of
//!   characters without a Chinese or Japanese character in it (apart from the ellipses and
//!   dashes below), so Latin, Cyrillic, Greek, Korean… text lays out exactly as upstream, and
//!   “ ‘ « — which open a quote in Chinese and English but close one in German („Tor“) — only
//!   count as opening brackets next to Chinese or Japanese characters.
//!
//! Row breaking uses [`line_break`] through a paragraph-level scan (crate-private), which also
//! looks past spaces: a space between a bracket and its text ("「　重要　」") doesn't let the
//! bracket end or start a line.
//!
//! The character classes follow JIS X 4051 (Japanese) and the W3C requirements for Japanese
//! and Chinese layout (JLREQ, CLREQ), strict level: no line starts with a closing bracket or
//! quote, a comma, full stop, colon, semicolon, ?, !, middle dot, hyphen or wave dash, small
//! kana, the prolonged sound mark, an iteration mark, a postfixed sign (％ ℃) or an ellipsis;
//! no line ends with an opening bracket or quote or a prefixed sign (￥ ＄); "——" "……" "‥‥"
//! stay together; no line breaks on either side of a straight quote `"` `'` (it may open or
//! close). Hanging punctuation (ぶら下げ) and justification are out of scope.

use crate::text::Glyph;

/// Revision of these rules, bumped whenever they change. Its presence tells a program or a
/// test that epaint is Space3d's patched copy (`egui::epaint::text::kinsoku::VERSION`).
pub const VERSION: u32 = 2;

/// Kinsoku's ruling on a line break between two characters, see [`line_break`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Break {
    /// No break here, unless the row has no other place to break at all.
    Prohibited,

    /// A good place to break (a word boundary), even where egui's own rules have none.
    Allowed,

    /// Kinsoku has no say: egui's own rules decide (spaces, Latin text, …).
    Upstream,
}

/// May a line break between `before` and `after`?
///
/// Text without Chinese or Japanese characters gets [`Break::Upstream`] (except that no line
/// starts with an ellipsis after a non-space, and "——" "……" stay together), so it lays out
/// as with upstream egui. A space decides for itself — a line breaks after it as egui does —
/// except that next to Chinese or Japanese no line starts with a closing mark after it or ends
/// with an opening one before it ("字　」", "（\u{3000}").
pub fn line_break(before: char, after: char) -> Break {
    if is_inseparable(before, after) {
        return Break::Prohibited;
    }
    if before.is_whitespace() || after.is_whitespace() {
        // A space breaks as egui decides, but next to Chinese or Japanese it takes no closing
        // mark to the start of a line and leaves no opening one at the end ("字　」", "「 字").
        let cjk = is_cjk(before) || is_cjk(after);
        return if cjk && (is_line_start_prohibited(after) || is_line_end_prohibited(before)) {
            Break::Prohibited
        } else {
            Break::Upstream
        };
    }
    if !is_cjk(before) && !is_cjk(after) {
        // Unicode line breaking (UAX #14, LB22) keeps an ellipsis with what precedes it in
        // every script; Chinese text puts "……" right after Latin words ("Thulium……").
        return if is_ellipsis(after) {
            Break::Prohibited
        } else {
            Break::Upstream
        };
    }
    if is_line_end_prohibited(before)
        || is_line_start_prohibited(after)
        || is_straight_quote(before)
        || is_glue(before)
        || is_glue(after)
        || is_mark(after)
    {
        Break::Prohibited
    } else {
        Break::Allowed
    }
}

/// Characters of Chinese and Japanese text, which switch the kinsoku rules on next to them.
///
/// Han characters, kana, Bopomofo, CJK symbols and punctuation, full-width forms and
/// half-width katakana. Not Hangul: Korean breaks at spaces, as upstream egui does.
pub fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{2E80}'..='\u{2FDF}' // CJK Radicals Supplement, Kangxi Radicals
        | '\u{2FF0}'..='\u{2FFF}' // Ideographic Description Characters
        | '\u{3000}'..='\u{303F}' // CJK Symbols and Punctuation (、。「」〜々)
        | '\u{3040}'..='\u{30FF}' // Hiragana, Katakana
        | '\u{3100}'..='\u{312F}' // Bopomofo
        | '\u{3190}'..='\u{31FF}' // Kanbun, Bopomofo Extended, CJK Strokes, Katakana Phonetic Extensions
        | '\u{3200}'..='\u{33FF}' // Enclosed CJK Letters and Months, CJK Compatibility
        | '\u{3400}'..='\u{4DBF}' // CJK Unified Ideographs Extension A
        | '\u{4E00}'..='\u{9FFF}' // CJK Unified Ideographs
        | '\u{F900}'..='\u{FAFF}' // CJK Compatibility Ideographs
        | '\u{FE10}'..='\u{FE1F}' // Vertical Forms
        | '\u{FE30}'..='\u{FE4F}' // CJK Compatibility Forms
        | '\u{FE50}'..='\u{FE6F}' // Small Form Variants (﹐﹑)
        | '\u{FF00}'..='\u{FF9F}' // Full-width ASCII (，！（）：；？％), half-width katakana
        | '\u{FFE0}'..='\u{FFEF}' // Full-width signs (￥￠), half-width symbols
        | '\u{1AFF0}'..='\u{1B16F}' // Kana Extended-B, Kana Supplement, Kana Extended-A, Small Kana Extension
        | '\u{20000}'..='\u{3FFFF}' // Supplementary and Tertiary Ideographic Planes
    )
}

/// Characters that may not start a line of Chinese or Japanese text (行頭禁則).
///
/// A superset of the list egui checks after a Han character or kana (the ASCII marks among
/// them only count next to Chinese or Japanese characters, see [`line_break`]).
pub fn is_line_start_prohibited(c: char) -> bool {
    matches!(c,
        // Closing brackets and quotes (JIS X 4051 cl-02).
        ')' | ']' | '}' | '\'' | '"' | '’' | '”' | '»'
        | '）' | '］' | '｝' | '〕' | '〉' | '》' | '」' | '』' | '】' | '〙' | '〗' | '〛'
        | '〞' | '〟' | '｠' | '｣'
        | '︘' | '︶' | '︸' | '︺' | '︼' | '︾' | '﹀' | '﹂' | '﹄' | '﹈' | '﹚' | '﹜' | '﹞'
        // Hyphens and wave dashes (cl-03).
        | '‐' | '–' | '゠' | '〜' | '～'
        // Question and exclamation marks (cl-04).
        | '?' | '!' | '？' | '！' | '‼' | '⁇' | '⁈' | '⁉' | '︕' | '︖' | '﹖' | '﹗'
        // Middle dots, colons and semicolons (cl-05), the Chinese interpunct (CLREQ).
        | ':' | ';' | '：' | '；' | '・' | '･' | '·' | '‧' | '︓' | '︔' | '﹔' | '﹕'
        // Full stops (cl-06).
        | '.' | '。' | '．' | '｡' | '︒' | '﹒'
        // Commas (cl-07).
        | ',' | '、' | '，' | '､' | '︐' | '︑' | '﹐' | '﹑'
        // Iteration marks (cl-09).
        | 'ヽ' | 'ヾ' | 'ゝ' | 'ゞ' | '々' | '〻'
        // Prolonged sound mark (cl-10).
        | 'ー' | 'ｰ'
        // Small kana (cl-11).
        | 'ぁ' | 'ぃ' | 'ぅ' | 'ぇ' | 'ぉ' | 'っ' | 'ゃ' | 'ゅ' | 'ょ' | 'ゎ' | 'ゕ' | 'ゖ'
        | 'ァ' | 'ィ' | 'ゥ' | 'ェ' | 'ォ' | 'ッ' | 'ャ' | 'ュ' | 'ョ' | 'ヮ' | 'ヵ' | 'ヶ'
        | '\u{31F0}'..='\u{31FF}' // ㇰ-ㇿ (Ainu)
        | '\u{FF67}'..='\u{FF6F}' // ｧ-ｯ (half-width)
        | '\u{1B132}' | '\u{1B150}'..='\u{1B152}' | '\u{1B155}' | '\u{1B164}'..='\u{1B167}'
        // Voiced sound marks, spacing forms.
        | '゛' | '゜' | 'ﾞ' | 'ﾟ'
        // Postfixed signs.
        | '%' | '％' | '‰' | '°' | '′' | '″' | '℃' | '℉' | '¢' | '￠'
    ) || is_ellipsis(c)
}

/// Characters that may not end a line of Chinese or Japanese text (行末禁則): opening
/// brackets and quotes (JIS X 4051 cl-01) and the full-width prefixed signs, which stay with
/// the number after them.
pub fn is_line_end_prohibited(c: char) -> bool {
    matches!(
        c,
        // Opening brackets and quotes (JIS X 4051 cl-01).
        '(' | '[' | '{' | '‘' | '“' | '«'
        | '（' | '［' | '｛' | '〔' | '〈' | '《' | '「' | '『' | '【' | '〘' | '〖' | '〚'
        | '〝' | '｟' | '｢'
        | '︗' | '︵' | '︷' | '︹' | '︻' | '︽' | '︿' | '﹁' | '﹃' | '﹇' | '﹙' | '﹛' | '﹝'
        // Prefixed signs.
        | '￥' | '＄' | '￡' | '＃'
    )
}

/// Pairs that stay together (分離禁止): the two halves of "——" "――" "……" "‥‥" "⋯⋯", and
/// the vertical kana repeat marks 〳〵 〴〵.
pub fn is_inseparable(before: char, after: char) -> bool {
    (before == after && matches!(before, '—' | '―' | '…' | '‥' | '⋯' | '︙'))
        || (matches!(before, '〳' | '〴') && after == '〵')
}

/// `"` and `'` open and close quotes (Unicode line breaking class QU): next to Chinese or
/// Japanese no line breaks on either side of them.
fn is_straight_quote(c: char) -> bool {
    matches!(c, '"' | '\'')
}

fn is_ellipsis(c: char) -> bool {
    matches!(c, '…' | '‥' | '⋯' | '︙')
}

/// Word joiners (and zero-width joiners), across which no line breaks.
fn is_glue(c: char) -> bool {
    matches!(c, '\u{2060}' | '\u{FEFF}' | '\u{200D}')
}

/// Combining marks and variation selectors continue the character before them (か + ゙,
/// 葛 + an ideographic variation selector).
fn is_mark(c: char) -> bool {
    use unicode_general_category::{GeneralCategory, get_general_category};
    matches!(
        get_general_category(c),
        GeneralCategory::NonspacingMark
            | GeneralCategory::SpacingMark
            | GeneralCategory::EnclosingMark
    )
}

// ---- Glyph level (used by `RowBreakCandidates` in `text_layout.rs`) -------------------------

/// Kinsoku along a paragraph: [`line_break`] at each position between two glyphs, plus what
/// takes more than the two characters around a position. Spaces between a bracket and its text
/// keep the bracket on the line of its text ("「　字", "字　」": no line ends with "「　" or
/// starts with "　」"), and a cluster stays whole even when the shaper gave its variation
/// selector the base's character (see [`continues`]).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Scan {
    /// In a run of spaces after an opening bracket of Chinese or Japanese text.
    after_opener: bool,

    /// In (or right before) a run of spaces that a closing mark of Chinese or Japanese text
    /// follows.
    before_closer: bool,
}

impl Scan {
    /// Kinsoku's ruling on the position after `glyphs[0]` (`glyphs`: the rest of the
    /// paragraph). Call it for every position of a paragraph, in order.
    pub(crate) fn next(&mut self, glyphs: &[Glyph]) -> Break {
        let glyph = &glyphs[0];
        let Some(next) = glyphs.get(1) else {
            return Break::Upstream;
        };
        if !glyph.chr.is_whitespace() {
            self.after_opener = is_cjk(glyph.chr) && is_line_end_prohibited(glyph.chr);
            // Before a run of spaces, look past it once.
            self.before_closer = next.chr.is_whitespace()
                && glyphs[1..]
                    .iter()
                    .find(|g| !g.chr.is_whitespace())
                    .is_some_and(|g| is_cjk(g.chr) && is_line_start_prohibited(g.chr));
        }
        if (glyph.chr.is_whitespace() && self.after_opener)
            || (next.chr.is_whitespace() && self.before_closer)
            || continues(glyph, next)
        {
            Break::Prohibited
        } else {
            line_break(glyph.chr, next.chr)
        }
    }
}

/// `next` continues the cluster of `glyph`, a Chinese or Japanese character: it has no width
/// (a combining mark, or a variation selector or joiner, which the shaper gives the base's
/// character when the font lacks it). No row breaks between them, not even as a last resort:
/// that would only start the next row with an empty glyph.
pub(crate) fn continues(glyph: &Glyph, next: &Glyph) -> bool {
    next.advance_width <= 0.0 && is_cjk(glyph.chr)
}

/// A last-resort break between `glyph` and `next` is better than one elsewhere in the row:
/// not inside "——" "……" (unless the pair alone is wider than the row) or a cluster.
pub(crate) fn separable(glyph: &Glyph, next: &Glyph) -> bool {
    !is_inseparable(glyph.chr, next.chr) && !continues(glyph, next)
}

/// Whether `glyphs` (from the first after a break to the one that overflowed the row) fit on
/// a row `width` wide.
pub(crate) fn fits(glyphs: &[Glyph], width: f32) -> bool {
    match (glyphs.first(), glyphs.last()) {
        (Some(first), Some(last)) => last.max_x() - first.pos.x <= width,
        _ => true,
    }
}

/// Whether a row may break after `glyphs[0]` and leave the rest (up to the glyph that
/// overflowed the row) for the next row: they fit there, or there is no Chinese or Japanese
/// in `glyphs` — then kinsoku had no say about the breaks in them, and the rest of a Latin
/// word may overflow a little as with upstream egui.
pub(crate) fn may_carry(glyphs: &[Glyph], width: f32) -> bool {
    fits(&glyphs[1..], width) || !glyphs.iter().any(|glyph| is_cjk(glyph.chr))
}

/// Whether the run kinsoku keeps together after a break after `glyphs[index]` (up to the next
/// position it allows a break at) fits on a row `width` wide. `RowBreakCandidates` moves such
/// a run to the next row only when it fits there whole; a wider one breaks where the row is
/// full, as upstream egui does, instead of overflowing the next row.
pub(crate) fn run_fits(glyphs: &[Glyph], index: usize, width: f32) -> bool {
    let Some(first) = glyphs.get(index + 1) else {
        return true;
    };
    // A break is allowed after `index`, so a scan from there sees what the paragraph's saw.
    let mut scan = Scan::default();
    scan.next(&glyphs[index..]);
    for end in index + 1..glyphs.len() {
        if glyphs[end].max_x() - first.pos.x > width {
            return false;
        }
        if scan.next(&glyphs[end..]) != Break::Prohibited {
            return true;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        Break, Scan, is_cjk, is_inseparable, is_line_end_prohibited, is_line_start_prohibited,
        line_break,
    };
    use crate::text::{
        FontDefinitions, FontId, FontsImpl, LayoutJob, TextFormat, TextOptions,
        font::is_cjk_break_allowed, layout,
    };

    /// Asserts `line_break` on each two-character string.
    fn expect(expected: Break, pairs: &[&str]) {
        for pair in pairs {
            let chars: Vec<char> = pair.chars().collect();
            let [before, after] = chars[..] else {
                panic!("{pair:?} is not two characters");
            };
            assert_eq!(line_break(before, after), expected, "{pair:?}");
        }
    }

    fn fonts() -> FontsImpl {
        FontsImpl::new(TextOptions::default(), FontDefinitions::default())
    }

    /// Rows of `text` wrapped at `width` points. egui's default fonts have no Han characters or
    /// kana (they are replacement glyphs), which is fine: the rules only look at characters.
    fn rows(fonts: &mut FontsImpl, text: &str, width: f32) -> Vec<String> {
        let mut job = LayoutJob::single_section(
            text.to_owned(),
            TextFormat::simple(FontId::proportional(14.0), crate::Color32::WHITE),
        );
        job.wrap.max_width = width;
        let galley = layout(fonts, 1.0, Arc::new(job));
        let rows: Vec<String> = galley.rows.iter().map(|row| row.text()).collect();
        assert_eq!(rows.concat(), text, "characters lost at {width}");
        rows
    }

    /// Kinsoku violations in `rows`: a break that [`line_break`] prohibits, a row (after the
    /// first) starting with one of the closing marks egui used to put there, or one (before the
    /// last) ending with an opening bracket.
    fn violations(rows: &[String]) -> Vec<String> {
        const NOT_FIRST: &str =
            "，．：；？！）］｝〕〉》」』】、。・ーっゃゅょァィゥェォッャュョ々～％…”’";
        const NOT_LAST: &str = "（［｛〔〈《「『【“‘";
        let mut bad = Vec::new();
        for (i, pair) in rows.windows(2).enumerate() {
            let (Some(last), Some(first)) = (pair[0].chars().last(), pair[1].chars().next()) else {
                continue;
            };
            if line_break(last, first) == Break::Prohibited
                || NOT_FIRST.contains(first)
                || NOT_LAST.contains(last)
            {
                bad.push(format!("{last:?} | {first:?} after row {i}: {rows:?}"));
            }
        }
        bad
    }

    /// Japanese and Chinese only.
    const CJK: &[&str] = &[
        "まっさらなスタート：育成のみで、パイロット同士は攻撃できません。「重要」（注意）次へ！本当に？はい、そうです。",
        "ロッカーのキャッシュ：人々は時々、こゝろの中でいすゞの名を呼んだ。コーヒー――それとも……紅茶？ェェッ！",
        "『星の航路』（第三版）――出発はまだ先だ。どこへ行くのか、誰も知らない。",
        "星门还差三个部件：星门完成后扫描即停止，只按实际进行的扫描收费。你确定吗？是的！（请注意）继续吧，好的；完成。",
        "按「确认」锁定（冻结）传送缓存：锁定后无法放入或取出物品，【注意】解锁后可再次修改。",
        "连接中断……正在重新连接，请稍候。服务器没有响应……请检查网络。他说：“你好！”——然后走了。",
    ];

    /// Japanese and Chinese with Latin words and numbers (from the `SpaceCorps` help texts).
    const MIXED: &[&str] = &[
        "シールドボーナス、Shield Wall ブースター（+25%）、Shield Boost バフで合計が増えます。",
        "账号的其余部分都会保留：信用点、Thulium、等级、荣誉、重置点数、永久加成、Skylab 和战队。",
        "齐射随机造成该数值的 80–100%，可能暴击，并乘以弹药倍率。机库显示的是未计增益时 x1 弹药齐射的伤害。",
        "欢迎来到 SpaceCorps。本周末双倍经验值活动进行中。4-2 星区目前海盗活动频繁，请谨慎前往。",
        "価格は￥1,200（税込）、送料は無料です。詳しくはSupport「FAQ」をご覧ください。",
    ];

    const GERMAN: &[&str] = &[
        "Wie Piloten beitreten: von den Offizieren unter „Verwaltung“ festgelegt",
        "Aktive Konfiguration (wechseln mit der Taste für „Konfig wechseln“)",
        "Kaufe „Slave Drones“ im Shop, um Slots freizuschalten, und ‚Mehr‘ dazu.",
        "»Die Flotte«, sagte sie, »wartet nicht.« – Ende…",
    ];

    // ---- Character classes -----------------------------------------------------------------

    fn assert_class(chars: &str, class: fn(char) -> bool, name: &str) {
        for c in chars.chars() {
            assert!(class(c), "{c:?} (U+{:04X}) should be {name}", c as u32);
        }
    }

    #[test]
    fn closing_brackets_and_quotes_may_not_start_a_line() {
        let closing = "）］｝〕〉》」』】〙〗〛〞〟｠｣’”»)]}";
        assert_class(closing, is_line_start_prohibited, "line-start prohibited");
        for c in closing.chars() {
            expect(Break::Prohibited, &[&format!("字{c}"), &format!("。{c}")]);
        }
    }

    #[test]
    fn punctuation_may_not_start_a_line() {
        // Commas, full stops, colons, semicolons, middle dots, ? and ! in full and half width.
        let marks = "、，､。．｡：；・･·？！‼⁇⁈⁉,.:;?!";
        assert_class(marks, is_line_start_prohibited, "line-start prohibited");
        for c in marks.chars() {
            expect(Break::Prohibited, &[&format!("字{c}"), &format!("か{c}")]);
            // After a Latin letter only the CJK marks count: "A," is Latin text.
            let after_latin = if is_cjk(c) {
                Break::Prohibited
            } else {
                Break::Upstream
            };
            expect(after_latin, &[&format!("A{c}")]);
        }
    }

    #[test]
    fn small_kana_prolonged_sound_and_iteration_marks_may_not_start_a_line() {
        let marks = "ぁぃぅぇぉっゃゅょゎゕゖァィゥェォッャュョヮヵヶㇰㇱㇲㇳㇴㇵㇶㇷㇸㇹㇺㇻㇼㇽㇾㇿｧｨｩｪｫｬｭｮｯーｰ々ゝゞヽヾ〻゛゜ﾞﾟ";
        assert_class(marks, is_line_start_prohibited, "line-start prohibited");
        for c in marks.chars() {
            expect(Break::Prohibited, &[&format!("ロ{c}"), &format!("字{c}")]);
        }
        // Full-size kana may start a line (upstream broke "ロ" | "ッ" through its fallback).
        for c in "あいうえおつやゆよわかけアイウエオツヤユヨワカケｱｶ".chars()
        {
            assert!(!is_line_start_prohibited(c), "{c:?}");
            expect(Break::Allowed, &[&format!("ロ{c}")]);
        }
    }

    #[test]
    fn hyphens_postfixed_signs_and_ellipses_may_not_start_a_line() {
        let marks = "‐–゠〜～％‰℃°′″￠…‥⋯";
        assert_class(marks, is_line_start_prohibited, "line-start prohibited");
        for c in marks.chars() {
            expect(Break::Prohibited, &[&format!("字{c}")]);
        }
    }

    #[test]
    fn opening_brackets_and_prefixed_signs_may_not_end_a_line() {
        let opening = "（［｛〔〈《「『【〘〖〚〝｟｢‘“«([{￥＄￡＃";
        assert_class(opening, is_line_end_prohibited, "line-end prohibited");
        for c in opening.chars() {
            assert!(!is_line_start_prohibited(c), "{c:?}");
            expect(Break::Prohibited, &[&format!("{c}字"), &format!("{c}か")]);
        }
        // A line may break before one, also after a Latin word, a number or a mark.
        for c in "（［「『【《〈".chars() {
            let pairs = ["字", "。", "！", "b", "5", ")"].map(|b| format!("{b}{c}"));
            expect(Break::Allowed, &pairs.each_ref().map(String::as_str));
        }
    }

    #[test]
    fn inseparable_pairs_stay_together() {
        let pairs = ["——", "――", "……", "‥‥", "⋯⋯", "〳〵", "〴〵"];
        for pair in pairs {
            let chars: Vec<char> = pair.chars().collect();
            assert!(is_inseparable(chars[0], chars[1]), "{pair}");
        }
        expect(Break::Prohibited, &pairs);
        // A single dash may start and end a line; a dash and an ellipsis are not a pair.
        expect(Break::Allowed, &["字—", "—字"]);
        assert!(!is_inseparable('—', '…'), "—…");
    }

    #[test]
    fn marks_and_joiners_stay_with_their_character() {
        // か + a combining voiced sound mark (a decomposed が), 葛 + an ideographic variation
        // selector, word joiners.
        expect(
            Break::Prohibited,
            &["か\u{3099}", "葛\u{E0100}", "字\u{2060}", "\u{2060}字"],
        );
        expect(Break::Allowed, &["\u{3099}き"]);
    }

    /// Every mark in egui's own list still may not start a line after a Han character or kana:
    /// the patch only adds to it.
    #[test]
    fn egui_list_is_included() {
        let upstream = ")]｝〕〉》」』】〙〗〟'\"｠»ヽヾーァィゥェォッャュョヮヵヶぁぃぅぇぉっゃゅょゎゕゖㇰㇱㇲㇳㇴㇵㇶㇷㇸㇹㇺㇻㇼㇽㇾㇿ々〻‐゠–〜?!‼⁇⁈⁉・、:;,。.";
        for c in upstream.chars() {
            assert!(!is_cjk_break_allowed(c), "{c:?} left egui's list");
            expect(Break::Prohibited, &[&format!("日{c}"), &format!("か{c}")]);
        }
        assert_class(upstream, is_line_start_prohibited, "line-start prohibited");
    }

    #[test]
    fn cjk_characters() {
        assert_class(
            "日本語かなカナ㐀𠀋〇、。「」〜々，！（）：；？％ｱﾞ￥ㄅ豈︵",
            is_cjk,
            "CJK",
        );
        for c in "aZ09 -()…—“”‘’«»„ÄßПλ한글ㄱ😀".chars() {
            assert!(!is_cjk(c), "{c:?} (U+{:04X}) is not CJK", c as u32);
        }
    }

    // ---- Break rules -----------------------------------------------------------------------

    #[test]
    fn between_han_characters_and_kana_a_line_may_break() {
        expect(
            Break::Allowed,
            &[
                "日本", "本語", "語か", "かな", "なカ", "カナ", "ナ日", "〇日", "𠀋日",
            ],
        );
    }

    #[test]
    fn latin_words_and_numbers_inside_cjk_text() {
        // A Latin word or a number is one unit: breaks before and after it, none inside.
        expect(Break::Allowed, &["用T", "m等", "的8", "0年"]);
        expect(Break::Upstream, &["Th", "25", "+2"]);
        // After CJK punctuation before a Latin word or a number, where egui had no break and
        // fell back to breaking anywhere ("Thulium" | "、").
        expect(Break::Allowed, &["、T", "。4", "）x", "」S", "，8"]);
        // Never before CJK closing punctuation, also not after an ASCII mark ("+25%" | "）")
        // or before an ellipsis after a Latin word.
        expect(
            Break::Prohibited,
            &["m、", "%）", "5％", ")、", "-」", "s…", "m…"],
        );
        // Never after an opening bracket or a prefixed sign.
        expect(Break::Prohibited, &["（+", "「F", "(中", "“你", "￥1"]);
        // A space breaks as in upstream egui.
        expect(Break::Upstream, &["字 ", " S", "　字", "、 "]);
    }

    #[test]
    fn chinese_quotes() {
        // “ ” and ‘ ’ open and close quotes in Chinese.
        expect(Break::Allowed, &["说“", "：“", "”然", "’字"]);
        expect(Break::Prohibited, &["“你", "‘你", "好”", "！”", "好’"]);
        // " and ' open and close: no break on either side of them next to CJK.
        expect(
            Break::Prohibited,
            &["说\"", "\"你", "好'", "'你", "\"（", "）\""],
        );
        expect(Break::Upstream, &["s'", "'s", "\"T", "T\""]);
    }

    /// A space next to a bracket of Chinese or Japanese text: no line starts with the closing
    /// mark after it or ends with the opening one before it (as upstream, a line still breaks
    /// after a space elsewhere).
    #[test]
    fn spaces_do_not_separate_brackets_from_their_text() {
        expect(
            Break::Prohibited,
            &[
                "\u{3000}」",
                " 」",
                "\u{A0}）",
                "\u{3000}。",
                "\u{3000}…",
                "\u{3000})",
                "（\u{3000}",
                "（\u{A0}",
                "「 ",
                "【 ",
                "“\u{3000}",
                "‘\u{3000}",
            ],
        );
        expect(
            Break::Upstream,
            &[
                "字 ",
                " 字",
                "\u{3000}字",
                "」\u{3000}",
                "\u{3000}「",
                "。 ",
                "“ ",
                " )",
                "a ",
            ],
        );
    }

    /// No Chinese or Japanese character in the pair: egui's own rules, so text without them
    /// lays out exactly as upstream (`RowBreakCandidates` records every other break as egui
    /// does). The only exceptions are an ellipsis after a non-space and "——" "……", which
    /// Unicode line breaking keeps together in every script.
    #[test]
    fn text_without_cjk_is_left_to_egui() {
        let mut sample: Vec<char> = (' '..='~').collect();
        sample.extend('\u{A0}'..='\u{24F}'); // Latin-1, Latin Extended-A and -B
        sample.extend('\u{370}'..='\u{3FF}'); // Greek
        sample.extend('\u{400}'..='\u{4FF}'); // Cyrillic
        sample.extend('\u{600}'..='\u{6FF}'); // Arabic
        sample.extend('\u{2000}'..='\u{206F}'); // General Punctuation („“‚‘ – — …)
        sample.extend('\u{20A0}'..='\u{20CF}'); // Currency Symbols
        sample.extend('\u{2100}'..='\u{214F}'); // Letterlike Symbols (℃)
        sample.extend('\u{AC00}'..='\u{AC80}'); // Hangul syllables
        sample.extend('\u{1F600}'..='\u{1F64F}'); // Emoji
        let mut checked = 0_usize;
        for &a in &sample {
            assert!(!is_cjk(a), "{a:?}");
            for &b in &sample {
                let ellipsis = matches!(b, '…' | '‥' | '⋯');
                let expected = if is_inseparable(a, b)
                    || (ellipsis && !a.is_whitespace() && !b.is_whitespace())
                {
                    Break::Prohibited
                } else {
                    Break::Upstream
                };
                assert_eq!(line_break(a, b), expected, "{a:?} {b:?}");
                checked += 1;
            }
        }
        assert!(checked > 1_000_000, "only {checked} pairs");
    }

    // ---- Layout ----------------------------------------------------------------------------

    fn width(fonts: &mut FontsImpl, text: &str) -> f32 {
        let job = LayoutJob::single_section(
            text.to_owned(),
            TextFormat::simple(FontId::proportional(14.0), crate::Color32::WHITE),
        );
        layout(fonts, 1.0, Arc::new(job)).size().x
    }

    /// The width of the widest run of `text` that kinsoku keeps on one line ("茶？ェェッ！",
    /// "m、", "%）、"): narrower rows must break inside such a run, as upstream would.
    fn widest_unit(fonts: &mut FontsImpl, text: &str) -> f32 {
        let chars: Vec<char> = text.chars().collect();
        let mut widest = 0.0_f32;
        let mut start = 0;
        for i in 0..chars.len() {
            let joined = chars
                .get(i + 1)
                .is_some_and(|&next| line_break(chars[i], next) == Break::Prohibited);
            if !joined {
                let unit: String = chars[start..=i].iter().collect();
                widest = widest.max(width(fonts, &unit));
                start = i + 1;
            }
        }
        widest
    }

    /// Japanese and Chinese paragraphs, alone and with Latin words and numbers, at every width
    /// from the widest run kinsoku keeps together to 420 points: no line starts with a closing
    /// mark or ends with an opening one, and no "——" or "……" is split. Upstream egui fails
    /// this at most widths.
    #[test]
    fn wrapped_cjk_text_follows_kinsoku() {
        let mut fonts = fonts();
        let mut bad = Vec::new();
        for text in CJK.iter().chain(MIXED) {
            let min_width = widest_unit(&mut fonts, text).ceil() as usize + 1;
            assert!(min_width < 100, "{min_width}: {text}");
            for width in (min_width..=420).step_by(2) {
                let rows = rows(&mut fonts, text, width as f32);
                bad.extend(
                    violations(&rows)
                        .into_iter()
                        .map(|v| format!("{width}: {v}")),
                );
            }
        }
        assert!(
            bad.is_empty(),
            "{} violations:\n{}",
            bad.len(),
            bad.join("\n")
        );
    }

    /// German quotes „…“ and ‚…‘ close with the characters that open Chinese and English ones:
    /// no German line starts with “ or ‘ at any width that holds the longest word (an earlier
    /// version of the game's patch broke before them).
    #[test]
    fn german_closing_quotes_stay_on_their_word() {
        let mut fonts = fonts();
        for text in GERMAN {
            let min_width = text
                .split(' ')
                .map(|word| width(&mut fonts, word))
                .fold(0.0, f32::max)
                .ceil() as usize
                + 1;
            for width in (min_width..=460).step_by(2) {
                let rows = rows(&mut fonts, text, width as f32);
                assert!(
                    rows.iter().skip(1).all(|r| !r.starts_with(['“', '‘', '«'])),
                    "{width}: {rows:?}"
                );
            }
        }
    }

    /// A run of marks that may not start a line still wraps at the width, filling each row as
    /// upstream does, rather than overflowing or breaking after every character.
    #[test]
    fn a_run_of_prohibited_marks_still_wraps() {
        let mut fonts = fonts();
        for text in [
            "ーーーーーーーーーーーーーーーーーーーーーーーーーーーーーー",
            "……………………………………………………………………",
        ] {
            let rows = rows(&mut fonts, text, 100.0);
            assert!(rows.len() > 2, "{rows:?}");
            assert!(
                rows.iter().rev().skip(1).all(|r| r.chars().count() > 2),
                "{rows:?}"
            );
        }
    }

    /// A Latin word wider than the row still breaks inside, but not before the "、" after it
    /// (at any width that holds "m、" and "点、", which may not be split).
    #[test]
    fn a_word_wider_than_the_row_keeps_its_comma() {
        let mut fonts = fonts();
        let text = "信用点、Thulium、等级";
        let min_width = ["m、", "点、"]
            .map(|t| width(&mut fonts, t))
            .into_iter()
            .fold(0.0, f32::max);
        for width in (min_width.ceil() as usize..=120).step_by(1) {
            let rows = rows(&mut fonts, text, width as f32);
            assert!(
                rows.iter().skip(1).all(|r| !r.starts_with('、')),
                "{width}: {rows:?}"
            );
        }
    }

    /// Glyphs of `text` laid out on one row.
    fn glyphs(fonts: &mut FontsImpl, text: &str) -> Vec<crate::text::Glyph> {
        let job = LayoutJob::single_section(
            text.to_owned(),
            TextFormat::simple(FontId::proportional(14.0), crate::Color32::WHITE),
        );
        let galley = layout(fonts, 1.0, Arc::new(job));
        galley
            .rows
            .iter()
            .flat_map(|row| row.glyphs.clone())
            .collect()
    }

    /// Width of the widest run of `text` that a paragraph's [`Scan`] keeps together.
    fn widest_scanned_unit(fonts: &mut FontsImpl, text: &str) -> f32 {
        let glyphs = glyphs(fonts, text);
        let mut scan = Scan::default();
        let (mut widest, mut start) = (0.0_f32, 0);
        for i in 0..glyphs.len() {
            if scan.next(&glyphs[i..]) != Break::Prohibited {
                widest = widest.max(glyphs[i].max_x() - glyphs[start].pos.x);
                start = i + 1;
            }
        }
        widest
    }

    /// Spaces between brackets and their text, as Japanese sometimes has: at every width that
    /// holds the widest run kept together, no row starts with a closing mark after its leading
    /// spaces or ends with an opening one before its trailing spaces.
    #[test]
    fn brackets_stay_with_their_text_across_spaces() {
        let mut fonts = fonts();
        let text = "第一章　「　始まり　」　そして誰も（ 注意 ）いなくなった　。「重要」　です";
        let min_width = widest_scanned_unit(&mut fonts, text).ceil() as usize + 1;
        assert!(min_width < 120, "{min_width}");
        for width in (min_width..=420).step_by(1) {
            let rows = rows(&mut fonts, text, width as f32);
            for pair in rows.windows(2) {
                let last = pair[0].trim_end_matches(char::is_whitespace).chars().last();
                let first = pair[1]
                    .trim_start_matches(char::is_whitespace)
                    .chars()
                    .next();
                assert!(
                    !last.is_some_and(is_line_end_prohibited)
                        && !first.is_some_and(is_line_start_prohibited),
                    "{width}: {rows:?}"
                );
            }
        }
    }

    /// Row widths of `text` wrapped at `width` points, with the number of glyphs with a width
    /// in each row.
    fn row_widths(fonts: &mut FontsImpl, text: &str, width: f32) -> Vec<(String, f32, usize)> {
        let mut job = LayoutJob::single_section(
            text.to_owned(),
            TextFormat::simple(FontId::proportional(14.0), crate::Color32::WHITE),
        );
        job.wrap.max_width = width;
        let galley = layout(fonts, 1.0, Arc::new(job));
        galley
            .rows
            .iter()
            .map(|row| {
                let visible = row.glyphs.iter().filter(|g| g.advance_width > 0.0).count();
                (row.text(), row.size.x, visible)
            })
            .collect()
    }

    /// Chinese and Japanese rows never overflow the width (egui's rounding allows half a point)
    /// unless a single glyph is wider, also where a run kinsoku keeps together is wider than a
    /// row and must break: it breaks where the row is full, and a run moved to the next row
    /// whole fits there.
    #[test]
    fn cjk_rows_never_overflow() {
        let mut fonts = fonts();
        let texts = CJK.iter().copied().chain([
            "字「『（【〔〈《“‘字’”》〉〕】）』」。字「『（【〔〈《“‘字’”》〉〕】）』」、字",
            "「えっ？」「ちょっと待って！」――彼は言った。「そんな……まさか」。ァィゥェォッャュョ",
            "。。。。。。、、、、、，，，，！！！？？？：：；；・・・ーーーーッッッ",
        ]);
        for text in texts {
            let mut width = 1.0;
            while width <= 240.0 {
                for (row, row_width, visible) in row_widths(&mut fonts, text, width) {
                    assert!(
                        row_width <= width + 0.5 + 1e-3 || visible <= 1,
                        "{width}: {row:?} is {row_width} wide"
                    );
                }
                width += 0.5;
            }
        }
    }

    /// "——" and "……" stay whole at every width that holds them, in any script — also where
    /// the rest of the row is a word wider than the row ("over……").
    #[test]
    fn dashes_and_ellipses_stay_whole() {
        let mut fonts = fonts();
        let pair_width = ["——", "……", "――"]
            .map(|t| width(&mut fonts, t))
            .into_iter()
            .fold(0.0, f32::max);
        for text in [
            "Wait… what? He said——no, he shouted——that it was over……",
            "Loading…… please wait… still loading… — done — really—really—done",
            "连接中断……正在重新连接——请稍候。服务器没有响应……请检查网络",
            "コーヒー――それとも……紅茶？",
        ] {
            let mut width = pair_width.ceil();
            while width <= 300.0 {
                let rows = rows(&mut fonts, text, width);
                for pair in rows.windows(2) {
                    let (Some(last), Some(first)) =
                        (pair[0].chars().last(), pair[1].chars().next())
                    else {
                        continue;
                    };
                    assert!(!is_inseparable(last, first), "{width}: {rows:?}");
                }
                width += 0.5;
            }
        }
    }

    /// A Chinese or Japanese character stays with its combining mark or variation selector at
    /// every width, even where the shaper gives the selector the base's character (egui's
    /// default fonts have no glyph for it) and even when the row is narrower than one glyph.
    #[test]
    fn clusters_stay_whole() {
        let mut fonts = fonts();
        let text = "葛\u{E0100}字\u{FE0F}か\u{3099}き\u{3099}葛\u{E0100}辻\u{E0101}";
        for width in (0..=120).map(|w| w as f32 * 0.5) {
            let mut job = LayoutJob::single_section(
                text.to_owned(),
                TextFormat::simple(FontId::proportional(14.0), crate::Color32::WHITE),
            );
            job.wrap.max_width = width;
            let galley = layout(&mut fonts, 1.0, Arc::new(job));
            for row in galley.rows.iter().skip(1) {
                assert!(
                    row.glyphs.first().is_some_and(|g| g.advance_width > 0.0),
                    "{width}: a row starts inside a cluster: {:?}",
                    galley.rows.iter().map(|r| r.text()).collect::<Vec<_>>()
                );
            }
        }
    }

    /// The upstream cases (`text_layout::tests::test_cjk` and `test_pre_cjk`) keep their rows.
    #[test]
    fn upstream_cjk_rows_are_unchanged() {
        let mut fonts = fonts();
        let text = "日本語とEnglishの混在した文章";
        let mut rows_at = |width: f32| {
            let mut job = LayoutJob::single_section(text.into(), TextFormat::default());
            job.wrap.max_width = width;
            let galley = layout(&mut fonts, 1.0, Arc::new(job));
            galley.rows.iter().map(|r| r.text()).collect::<Vec<_>>()
        };
        assert_eq!(
            rows_at(90.0),
            ["日本語と", "Englishの混在", "した文章"],
            "90 pt"
        );
        assert_eq!(
            rows_at(110.0),
            ["日本語とEnglish", "の混在した文章"],
            "110 pt"
        );
    }
}
