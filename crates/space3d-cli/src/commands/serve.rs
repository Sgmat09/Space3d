//! `space3d serve`: web build + static server (`space3d_net::HttpServer`).

use crate::Ctx;
use crate::args::{BuildArgs, Platform, ServeArgs};
use crate::project::Project;
use anyhow::{Context, Result};
use space3d_net::{HttpServer, ServerHandle};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Build for the web (unless `--no-build`), serve `dist/web` until Ctrl-C.
pub fn run(ctx: &Ctx, args: &ServeArgs) -> Result<()> {
    let project = Project::load(&ctx.cwd, args.cargo.package.as_deref())?;
    let build_args = BuildArgs {
        target: Platform::Web,
        cargo: args.cargo.clone(),
        out: None,
        install_tools: args.install_tools,
        sim: false,
        no_wasm_opt: !args.cargo.release,
        aab: false,
    };
    let dist = project.dist_dir(Platform::Web.name());
    if !args.no_build {
        super::build::build(ctx, &project, &build_args)?;
    }
    if ctx.exec.dry_run {
        eprintln!("[dry-run] would serve {} on port {}", dist.display(), args.port);
        return Ok(());
    }
    let ip = if args.host { [0, 0, 0, 0] } else { [127, 0, 0, 1] };
    let handle = start(&dist, (ip, args.port))?;
    println!("serving {} at {}/  (Ctrl-C to stop)", dist.display(), handle.url());
    if !args.watch {
        return handle.wait().context("server stopped");
    }
    watch(ctx, &project, &build_args, &handle)
}

/// Start the static server used by `serve`: SPA fallback, COOP/COEP (so wasm threads can use
/// `SharedArrayBuffer`) and `Cache-Control: no-store`.
pub fn start(dir: &Path, addr: ([u8; 4], u16)) -> Result<ServerHandle> {
    anyhow::ensure!(
        dir.join("index.html").is_file(),
        "{} has no index.html — run `space3d build --target web` first",
        dir.display()
    );
    HttpServer::new()
        .bind(addr)
        .serve_spa(dir)
        .cross_origin_isolated()
        .no_cache()
        .start()
        .with_context(|| format!("cannot listen on {}:{}", std::net::Ipv4Addr::from(addr.0), addr.1))
}

/// Poll source timestamps and rebuild on change (the server keeps serving the same directory).
fn watch(ctx: &Ctx, project: &Project, build: &BuildArgs, handle: &ServerHandle) -> Result<()> {
    let roots: Vec<PathBuf> = ["src", "assets", "web", "Cargo.toml"]
        .iter()
        .map(|p| project.dir().join(p))
        .collect();
    let mut last = newest_mtime(&roots);
    println!("watching src/, assets/, web/ and Cargo.toml for changes");
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let now = newest_mtime(&roots);
        if now > last {
            last = now;
            println!("change detected, rebuilding…");
            match super::build::build(ctx, project, build) {
                Ok(_) => println!("rebuilt — reload {}/", handle.url()),
                Err(e) => eprintln!("build failed: {e:#}"),
            }
        }
    }
}

/// Newest modification time below `roots` (files or directories).
pub fn newest_mtime(roots: &[PathBuf]) -> Option<SystemTime> {
    fn visit(path: &Path, newest: &mut Option<SystemTime>) {
        let Ok(meta) = std::fs::metadata(path) else { return };
        if let Ok(m) = meta.modified() {
            *newest = (*newest).max(Some(m));
        }
        if meta.is_dir()
            && let Ok(entries) = std::fs::read_dir(path)
        {
            for e in entries.flatten() {
                visit(&e.path(), newest);
            }
        }
    }
    let mut newest = None;
    for root in roots {
        visit(root, &mut newest);
    }
    newest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_dist_with_isolation_headers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<html>game</html>").unwrap();
        std::fs::write(dir.path().join("game_bg.wasm"), b"\0asm\x01\0\0\0").unwrap();
        let handle = start(dir.path(), ([127, 0, 0, 1], 0)).unwrap();
        let http = space3d_net::Http::new();
        let get = |path: &str| {
            let mut task = http.get(format!("{}{path}", handle.url()));
            loop {
                if let Some(r) = task.poll() {
                    break r.unwrap();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let index = get("/");
        assert_eq!(index.status, 200);
        assert_eq!(index.text(), "<html>game</html>");
        assert_eq!(index.header("cross-origin-opener-policy"), Some("same-origin"));
        assert_eq!(index.header("cross-origin-embedder-policy"), Some("require-corp"));
        assert!(index.header("cache-control").unwrap().contains("no-store"));
        let wasm = get("/game_bg.wasm");
        assert_eq!(wasm.header("content-type"), Some("application/wasm"));
        // SPA fallback.
        assert_eq!(get("/level/3").text(), "<html>game</html>");
        handle.shutdown();
    }

    #[test]
    fn refuses_empty_dist() {
        let dir = tempfile::tempdir().unwrap();
        assert!(start(dir.path(), ([127, 0, 0, 1], 0)).is_err());
    }

    #[test]
    fn mtime_scan() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(newest_mtime(&[dir.path().join("missing")]), None);
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        assert!(newest_mtime(&[dir.path().join("src")]).is_some());
    }
}
