//! Request/response types for the subset of the GitHub REST API Space3d uses.
//! Response structs only model the fields we need; unknown fields are ignored.

use crate::error::{GithubError, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// `owner/name` repository identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoRef {
    /// User or organization login.
    pub owner: String,
    /// Repository name.
    pub name: String,
}

impl RepoRef {
    /// Build from parts.
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            name: name.into(),
        }
    }
}

impl FromStr for RepoRef {
    type Err = GithubError;

    /// Parses `owner/name`, also accepting `https://github.com/owner/name(.git)`.
    fn from_str(s: &str) -> Result<Self> {
        let trimmed = s
            .trim()
            .trim_start_matches("https://github.com/")
            .trim_start_matches("git@github.com:")
            .trim_end_matches('/')
            .trim_end_matches(".git");
        match trimmed.split('/').collect::<Vec<_>>()[..] {
            [owner, name] if !owner.is_empty() && !name.is_empty() => Ok(Self::new(owner, name)),
            _ => Err(GithubError::InvalidRepo(s.to_owned())),
        }
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// A GitHub user or organization (as embedded in other objects).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    /// Login name.
    pub login: String,
    /// Numeric id.
    pub id: u64,
    /// `"User"` or `"Organization"`.
    #[serde(rename = "type")]
    pub kind: String,
}

/// A repository.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Repository {
    /// Numeric id.
    pub id: u64,
    /// Short name.
    pub name: String,
    /// `owner/name`.
    pub full_name: String,
    /// Owner account.
    pub owner: Account,
    /// Private repository?
    pub private: bool,
    /// Description, if set.
    pub description: Option<String>,
    /// Web URL.
    pub html_url: String,
    /// HTTPS clone URL.
    pub clone_url: String,
    /// SSH clone URL.
    pub ssh_url: String,
    /// Default branch name.
    pub default_branch: String,
}

/// Parameters for [`crate::GithubClient::create_repo`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CreateRepo {
    /// Repository name.
    pub name: String,
    /// Create under this organization instead of the authenticated user.
    #[serde(skip)]
    pub org: Option<String>,
    /// Make the repository private.
    pub private: bool,
    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional homepage URL (e.g. the GitHub Pages build of the game).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// Create an initial commit with a README.
    pub auto_init: bool,
}

impl CreateRepo {
    /// A repository owned by the authenticated user.
    pub fn user(name: impl Into<String>, private: bool) -> Self {
        Self {
            name: name.into(),
            private,
            ..Default::default()
        }
    }

    /// A repository owned by organization `org`.
    pub fn org(org: impl Into<String>, name: impl Into<String>, private: bool) -> Self {
        Self {
            org: Some(org.into()),
            ..Self::user(name, private)
        }
    }

    /// Set the description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// A file attached to a release.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReleaseAsset {
    /// Numeric id.
    pub id: u64,
    /// File name.
    pub name: String,
    /// Optional label.
    pub label: Option<String>,
    /// MIME type.
    pub content_type: String,
    /// Size in bytes.
    pub size: u64,
    /// Download count.
    pub download_count: u64,
    /// Public download URL.
    pub browser_download_url: String,
    /// `"uploaded"` once complete.
    pub state: String,
}

/// A release.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Release {
    /// Numeric id.
    pub id: u64,
    /// Git tag, e.g. `v1.2.0`.
    pub tag_name: String,
    /// Commitish the tag was created from.
    pub target_commitish: String,
    /// Title.
    pub name: Option<String>,
    /// Markdown release notes.
    pub body: Option<String>,
    /// Draft release (not public)?
    pub draft: bool,
    /// Marked as pre-release?
    pub prerelease: bool,
    /// Web URL.
    pub html_url: String,
    /// Hypermedia upload URL template (`.../assets{?name,label}`).
    pub upload_url: String,
    /// ISO-8601 creation time.
    pub created_at: Option<String>,
    /// ISO-8601 publication time.
    pub published_at: Option<String>,
    /// Attached files.
    pub assets: Vec<ReleaseAsset>,
}

