//! `space3d publish --tag vX.Y.Z`: release builds → archives in `dist/release/` → GitHub
//! release (via `space3d-github`) with the archives attached.

use super::build;
use crate::Ctx;
use crate::args::{BuildArgs, CargoSel, Platform, PublishArgs};
use crate::process::Cmd;
use crate::project::Project;
use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use space3d_github::{GithubClient, NewRelease, Release, RepoRef, discover_token};
use std::path::{Path, PathBuf};

/// Build, package and publish.
pub fn run(ctx: &Ctx, args: &PublishArgs) -> Result<()> {
    validate_tag(&args.tag)?;
    let platforms = parse_platforms(&args.targets)?;
    let project = Project::load(&ctx.cwd, args.cargo.package.as_deref())?;
    let repo = match &args.repo {
        Some(r) => r.parse::<RepoRef>().context("--repo")?,
        None => origin_repo(ctx, &project)?,
    };
    let bin = project.package().main_bin(args.cargo.bin.as_deref())?;
    let release_dir = project.dir().join("dist").join("release");
    let mut assets = Vec::new();
    for platform in &platforms {
        let build_args = BuildArgs {
            target: *platform,
            cargo: CargoSel {
                release: true,
                ..args.cargo.clone()
            },
            out: None,
            install_tools: false,
            sim: false,
            no_wasm_opt: false,
            aab: false,
        };
        let out = if args.no_build {
            project.dist_dir(platform.name())
        } else {
            build::build(ctx, &project, &build_args)?
        };
        assets.push(package(ctx, *platform, &out, &release_dir, &bin, &args.tag)?);
    }
    let release = new_release(args, &bin);
    if ctx.exec.dry_run {
        println!(
            "[dry-run] would create release {} on {repo}:\n{}\n[dry-run] with assets:",
            args.tag,
            serde_json::to_string_pretty(&release)?
        );
        for a in &assets {
            println!("  {}", a.display());
        }
        return Ok(());
    }
    let mut client = GithubClient::new(discover_token());
    if let Some(url) = &args.api_url {
        client = client.with_base_url(url.clone());
    }
    if !client.has_token() {
        bail!("no GitHub token: export GITHUB_TOKEN (or GH_TOKEN), or log in with `gh auth login`");
    }
    let published = space3d_tasks::runtime().block_on(publish(&client, &repo, &release, &assets))?;
    println!(
        "published {} with {} asset(s): {}",
        published.tag_name,
        assets.len(),
        published.html_url
    );
    Ok(())
}

/// Create (or reuse) the release for `release.tag_name` and upload `assets`.
pub async fn publish(
    client: &GithubClient,
    repo: &RepoRef,
    release: &NewRelease,
    assets: &[PathBuf],
) -> Result<Release> {
    let existing = client.release_by_tag(repo, &release.tag_name).await?;
    let published = match existing {
        Some(r) => {
            eprintln!("release {} already exists; adding assets", r.tag_name);
            r
        }
        None => client.create_release(repo, release).await?,
    };
    for asset in assets {
        eprintln!("uploading {}", asset.display());
        client
            .upload_release_asset_file(&published, asset)
            .await
            .with_context(|| format!("uploading {}", asset.display()))?;
    }
    Ok(published)
}

/// The release payload.
pub fn new_release(args: &PublishArgs, bin: &str) -> NewRelease {
    let mut r = NewRelease::new(&args.tag);
    r.name = Some(format!("{bin} {}", args.tag));
    r.generate_release_notes = args.notes.is_none();
    r.body = args.notes.clone();
    r.draft = args.draft;
    r.prerelease = args.prerelease || args.tag.contains('-');
    r
}

/// `vMAJOR.MINOR.PATCH` with an optional `-prerelease` suffix.
pub fn validate_tag(tag: &str) -> Result<()> {
    let Some(rest) = tag.strip_prefix('v') else {
        bail!("tag `{tag}` must look like v1.2.3");
    };
    let core = rest.split_once('-').map_or(rest, |(c, _)| c);
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        bail!("tag `{tag}` must look like v1.2.3 (optionally v1.2.3-beta.1)");
    }
    Ok(())
}

/// Parse `desktop,web,android,ios`.
pub fn parse_platforms(list: &str) -> Result<Vec<Platform>> {
    let mut out = Vec::new();
    for part in list.split([',', ' ']).map(str::trim).filter(|p| !p.is_empty()) {
        let p = Platform::from_str(part, true)
            .map_err(|_| anyhow::anyhow!("unknown platform `{part}` (expected desktop, web, android, ios)"))?;
        if !out.contains(&p) {
            out.push(p);
        }
    }
    if out.is_empty() {
        bail!("no platforms given");
    }
    Ok(out)
}

/// Archive name for a platform build, e.g. `game-v1.0.0-macos-aarch64.tar.gz`.
pub fn asset_name(bin: &str, tag: &str, platform: Platform) -> String {
    match platform {
        Platform::Desktop => format!("{bin}-{tag}-{}-{}.tar.gz", std::env::consts::OS, std::env::consts::ARCH),
        Platform::Android => format!("{bin}-{tag}-android.apk"),
        Platform::Web | Platform::Ios => format!("{bin}-{tag}-{}.tar.gz", platform.name()),
    }
}

