//! `space3d new <name>`.

use crate::Ctx;
use crate::args::NewArgs;
use crate::process::Cmd;
use crate::template::{ScaffoldOptions, Space3dSource, scaffold, validate_name};
use anyhow::{Context, Result};
use space3d_github::templates::Target;

/// Scaffold a project and print the next steps.
pub fn run(ctx: &Ctx, args: &NewArgs) -> Result<()> {
    validate_name(&args.name)?;
    let dest = match &args.dir {
        Some(d) if d.is_absolute() => d.clone(),
        Some(d) => ctx.cwd.join(d),
        None => ctx.cwd.join(&args.name),
    };
    let source = match &args.space3d_path {
        Some(p) => Space3dSource::from_checkout(&if p.is_absolute() { p.clone() } else { ctx.cwd.join(p) })?,
        None => Space3dSource::Git {
            url: args.git.clone(),
            branch: args.branch.clone(),
            rev: args.rev.clone(),
        },
    };
    let ci_targets = if args.no_ci {
        None
    } else {
        Some(Target::parse_list(&args.targets).context("--targets")?)
    };
    let opts = ScaffoldOptions {
        name: args.name.clone(),
        template: args.template,
        source,
        ci_targets,
        force: args.force,
    };
    if ctx.exec.dry_run {
        eprintln!(
            "[dry-run] would create a `{}` project in {}",
            args.template.dir_name(),
            dest.display()
        );
        return Ok(());
    }
    let written = scaffold(&dest, &opts)?;
    if ctx.exec.verbose {
        for path in &written {
            eprintln!("  created {}", path.display());
        }
    }
    // The templates are rustfmt-clean for typical names; a long project name can push a line
    // over the limit, so format the generated sources (best effort — rustfmt may be missing).
    let sources: Vec<_> = written
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .map(|p| dest.join(p))
        .collect();
    if !sources.is_empty() {
        let _ = ctx.exec.probe(
            &Cmd::new("rustfmt")
                .args(["--edition", "2024"])
                .args(&sources)
                .cwd(&dest),
        );
    }
    let shown = dest.strip_prefix(&ctx.cwd).unwrap_or(&dest);
    println!(
        "Created `{}` ({} template, {} files) in {}\n\nNext steps:\n  cd {}\n  space3d run              # play on desktop\n  space3d serve            # play in the browser (http://localhost:8080)\n  space3d test             # unit, input-simulation and screenshot tests\n  space3d stories          # browse the storybook\n  space3d doctor           # check web/Android/iOS tooling",
        args.name,
        args.template.dir_name(),
        written.len(),
        shown.display(),
        shown.display()
    );
    Ok(())
}
