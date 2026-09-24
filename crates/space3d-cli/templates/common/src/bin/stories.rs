//! Storybook for {{title}}.
//!
//! ```text
//! space3d stories                             # interactive viewer (--filter selects the first story)
//! space3d stories --list                      # list story ids
//! space3d stories --headless --out shots      # render every story + shots/index.html gallery
//! ```
//! (`space3d stories ...` runs `cargo run --bin stories -- ...`.)

use space3d::editor::run_storybook_with;
use space3d::prelude::*;
use space3d::stories::{StoryStatus, screenshot_all};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let book = {{crate_name}}::stories::storybook();
    let filter = value("--filter");

    if flag("--list") {
        for story in book.select(filter.as_deref()) {
            println!("{:<32} {}", story.id, story.description);
        }
        return;
    }

    if flag("--headless") {
        let out = value("--out").unwrap_or_else(|| "target/stories".into());
        let results = screenshot_all(&book, &out, filter.as_deref()).expect("cannot write the gallery");
        let mut failed = 0;
        for r in &results {
            let status = match r.status {
                StoryStatus::Rendered => "ok     ",
                StoryStatus::Skipped => "skipped",
                StoryStatus::Failed => {
                    failed += 1;
                    "FAILED "
                }
            };
            println!("{status} {:<32} {}", r.id, r.message.as_deref().unwrap_or_default());
        }
        println!("gallery: {out}/index.html");
        std::process::exit(i32::from(failed > 0));
    }

    // Interactive viewer (space3d-editor): story tree + search, live viewport, knobs, tick
    // controls and screenshots.
    let initial_story = book.select(filter.as_deref()).first().map(|s| s.id.clone());
    run_storybook_with(
        book,
        StorybookConfig {
            title: "{{title}} — storybook".into(),
            initial_story,
            ..StorybookConfig::default()
        },
    );
}