/// Turn a build directory into one release asset in `release_dir`.
fn package(ctx: &Ctx, platform: Platform, out: &Path, release_dir: &Path, bin: &str, tag: &str) -> Result<PathBuf> {
    let asset = release_dir.join(asset_name(bin, tag, platform));
    if !ctx.exec.dry_run {
        std::fs::create_dir_all(release_dir)?;
    }
    if platform == Platform::Android {
        let apk = if ctx.exec.dry_run {
            out.join("<built>.apk")
        } else {
            std::fs::read_dir(out)
                .with_context(|| format!("no Android build in {}", out.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .find(|p| p.extension().is_some_and(|e| e == "apk"))
                .with_context(|| format!("no .apk in {}", out.display()))?
        };
        if ctx.exec.dry_run {
            eprintln!("[dry-run] copy {} -> {}", apk.display(), asset.display());
        } else {
            std::fs::copy(&apk, &asset)?;
        }
    } else {
        // `tar` ships with macOS, Linux and Windows 10+.
        ctx.exec
            .run(&Cmd::new("tar").arg("-czf").arg(&asset).arg("-C").arg(out).arg("."))?;
    }
    Ok(asset)
}

/// `owner/name` from `git remote get-url origin`.
fn origin_repo(ctx: &Ctx, project: &Project) -> Result<RepoRef> {
    let url = ctx
        .exec
        .probe(&Cmd::new("git").args(["remote", "get-url", "origin"]).cwd(project.dir()))
        .context("no `origin` git remote; pass --repo owner/name")?;
    url.parse::<RepoRef>()
        .with_context(|| format!("origin `{url}` is not a GitHub repository; pass --repo owner/name"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_net::HttpServer;
    use space3d_net::server::axum::{self, body::Bytes, extract::Query, routing::post};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn args(tag: &str) -> PublishArgs {
        PublishArgs {
            tag: tag.into(),
            targets: "desktop,web".into(),
            repo: None,
            draft: false,
            prerelease: false,
            notes: None,
            no_build: false,
            api_url: None,
            cargo: CargoSel::default(),
        }
    }

    #[test]
    fn tags() {
        for ok in ["v1.2.3", "v0.1.0", "v10.20.30-beta.1"] {
            validate_tag(ok).unwrap();
        }
        for bad in ["1.2.3", "v1.2", "v1.2.x", "v1..3", "release"] {
            assert!(validate_tag(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn platforms_and_names() {
        assert_eq!(
            parse_platforms("web, desktop,web").unwrap(),
            vec![Platform::Web, Platform::Desktop]
        );
        assert!(parse_platforms("switch").is_err());
        assert!(parse_platforms("").is_err());
        assert_eq!(asset_name("g", "v1.0.0", Platform::Web), "g-v1.0.0-web.tar.gz");
        assert_eq!(asset_name("g", "v1.0.0", Platform::Android), "g-v1.0.0-android.apk");
        assert!(
            asset_name("g", "v1.0.0", Platform::Desktop).starts_with(&format!("g-v1.0.0-{}-", std::env::consts::OS))
        );
    }

    #[test]
    fn release_payload() {
        let r = new_release(&args("v2.0.0-rc.1"), "game");
        assert_eq!(r.name.as_deref(), Some("game v2.0.0-rc.1"));
        assert!(r.prerelease && r.generate_release_notes && !r.draft);
        let r = new_release(
            &PublishArgs {
                notes: Some("hi".into()),
                draft: true,
                ..args("v2.0.0")
            },
            "game",
        );
        assert!(!r.prerelease && !r.generate_release_notes && r.draft);
        assert_eq!(r.body.as_deref(), Some("hi"));
    }

    #[test]
    fn publishes_against_a_mock_github() {
        let uploads: Arc<Mutex<Vec<(String, usize)>>> = Arc::default();
        let created: Arc<Mutex<Option<serde_json::Value>>> = Arc::default();
        let (u, c) = (uploads.clone(), created.clone());
        // The upload URL must point back at this server; fill it in after binding.
        let base: Arc<Mutex<String>> = Arc::default();
        let b = base.clone();
        let server = HttpServer::new()
            .post_json("/repos/acme/game/releases", move |req: serde_json::Value| {
                *c.lock().unwrap() = Some(req.clone());
                Release {
                    id: 7,
                    tag_name: req["tag_name"].as_str().unwrap_or_default().to_owned(),
                    html_url: "https://github.com/acme/game/releases/tag/v1.0.0".into(),
                    upload_url: format!("{}/uploads/7/assets{{?name,label}}", b.lock().unwrap()),
                    ..Default::default()
                }
            })
            .route(
                "/uploads/7/assets",
                post(move |Query(q): Query<HashMap<String, String>>, body: Bytes| {
                    u.lock().unwrap().push((q["name"].clone(), body.len()));
                    async { axum::Json(serde_json::json!({"id": 1, "name": "x", "size": 1})) }
                }),
            )
            .start()
            .unwrap();
        *base.lock().unwrap() = server.url();

        let dir = tempfile::tempdir().unwrap();
        let asset = dir.path().join("game-v1.0.0-web.tar.gz");
        std::fs::write(&asset, vec![7u8; 1234]).unwrap();
        let client = GithubClient::new(Some("test-token".into())).with_base_url(server.url());
        let release = new_release(&args("v1.0.0"), "game");
        let published = space3d_tasks::runtime()
            .block_on(publish(
                &client,
                &RepoRef::new("acme", "game"),
                &release,
                std::slice::from_ref(&asset),
            ))
            .unwrap();
        assert_eq!(published.id, 7);
        assert_eq!(created.lock().unwrap().as_ref().unwrap()["tag_name"], "v1.0.0");
        assert_eq!(
            *uploads.lock().unwrap(),
            vec![("game-v1.0.0-web.tar.gz".to_owned(), 1234)]
        );
    }
}
