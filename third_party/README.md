# Patched third-party crates

Crates from crates.io that Space3d ships with a local change, applied to every Space3d build
through `[patch.crates-io]` in the root `Cargo.toml`. They are not workspace members (the root
`Cargo.toml` excludes `third_party/`): `cargo fmt`, `cargo clippy --workspace` and
`cargo test --workspace` leave them alone, and they keep upstream's formatting
(`third_party/rustfmt.toml` holds rustfmt's defaults, which egui uses).

| crate | what Space3d changes | patch |
|---|---|---|
| [`epaint`](epaint) | kinsoku (禁則処理) line breaking for Chinese and Japanese | [`epaint.kinsoku.patch`](epaint.kinsoku.patch) |

## epaint: kinsoku line breaking

Vendored version: **epaint 0.36.2** (egui commit `49682f8baa058bf49e011035cfbd6e825f88a5ef`, `crates/epaint`).

epaint is egui's paint and text layout crate, © Emil Ernerfeldt and the egui contributors,
dual-licensed MIT or Apache-2.0. `epaint/` is the crate exactly as published on crates.io
(`.cargo_vcs_info.json` names the upstream commit; `Cargo.toml.orig` is upstream's manifest,
`Cargo.lock` the lock file it was published with, used only when its tests run on their own)
plus `LICENSE-MIT` and `LICENSE-APACHE` from the egui repository, which the published crate
lacks, plus the patch below. Everything else is untouched, so `epaint.kinsoku.patch` is the
complete difference to the published crate (`scripts/update-epaint.py --check` proves it).

### Why

egui breaks a row at a space, after a Han character or kana, before one, and as fallbacks at a
dash, after ASCII punctuation, or anywhere. Its only CJK rule is a short list of marks that may
not follow a Han character or kana, which misses the full-width forms Chinese and Japanese
actually use: a Chinese or Japanese line starts with "，" "？" "！" "：" "）" "…", a Japanese
one with "ッ" after "ロ" (the fallback breaks before a small kana anyway), with "、" after a
Latin word, with "）" after "%", and lines end with "（" or "「". No hook reaches egui's row
breaking, so the rules live in a patched epaint. The Space3d test corpus
(`space3d-ui/tests/kinsoku.rs`, 3603 layouts) has 2636 such violations with upstream epaint,
0 with the patch.

The patch started as the one the SpaceCorps game carried in its own `third_party/epaint`
(SpaceCorps2027 commits b8af874, "kinsoku line breaking for Chinese and Japanese", and e531f10,
"kinsoku around Latin words and numbers, and none of it in German"), which Space3d now owns so
that games don't maintain a copy. It was reviewed and rewritten as a self-contained module:

* **`src/text/kinsoku.rs`** (new, public as `epaint::text::kinsoku`, also
  `egui::epaint::text::kinsoku`): the character classes and `line_break(before, after)`, which
  answers `Prohibited`, `Allowed` or `Upstream` for every position between two characters;
  crate-private, the glyph-level part the hook uses (`Scan`, which walks a paragraph and also
  looks past spaces and into clusters, and the fit checks of the fallbacks); the unit tests.
* **`src/text/text_layout.rs`**, `RowBreakCandidates` (the hook, ~60 lines): `add` asks
  `kinsoku::Scan` first. `Prohibited` records the position only for the fallbacks (below);
  `Allowed` makes it a word-boundary candidate (`pre_cjk`); otherwise egui's own rules run
  unchanged. `get` (called when a glyph overflows the row) takes egui's word-boundary, dash
  and punctuation candidates only if the Chinese or Japanese text they leave for the next row
  fits there; then two new fallbacks before egui's `any`: `any_allowed`, the last position
  kinsoku doesn't prohibit, if the run kinsoku keeps together after it fits on the next row
  whole (so even a word wider than the row doesn't break right before its "、"), and
  `any_separable`, the last position not inside "——" "……" or a cluster; `any` itself never
  splits a cluster.
* **`src/text/mod.rs`**: `pub mod kinsoku;` (one line).

`font.rs` (egui's `is_cjk_break_allowed` list) is untouched: kinsoku's list is a superset.

### The rules

Kinsoku applies to a position when a character next to it is Chinese or Japanese (`is_cjk`:
Han characters in all planes, kana, Bopomofo, CJK symbols and punctuation, full-width forms,
half-width katakana; not Hangul — Korean breaks at spaces, as upstream). A space breaks as
egui decides, except next to a bracket (below). Classes follow JIS X 4051 and the W3C
JLREQ/CLREQ ("strict" line breaking):

| rule | characters |
|---|---|
| may not start a line (行頭禁則) | closing brackets and quotes `）］｝〕〉》」』】〙〗〛〞〟｠｣’”»)]}'"`; hyphens `‐–゠〜～`; `？！‼⁇⁈⁉?!`; middle dots, colons, semicolons `・･·‧：；:;`; full stops `。．｡.`; commas `、，､,`; iteration marks `ヽヾゝゞ々〻`; prolonged sound mark `ーｰ`; small kana `ぁぃぅぇぉっゃゅょゎゕゖァィゥェォッャュョヮヵヶㇰ-ㇿｧ-ｯ` (and the small kana extension); sound marks `゛゜ﾞﾟ`; postfixed signs `％%‰°′″℃℉¢￠`; ellipses `…‥⋯` (plus the vertical and small forms) |
| may not end a line (行末禁則) | opening brackets and quotes `（［｛〔〈《「『【〘〖〚〝｟｢‘“«([{`; prefixed signs `￥＄￡＃` |
| inseparable (分離禁止) | `——` `――` `……` `‥‥` `⋯⋯`, `〳〵` `〴〵` |
| also kept together | a Chinese or Japanese character and its combining mark or variation selector (か + ゙, 葛 + VS17, 字 + U+FE0F) — as characters, and as glyphs: the shaper gives a selector the font has no glyph for the base's character and no width, and no row starts with such a glyph, even in a row narrower than one glyph; word joiners; a straight quote `"` `'` with the CJK text on both sides (it may open or close a quote, Unicode class QU) |
| across spaces | spaces between a bracket and its text (「　重要　」, （ 注意 ）): no line ends with the opening bracket and its spaces or starts with the spaces and the closing mark (as Unicode's `OP SP* ×` and `× CL` after spaces) |
| a word boundary egui lacked | after CJK punctuation before a Latin word or number (「、」\|「Thulium」, 「。」\|「4-2」), before an opening bracket after a Latin word, number or mark (「Skylab」\|「「」, 「！」\|「（」), between any other pair with a CJK character that isn't prohibited (after ASCII marks: 「!」\|「中」) |

Latin words and numbers inside CJK text stay whole (egui's own rules: no break inside them),
get breaks before and after them, and an ASCII mark never breaks before a CJK closing one
(「+25%」\|「）」).

A run kinsoku keeps together moves to the next row whole when it fits there. A run wider than
a row (「茶？ェェッ！」 in a 60 pt column, 「m、」 in a column narrower than those two glyphs)
still breaks, where the row is full, as upstream egui breaks a word wider than the row — but
not inside "——" or "……" while such a pair fits on a row, and never inside a cluster. No break
kinsoku chooses or rules out makes a row of Chinese or Japanese text wider than the wrap width
(plus the half point egui's rounding allows) unless one glyph is wider; the unit test
`cjk_rows_never_overflow` checks paragraphs at every half point from 1 to 240 pt. As with
upstream egui, the rest of a Latin word can still overflow a row by less than a glyph in
columns only two or three characters wide.

**Text without Chinese or Japanese characters lays out as with upstream egui**: every such
pair gets `Upstream`, so `RowBreakCandidates` behaves as before (the unit test
`text_without_cjk_is_left_to_egui` checks over a million pairs of Latin, Greek, Cyrillic,
Arabic, Hangul, punctuation, currency and emoji characters; the fallbacks and fit checks only
act on Chinese or Japanese glyphs). The exception, in every script as in Unicode line breaking
(UAX #14), is ellipses and doubled dashes: no line starts with "…" "‥" "⋯" after a non-space
("wait…"), and "——" "――" "……" "‥‥" "⋯⋯" are not split while the pair fits on a row. Text
with them can wrap differently from upstream at narrow widths: where "over……" doesn't fit
but "r……" does, the row breaks inside the word ("ove" \| "r……", as upstream breaks any word
wider than the row) instead of before the ellipsis ("over" \| "……"); upstream splits a pair
at the row's end ("d—" \| "—th") where the patch moves it whole. In particular “ ‘ « count
as opening brackets only next to Chinese or Japanese characters, because they close quotes in
German („Verwaltung“): an earlier version of the game's patch started German lines with “.

Improvements over the game's patch: prefixed signs (「￥」\|「1,200」), a Latin word before
「。」 or 「、」 and 「Q」\|「」」 in the last-resort fallback, doubled dashes and ellipses,
half-width katakana, ASCII opening brackets before CJK text, combining marks, and the full JIS
class lists, spaces next to brackets, straight quotes and rows that never overflow; on the
test corpus above the game's patch has 370 violations, this one 0. Not done
(egui can't): hanging punctuation (ぶら下げ), justification, and pushing a character back to
the previous line (追い込み) instead of forward.

### Tests

* `epaint/src/text/kinsoku.rs` unit tests (in the vendored crate; CI runs them): every class
  and rule, egui's list included, Latin words and numbers, Chinese and straight quotes, spaces
  next to brackets (pairs, and paragraphs at every width), the million-pair "no CJK, no change"
  check, German quotes at every width, runs of prohibited marks, a word wider than the row,
  rows that never overflow, dashes and ellipses kept whole in any script, clusters kept whole,
  and upstream's own `test_cjk`/`test_pre_cjk` rows. With the hook removed, 1084 violations
  and six failing tests. Run:
  `cargo test --manifest-path third_party/epaint/Cargo.toml --target-dir target/third_party --lib text::`
  (all of epaint's own tests pass too).
* `crates/space3d-ui/tests/kinsoku.rs`: Japanese, Traditional and Simplified Chinese paragraphs
  (and one with spaces around brackets) with a CJK test font (a Noto Sans JP subset,
  `crates/space3d-ui/tests/fonts/`) at three sizes and every fourth width, checked against the
  test's own lists, which look past spaces; the golden story
  `UI/Text/Kinsoku` (`tests/__screenshots__/UI/Text/Kinsoku.png`); and
  `epaint_resolves_to_the_vendored_kinsoku_copy`, which fails when `Cargo.lock` shows epaint
  from crates.io or the patch unused.
* CI (`lint` job): `scripts/update-epaint.py --check` — the vendored copy is exactly the
  published crate + `epaint.kinsoku.patch`, its version is egui's, and `cargo metadata`
  resolves epaint to it — then the vendored unit tests.

### Updating epaint (egui bumps)

`[patch.crates-io]` only applies while `third_party/epaint` has the version egui depends on.
When egui moves (dependabot opens one PR for the `egui*` group, see `.github/dependabot.yml`),
cargo prints `warning: Patch 'epaint v0.36.2 (…)' was not used in the crate graph` and builds
crates.io's epaint without kinsoku; `epaint_resolves_to_the_vendored_kinsoku_copy` and the CI
check then fail on purpose. To update, on the bump branch:

```sh
python3 scripts/update-epaint.py          # the version egui now asks for (or pass one)
```

It fetches the published crate (local cargo registry, else crates.io with the checksum checked
against the index), applies `epaint.kinsoku.patch`, replaces `third_party/epaint` (keeping the
licenses), rewrites the patch for the new line numbers and this file's "Vendored version" line,
checks that cargo resolves epaint to the copy, and runs the kinsoku tests. Commit
`third_party/` with `Cargo.lock`. If the patch doesn't apply, the script leaves the new crate
with the rejected hunks (`*.rej`) in a temporary directory: copy it over `third_party/epaint`,
port the hunks by hand (the hook: the new fields of `RowBreakCandidates`, its `add`, `get` and
`forget_before_idx`, and the call to `get` in `line_break`), then
`python3 scripts/update-epaint.py --refresh` and the tests. After any hand edit of
`third_party/epaint`, run `--refresh` too, or the CI check fails.

Warnings: cargo caps lints (`--cap-lints allow`) only for crates from a registry or git, not
for a path dependency like `third_party/epaint`. rustc's warnings in the vendored code show up
in Space3d builds, and CI's `RUSTFLAGS: -D warnings` turns them into errors in every job, as
for Space3d's own code (which the copy now is). epaint 0.36.2 builds without a warning
(native, its tests, wasm32). The script runs the vendored tests with `-D warnings`, so a new
epaint or a Rust update that brings a warning fails there, on the bump branch: fix it in the
copy (or `#[allow]` it) and `--refresh`, which records the change in `epaint.kinsoku.patch`.

Drop the copy (and the `[patch]`, the CLI template line and the docs) if egui ever ships
equivalent rules; the space3d-ui tests say whether it does.

### Games

Games get kinsoku with the same one-line `[patch.crates-io]` (git form, same `rev` as their
`space3d` dependency); see `docs/ui.md`, "Chinese and Japanese line breaking". `space3d new`
writes it; `space3d doctor` warns when it is missing, unused, not Space3d's copy (a game's own
older one), or from another Space3d revision than `space3d`.
