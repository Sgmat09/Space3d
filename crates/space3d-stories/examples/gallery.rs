//! Render the built-in engine storybook into a static HTML gallery.
//!
//! ```text
//! cargo run -p space3d-stories --example gallery -- [out_dir] [filter]
//! ```
//!
//! Writes `<out_dir>/index.html`, `<out_dir>/manifest.json` and `<out_dir>/images/**.png`
//! (default `target/storybook`). Exits with status 1 if any story failed.

use space3d_stories::{StoryStatus, engine_storybook, screenshot_all};

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "target/storybook".to_string());
    let filter = args.next();
    let results = screenshot_all(&engine_storybook(), &out, filter.as_deref())?;
    for r in &results {
        let detail = r.message.as_deref().unwrap_or("");
        println!(
            "{:<9} {:<40} {:>7.1} ms  {detail}",
            format!("{:?}", r.status),
            r.id,
            r.duration_us as f64 / 1000.0
        );
    }
    println!("gallery: {}/index.html", out);
    if results.iter().any(|r| r.status == StoryStatus::Failed) {
        std::process::exit(1);
    }
    Ok(())
}
