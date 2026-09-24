//! In-game update checker: compares the running game's semver against the
//! repository's releases.

use crate::client::GithubClient;
use crate::error::Result;
use crate::models::{Release, ReleaseAsset, RepoRef};
use semver::Version;
use space3d_tasks::Task;

/// Parse a version out of a release tag: `v1.2.3`, `1.2.3`, `game-v1.2.3`,
/// `v1.2` (→ `1.2.0`), `v2` (→ `2.0.0`), `v1.3.0-beta.1`.
///
/// ```
/// use space3d_github::parse_tag_version;
/// assert_eq!(parse_tag_version("v1.2").unwrap().to_string(), "1.2.0");
/// assert_eq!(parse_tag_version("mygame-2-v0.4.1").unwrap().to_string(), "0.4.1");
/// assert!(parse_tag_version("nightly").is_none());
/// ```
pub fn parse_tag_version(tag: &str) -> Option<Version> {
    let tag = tag.trim();
    // Candidate starts: a digit that does not continue a number.
    let starts: Vec<usize> = tag
        .char_indices()
        .filter(|&(i, c)| {
            c.is_ascii_digit()
                && tag[..i]
                    .chars()
                    .next_back()
                    .is_none_or(|p| !p.is_ascii_digit() && p != '.')
        })
        .map(|(i, _)| i)
        .collect();
    starts
        .iter()
        .find_map(|&i| Version::parse(&tag[i..]).ok())
        .or_else(|| starts.iter().find_map(|&i| parse_padded(&tag[i..])))
}

fn parse_padded(s: &str) -> Option<Version> {
    let split = s.find(['-', '+']).unwrap_or(s.len());
    let (core, rest) = s.split_at(split);
    let padded = match core.split('.').count() {
        1 => format!("{core}.0.0{rest}"),
        2 => format!("{core}.0{rest}"),
        _ => return None,
    };
    Version::parse(&padded).ok()
}

/// A newer release than the running game.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableUpdate {
    /// Version of the running game.
    pub current: Version,
    /// Newest release version.
    pub latest: Version,
    /// The release itself (notes, assets, URL).
    pub release: Release,
}

impl AvailableUpdate {
    /// Download for the current platform, if the release has one.
    pub fn asset(&self) -> Option<&ReleaseAsset> {
        self.release.asset_for_current_platform()
    }

    /// Release page URL to show to players.
    pub fn url(&self) -> &str {
        &self.release.html_url
    }
}

/// Result of an update check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    /// No newer release. `latest` is the newest release found (if any).
    UpToDate {
        /// Version of the running game.
        current: Version,
        /// Newest published version, if any release exists.
        latest: Option<Version>,
    },
    /// A newer release exists.
    Available(Box<AvailableUpdate>),
}

impl UpdateStatus {
    /// The available update, if any.
    pub fn update(&self) -> Option<&AvailableUpdate> {
        match self {
            UpdateStatus::Available(u) => Some(&**u),
            UpdateStatus::UpToDate { .. } => None,
        }
    }

    /// `true` if a newer release exists.
    pub fn is_update_available(&self) -> bool {
        self.update().is_some()
    }
}

/// Decide whether `releases` contain something newer than `current`.
/// Drafts are ignored; prereleases (flagged on GitHub *or* with a semver
/// pre-release suffix) only count when `include_prereleases` is set.
pub fn evaluate_releases(current: &Version, releases: &[Release], include_prereleases: bool) -> UpdateStatus {
    let newest = releases
        .iter()
        .filter(|r| !r.draft)
        .filter_map(|r| r.version().map(|v| (v, r)))
        .filter(|(v, r)| include_prereleases || (!r.prerelease && v.pre.is_empty()))
        .max_by(|(a, _), (b, _)| a.cmp(b));
    match newest {
        Some((latest, release)) if latest > *current => UpdateStatus::Available(Box::new(AvailableUpdate {
            current: current.clone(),
            latest,
            release: release.clone(),
        })),
        other => UpdateStatus::UpToDate {
            current: current.clone(),
            latest: other.map(|(v, _)| v),
        },
    }
}

