# Test fonts

`Space3dTestCJK-Regular.otf` (57 KB) is a subset of **Noto Sans JP 1.004** (© 2014, 2015 Adobe
Systems Incorporated, [SIL Open Font License 1.1](OFL.txt)) holding only the characters that
appear in `../kinsoku.rs`: the Japanese and Traditional Chinese test paragraphs, plus the
Simplified Chinese characters the font has. It is used by the kinsoku layout test and the
`UI/Text/Kinsoku` golden screenshot story, never by the engine or by games. egui's own fonts
have no Han characters or kana, and system fonts differ between machines, which golden
screenshots can't have.

It is renamed "Space3d Test CJK" (the OFL asks modified versions not to use the original
font's name), and has no vertical metrics, `GSUB` or `GPOS` tables (egui lays text out
horizontally; kerning doesn't matter to line breaking).

Source: `NotoSansJP-Regular.otf` 1.004 as shipped in the `printpdf` 0.12.8 crate's
`examples/assets/fonts/` (crates.io), which is the Google Fonts release of Noto Sans JP.

## Regenerating it

After adding characters to `../kinsoku.rs` (a test fails with "missing … in …"), with
[fonttools](https://pypi.org/project/fonttools/) (`pip install fonttools`) and the source font:

```sh
cd crates/space3d-ui/tests
python3 -c "import sys; s=open('kinsoku.rs', encoding='utf-8').read(); \
  sys.stdout.write(''.join(sorted({c for c in s if ord(c) > 0x7e})))" > /tmp/chars.txt
pyftsubset NotoSansJP-Regular.otf --text-file=/tmp/chars.txt --unicodes=U+0020 \
  --drop-tables+=vhea,vmtx,VORG,GSUB,GPOS --layout-features= \
  --name-IDs='*' --name-languages='*' --output-file=/tmp/subset.otf
```

then rename it as the current file is (name IDs 1, 3, 4, 5, 6, 10, 16 and the CFF names to
"Space3d Test CJK" / `Space3dTestCJK-Regular`, drop name ID 7, the Noto trademark line; keep
the copyright, designer and license records), e.g. with `fontTools.ttLib.TTFont`, and
re-bless the story (`SPACE3D_UPDATE_SNAPSHOTS=1 cargo test -p space3d-ui --test kinsoku`).
