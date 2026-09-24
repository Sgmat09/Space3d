//! `space3d ci init`.

use crate::Ctx;
use crate::args::CiInitArgs;
use crate::process::Cmd;
use crate::project::Project;
use crate::template::{workflow_options, write_workflow_with};
use anyhow::{Context, Result, bail};
use space3d_github::templates::{Target, WORKFLOW_PATH};
use std::path::{Path, PathBuf};

/// Write `.github/workflows/space3d.yml` for the project, at the repository root.
pub fn init(ctx: &Ctx, args: &CiInitArgs) -> Result<()> {
    let targets = Target::parse_list(&args.targets).context("--targets")?;
    if targets.is_empty() {
        bail!("--targets is empty (use e.g. desktop,web,android,ios)");
    }
    let project = Project::load(&ctx.cwd, None)?;
    let bin = project.package().main_bin(args.bin.as_deref())?;
    let package_dir = project.dir().to_path_buf();
    let root = repo_root(ctx, &project);
    let path = root.join(WORKFLOW_PATH);
    if path.exists() && !args.force {
        bail!("{} already exists (use --force to overwrite)", path.display());
    }
    let names: Vec<&str> = targets.iter().map(|t| t.name()).collect();
    let nested = !same_dir(&root, &package_dir);
    let mut opts = workflow_options(&bin, !args.no_pages);
    if let Some(version) = project.locked_version("wasm-bindgen") {
        opts.wasm_bindgen_version = version;
    }
    if ctx.exec.dry_run {
        eprintln!("[dry-run] would write {} for {}", path.display(), names.join(", "));
    } else {
        write_workflow_with(&root, &package_dir, &targets, &opts)?;
        println!("wrote {} ({})", path.display(), names.join(", "));
    }
    if nested {
        let rel = package_dir.strip_prefix(&root).unwrap_or(&package_dir);
        eprintln!(
            "note: GitHub only reads workflows at the repository root ({}), but the package lives in {}.\n      \
             The jobs run from the root: add `-p {}` to their cargo commands or set\n      \
             `defaults: {{ run: {{ working-directory: {} }} }}`, and check the artifact paths.",
            root.display(),
            rel.display(),
            project.package().name,
            rel.display()
        );
    }
    if ctx.exec.dry_run {
        return Ok(());
    }
    if targets.contains(&Target::Web) && !args.no_pages {
        println!("  GitHub Pages: set Settings > Pages > Source to \"GitHub Actions\" to deploy the web build");
    }
    if targets.contains(&Target::Ios) && !package_dir.join("ios").join("project.yml").exists() {
        println!("  iOS: the workflow expects ios/project.yml (XcodeGen) or ios/{bin}.xcodeproj");
    }
    Ok(())
}

/// Where GitHub looks for workflows: the git top level containing the package, else (not a git
/// checkout yet) the cargo workspace root.
pub fn repo_root(ctx: &Ctx, project: &Project) -> PathBuf {
    ctx.exec
        .probe(
            &Cmd::new("git")
                .args(["rev-parse", "--show-toplevel"])
                .cwd(project.dir()),
        )
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| project.workspace_root.clone())
}

/// `true` if both paths name the same directory (symlinks such as macOS `/tmp` resolved).
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_dir_resolves_symlinks_and_dots() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir(&a).unwrap();
        assert!(same_dir(&a, &a.join("..").join("a")));
        assert!(!same_dir(&a, tmp.path()));
    }
}