impl Release {
    /// Version parsed from [`Release::tag_name`] (see [`crate::parse_tag_version`]).
    pub fn version(&self) -> Option<semver::Version> {
        crate::update::parse_tag_version(&self.tag_name)
    }

    /// First asset whose lower-cased name contains any of `needles`.
    pub fn find_asset(&self, needles: &[&str]) -> Option<&ReleaseAsset> {
        self.assets.iter().find(|a| {
            let n = a.name.to_ascii_lowercase();
            needles.iter().any(|k| n.contains(k))
        })
    }

    /// Best-guess build for the platform this binary is running on, based on
    /// asset names (`*-linux*`, `*-windows*`, `*-macos*`, `*-web*`, `*.apk`, `*.ipa`).
    pub fn asset_for_current_platform(&self) -> Option<&ReleaseAsset> {
        let needles: &[&str] = if cfg!(target_arch = "wasm32") {
            &["web", "wasm"]
        } else {
            match std::env::consts::OS {
                "macos" => &["macos", "darwin", "osx", "mac"],
                "windows" => &["windows", "win64", "win"],
                "linux" => &["linux"],
                "android" => &[".apk", "android"],
                "ios" => &[".ipa", "ios"],
                _ => &[],
            }
        };
        self.find_asset(needles)
    }
}

/// Parameters for [`crate::GithubClient::create_release`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NewRelease {
    /// Tag to create/use, e.g. `v1.2.0`.
    pub tag_name: String,
    /// Branch or SHA for a new tag (defaults to the default branch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_commitish: Option<String>,
    /// Title (defaults to the tag).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Markdown notes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Create as draft.
    pub draft: bool,
    /// Mark as pre-release.
    pub prerelease: bool,
    /// Let GitHub generate notes from merged PRs.
    pub generate_release_notes: bool,
}

impl NewRelease {
    /// Release for `tag` with generated notes.
    pub fn new(tag: impl Into<String>) -> Self {
        Self {
            tag_name: tag.into(),
            generate_release_notes: true,
            ..Default::default()
        }
    }
}

/// An issue.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Issue {
    /// Numeric id.
    pub id: u64,
    /// Issue number within the repository.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Markdown body.
    pub body: Option<String>,
    /// `"open"` / `"closed"`.
    pub state: String,
    /// Web URL.
    pub html_url: String,
}

/// Parameters for [`crate::GithubClient::create_issue`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NewIssue {
    /// Title.
    pub title: String,
    /// Markdown body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Labels (must exist or the token must be allowed to create them).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Logins to assign.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<String>,
}

impl NewIssue {
    /// Issue with a title and body.
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: Some(body.into()),
            ..Default::default()
        }
    }

    /// Add a label.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.labels.push(label.into());
        self
    }
}

/// A GitHub Actions workflow run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkflowRun {
    /// Numeric id.
    pub id: u64,
    /// Workflow name.
    pub name: Option<String>,
    /// Sequential run number.
    pub run_number: u64,
    /// Triggering event (`push`, `workflow_dispatch`, ...).
    pub event: String,
    /// `queued`, `in_progress`, `completed`, ...
    pub status: Option<String>,
    /// `success`, `failure`, `cancelled`, ... once completed.
    pub conclusion: Option<String>,
    /// Branch.
    pub head_branch: Option<String>,
    /// Commit SHA.
    pub head_sha: String,
    /// Workflow id.
    pub workflow_id: u64,
    /// Web URL.
    pub html_url: String,
    /// ISO-8601 creation time.
    pub created_at: Option<String>,
    /// ISO-8601 update time.
    pub updated_at: Option<String>,
}

impl WorkflowRun {
    /// `true` once the run finished (any conclusion).
    pub fn is_completed(&self) -> bool {
        self.status.as_deref() == Some("completed")
    }