/// Checks a repository's releases for a newer version of the running game.
///
/// ```no_run
/// # use space3d_github::{GithubClient, RepoRef, UpdateChecker};
/// let checker = UpdateChecker::new(
///     GithubClient::new(None),
///     RepoRef::new("SpaceCorps", "my-game"),
///     env!("CARGO_PKG_VERSION"),
/// ).unwrap();
/// let mut task = checker.check(); // poll from a system
/// ```
#[derive(Clone, Debug)]
pub struct UpdateChecker {
    client: GithubClient,
    repo: RepoRef,
    current: Version,
    include_prereleases: bool,
}

impl UpdateChecker {
    /// `current_version` is usually `env!("CARGO_PKG_VERSION")`.
    pub fn new(client: GithubClient, repo: RepoRef, current_version: &str) -> Result<Self> {
        Ok(Self {
            client,
            repo,
            current: parse_tag_version(current_version).map_or_else(|| Version::parse(current_version), Ok)?,
            include_prereleases: false,
        })
    }

    /// Also offer prereleases (beta channel).
    pub fn include_prereleases(mut self, include: bool) -> Self {
        self.include_prereleases = include;
        self
    }

    /// Version of the running game.
    pub fn current(&self) -> &Version {
        &self.current
    }

    /// Repository being checked.
    pub fn repo(&self) -> &RepoRef {
        &self.repo
    }

    /// Fetch releases and compare (async; for tools and async code).
    pub async fn check_async(&self) -> Result<UpdateStatus> {
        let releases = self.client.list_releases(&self.repo).await?;
        Ok(evaluate_releases(&self.current, &releases, self.include_prereleases))
    }

    /// Fetch releases and compare on the background runtime; poll the task from a system.
    pub fn check(&self) -> Task<Result<UpdateStatus>> {
        let this = self.clone();
        space3d_tasks::spawn(async move { this.check_async().await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(tag: &str, prerelease: bool, draft: bool) -> Release {
        Release {
            tag_name: tag.into(),
            prerelease,
            draft,
            ..Default::default()
        }
    }

    #[test]
    fn tag_parsing() {
        let v = |s: &str| parse_tag_version(s).map(|v| v.to_string());
        assert_eq!(v("v1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(v("1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(v("V2").as_deref(), Some("2.0.0"));
        assert_eq!(v("release-v0.10.1-rc.2").as_deref(), Some("0.10.1-rc.2"));
        assert_eq!(v("v1.2-beta").as_deref(), Some("1.2.0-beta"));
        assert_eq!(v("latest"), None);
    }

    #[test]
    fn evaluation_ignores_drafts_and_prereleases() {
        let releases = vec![
            rel("v1.0.0", false, false),
            rel("v1.2.0", false, false),
            rel("v1.1.5", false, false),
            rel("v1.3.0-beta.1", true, false),
            rel("v1.4.0", true, false), // flagged prerelease on GitHub
            rel("v2.0.0", false, true), // draft
            rel("nightly", false, false),
        ];
        let cur = Version::new(1, 1, 0);
        let s = evaluate_releases(&cur, &releases, false);
        assert_eq!(s.update().unwrap().latest, Version::new(1, 2, 0));
        let s = evaluate_releases(&cur, &releases, true);
        assert_eq!(s.update().unwrap().latest, Version::new(1, 4, 0));
        let s = evaluate_releases(&Version::new(1, 2, 0), &releases, false);
        assert_eq!(
            s,
            UpdateStatus::UpToDate {
                current: Version::new(1, 2, 0),
                latest: Some(Version::new(1, 2, 0))
            }
        );
        assert!(!evaluate_releases(&cur, &[], false).is_update_available());
    }
}
