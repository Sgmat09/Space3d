//! Space3d GitHub integration.
//!
//! * [`GithubClient`] — thin async REST client (`Accept: application/vnd.github+json`,
//!   `X-GitHub-Api-Version: 2022-11-28`): repos, releases + asset uploads,
//!   issues, workflow dispatch / runs. `async fn`s for the CLI, [`Task`]s for
//!   engine systems ([`GithubClient::task`]). The API root is injectable for
//!   tests and proxies. Tokens come from `GITHUB_TOKEN` / `GH_TOKEN` or
//!   `gh auth token` ([`discover_token`], native only).
//! * [`UpdateChecker`] — compares the game's semver with the newest release.
//! * [`CrashReporter`] — panic hook storing crash reports locally; files them as
//!   issues on the next launch once the player opts in.
//! * [`templates`] — GitHub Actions workflow generator for game projects
//!   (tests + screenshot diffs, desktop matrix, web + Pages, Android, iOS,
//!   release on tag).
//! * [`GithubPlugin`] — wires the update check and crash reporter into an [`App`].
//!
//! [`Task`]: space3d_tasks::Task

#![warn(missing_docs)]

mod client;
mod crash;
mod error;
mod models;
pub mod templates;
mod update;

pub use client::{API_VERSION, DEFAULT_API_URL, GithubClient, discover_token, token_from_env};
pub use crash::{CrashReport, CrashReporter, CrashReporterConfig, PlatformInfo};
pub use error::{GithubError, Result};
pub use models::{
    Account, CreateRepo, Issue, NewIssue, NewRelease, Release, ReleaseAsset, RepoRef, Repository, WorkflowRun,
    WorkflowRunsQuery, content_type_for,
};
pub use semver;
pub use templates::{Target, TemplateOptions, github_actions, github_actions_with};
pub use update::{AvailableUpdate, UpdateChecker, UpdateStatus, evaluate_releases, parse_tag_version};

use space3d_core::{App, Plugin};
use space3d_tasks::{AsyncQueue, TasksPlugin};

/// Adds GitHub services to an [`App`]:
///
/// * inserts a [`GithubClient`] resource (token from [`GithubPlugin::token`] or
///   `GITHUB_TOKEN`/`GH_TOKEN`; the `gh` CLI is never spawned in-game),
/// * optionally checks for updates at startup → [`UpdateCheckState`] resource,
/// * optionally installs a [`CrashReporter`] (resource) and publishes crash
///   reports left from earlier runs as [`PendingCrashReports`], or submits them
///   straight away if the player already opted in.
#[derive(Clone, Debug)]
pub struct GithubPlugin {
    /// Repository with the game's releases / issues.
    pub repo: RepoRef,
    /// Version of the running game (usually `env!("CARGO_PKG_VERSION")`).
    pub current_version: String,
    /// Check for a newer release at startup.
    pub check_for_updates: bool,
    /// Offer prereleases as updates.
    pub include_prereleases: bool,
    /// Install the crash reporter with this configuration.
    pub crash_reports: Option<CrashReporterConfig>,
    /// The player opted in: submit pending crash reports at startup.
    pub submit_crash_reports: bool,
    /// Override the API root (proxy / tests).
    pub api_base_url: Option<String>,
    /// Explicit token. `None` (the default) makes read-only calls anonymously, which is
    /// what a shipped game wants: public repos need no token for release checks, and a
    /// player's own `GITHUB_TOKEN` must never be used to file crash issues. Development
    /// tools can opt in with [`GithubPlugin::with_token_from_env`].
    pub token: Option<String>,
}

impl GithubPlugin {
    /// Update check on, crash reporting off.
    pub fn new(repo: RepoRef, current_version: impl Into<String>) -> Self {
        Self {
            repo,
            current_version: current_version.into(),
            check_for_updates: true,
            include_prereleases: false,
            crash_reports: None,
            submit_crash_reports: false,
            api_base_url: None,
            token: None,
        }
    }

    /// Enable crash reporting, storing reports in `storage_dir`.
    pub fn with_crash_reports(
        mut self,
        app_name: impl Into<String>,
        storage_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        self.crash_reports = Some(
            CrashReporterConfig::new(app_name, self.current_version.clone(), self.repo.clone())
                .with_storage_dir(storage_dir),
        );
        self
    }

    /// Use a token from `GITHUB_TOKEN` / `GH_TOKEN` (development builds and tools only;
    /// never in a game shipped to players).
    pub fn with_token_from_env(mut self) -> Self {
        self.token = token_from_env();
        self
    }

    /// Route API calls through `url`.
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = Some(url.into());
        self
    }
}

/// Startup update check state (resource inserted by [`GithubPlugin`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateCheckState {
    /// Request in flight.
    Checking,
    /// Finished.
    Done(UpdateStatus),
    /// Network / API / version error.
    Failed(String),
}

/// Crash reports from earlier runs awaiting the player's consent
/// (resource inserted by [`GithubPlugin`]). Submit with
/// [`CrashReporter::submit_pending`] or drop with [`CrashReporter::discard_all`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PendingCrashReports(pub Vec<CrashReport>);

impl Plugin for GithubPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugin(TasksPlugin);
        let mut client = GithubClient::new(self.token.clone());
        if let Some(url) = &self.api_base_url {
            client = client.with_base_url(url);
        }
        app.insert_resource(client.clone());

        if self.check_for_updates {
            match UpdateChecker::new(client.clone(), self.repo.clone(), &self.current_version) {
                Ok(checker) => {
                    let task = checker.include_prereleases(self.include_prereleases).check();
                    app.insert_resource(UpdateCheckState::Checking);
                    app.resources.expect_mut::<AsyncQueue>().track(task, |ctx, result| {
                        let state = match result {
                            Ok(status) => {
                                if let Some(u) = status.update() {
                                    tracing::info!(current = %u.current, latest = %u.latest, "update available");
                                }
                                UpdateCheckState::Done(status)
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "update check failed");
                                UpdateCheckState::Failed(e.to_string())
                            }
                        };
                        ctx.resources.insert(state);
                    });
                }
                Err(e) => {
                    app.insert_resource(UpdateCheckState::Failed(e.to_string()));
                }
            }
        }

        if let Some(config) = &self.crash_reports {
            let reporter = CrashReporter::new(config.clone());
            reporter.install_panic_hook();
            app.insert_resource(PendingCrashReports::default());
            let queue = app.resources.expect_mut::<AsyncQueue>();
            let r = reporter.clone();
            if self.submit_crash_reports {
                queue.spawn_then(
                    async move {
                        let submitted = r.submit_pending_async(&client).await;
                        (submitted, r.pending().unwrap_or_default())
                    },
                    |ctx, (submitted, remaining): (Result<Vec<Issue>>, Vec<CrashReport>)| {
                        match submitted {
                            Ok(issues) if !issues.is_empty() => {
                                tracing::info!(count = issues.len(), "submitted crash reports")
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!(error = %e, "crash report submission failed"),
                        }
                        ctx.resources.insert(PendingCrashReports(remaining));
                    },
                );
            } else {
                queue.compute_then(
                    move || r.pending().unwrap_or_default(),
                    |ctx, pending| {
                        ctx.resources.insert(PendingCrashReports(pending));
                    },
                );
            }
            app.insert_resource(reporter);
        }
    }
}