    /// `true` if completed successfully.
    pub fn is_success(&self) -> bool {
        self.conclusion.as_deref() == Some("success")
    }
}

/// Filter for [`crate::GithubClient::list_workflow_runs`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkflowRunsQuery {
    /// Only runs of this workflow (file name like `space3d.yml` or numeric id).
    pub workflow: Option<String>,
    /// Only runs on this branch.
    pub branch: Option<String>,
    /// Only runs with this status / conclusion (e.g. `completed`, `success`).
    pub status: Option<String>,
    /// Only runs triggered by this event.
    pub event: Option<String>,
    /// Page size (GitHub max 100; default 30).
    pub per_page: Option<u8>,
}

impl WorkflowRunsQuery {
    /// Runs of one workflow.
    pub fn workflow(workflow: impl Into<String>) -> Self {
        Self {
            workflow: Some(workflow.into()),
            ..Default::default()
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct WorkflowRunsPage {
    #[serde(default)]
    pub workflow_runs: Vec<WorkflowRun>,
}

/// MIME type for a release asset, from its file name.
///
/// ```
/// assert_eq!(space3d_github::content_type_for("game-web.wasm"), "application/wasm");
/// assert_eq!(space3d_github::content_type_for("Game.APK"), "application/vnd.android.package-archive");
/// ```
pub fn content_type_for(file_name: &str) -> &'static str {
    let lower = file_name.to_ascii_lowercase();
    let ext = |e: &str| lower.ends_with(e);
    if ext(".zip") {
        "application/zip"
    } else if ext(".wasm") {
        "application/wasm"
    } else if ext(".apk") {
        "application/vnd.android.package-archive"
    } else if ext(".tar.gz") || ext(".tgz") {
        "application/gzip"
    } else if ext(".dmg") {
        "application/x-apple-diskimage"
    } else if ext(".exe") {
        "application/vnd.microsoft.portable-executable"
    } else if ext(".msi") {
        "application/x-msi"
    } else if ext(".json") {
        "application/json"
    } else if ext(".txt") || ext(".md") {
        "text/plain"
    } else {
        // .ipa, .aab, .appimage, raw binaries, ...
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_ref_parsing() {
        let r: RepoRef = "SpaceCorps/Space3d".parse().unwrap();
        assert_eq!(r, RepoRef::new("SpaceCorps", "Space3d"));
        assert_eq!(r.to_string(), "SpaceCorps/Space3d");
        assert_eq!(
            "https://github.com/a/b.git".parse::<RepoRef>().unwrap(),
            RepoRef::new("a", "b")
        );
        assert_eq!(
            "git@github.com:a/b.git".parse::<RepoRef>().unwrap(),
            RepoRef::new("a", "b")
        );
        assert!("nope".parse::<RepoRef>().is_err());
        assert!("a/b/c".parse::<RepoRef>().is_err());
        assert!("/b".parse::<RepoRef>().is_err());
    }

    #[test]
    fn platform_assets() {
        let release = Release {
            assets: [
                "game-linux.zip",
                "game-windows.zip",
                "game-macos.zip",
                "game-web.zip",
                "game.apk",
            ]
            .iter()
            .map(|n| ReleaseAsset {
                name: n.to_string(),
                ..Default::default()
            })
            .collect(),
            ..Default::default()
        };
        assert_eq!(release.find_asset(&[".apk"]).unwrap().name, "game.apk");
        #[cfg(target_os = "macos")]
        assert_eq!(release.asset_for_current_platform().unwrap().name, "game-macos.zip");
        #[cfg(target_os = "linux")]
        assert_eq!(release.asset_for_current_platform().unwrap().name, "game-linux.zip");
        #[cfg(target_os = "windows")]
        assert_eq!(release.asset_for_current_platform().unwrap().name, "game-windows.zip");
    }
}
