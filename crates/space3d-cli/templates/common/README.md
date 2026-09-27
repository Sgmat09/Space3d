# {{title}}

A [Space3d](https://github.com/SpaceCorps/Space3d) game (created from the `{{template}}`
template with `space3d new`).

## Play

```sh
space3d run                    # desktop window          (cargo run)
space3d serve                  # browser, http://localhost:8080 (wasm build + local server)
space3d build --target android # APK in dist/android      (see android/README.md)
space3d build --target ios --sim                          # (see ios/README.md)
```

Controls: {{controls}}

## Develop

| | |
|---|---|
| `src/lib.rs` | the game: `build` (engine plugins + game) and `GamePlugin` (gameplay only) |
| `src/main.rs` | desktop/web entry point — `space3d::main!({{crate_name}}::build)` |
| `src/stories.rs` | storybook: isolated, parameterized scenes |
| `src/bin/stories.rs` | storybook viewer / gallery binary |
| `src/bin/editor.rs` | the Space3d editor with your scene loaded (`space3d editor`) |
| `tests/input.rs` | gameplay driven by simulated keyboard/touch input, tick by tick |
| `tests/screenshots.rs` | golden screenshot tests (incl. every story) |
| `assets/` | textures, models, sounds |
| `web/index.html` | page for the web build |
| `mobile/` | library crate for Android and the iOS Xcode project (same `build` function) |

```sh
space3d test                        # all tests + screenshot report (target/space3d-report/index.html)
space3d test --update-snapshots     # accept new rendering as the golden images
space3d stories                     # storybook viewer (story tree, knobs, live viewport)
space3d editor                      # scene editor: hierarchy, inspector, gizmos, play/pause/step
space3d stories --headless --out target/stories   # render the storybook gallery
space3d doctor                      # check web / Android / iOS tooling
```

Screenshot goldens live in `tests/__screenshots__/`: the first run writes them (commit them),
later runs compare against them. Tests that need a GPU skip themselves when there is none.

Handy environment variables: `SPACE3D_EXIT_AFTER_FRAMES=120` (quit after N frames — smoke
tests), `SPACE3D_HEADLESS=1` (no window), `RUST_LOG=debug`.

## Ship

`.github/workflows/space3d.yml` runs the tests (with a software GPU for screenshots) on every
push, builds desktop/web/Android/iOS, deploys the web build to GitHub Pages, and attaches
everything to a GitHub release when you push a `v*` tag. Locally:

```sh
space3d publish --tag v0.1.0 --dry-run   # what would be built and uploaded
space3d publish --tag v0.1.0             # needs GITHUB_TOKEN or `gh auth login`
```
