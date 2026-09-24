/// Result alias used across `space3d-github`.
pub type Result<T, E = GithubError> = std::result::Result<T, E>;

/// Errors from the GitHub client, update checker and crash reporter.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GithubError {
    /// Transport-level failure (DNS, TLS, connection, body decoding).
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    /// GitHub answered with a non-success status.
    #[error("GitHub API error {status}: {message}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// `message` from GitHub's error body (or the raw body).
        message: String,
        /// `documentation_url` from GitHub's error body, if any.
        documentation_url: Option<String>,
    },
    /// JSON (de)serialization failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// No token could be found for an operation that needs one.
    #[error("no GitHub token: set GITHUB_TOKEN (or GH_TOKEN) or run `gh auth login`")]
    NoToken,
    /// A repository string was not `owner/name`.
    #[error("invalid repository `{0}` (expected `owner/name`)")]
    InvalidRepo(String),
    /// A version string was not valid semver.
    #[error("invalid version: {0}")]
    Version(#[from] semver::Error),
    /// Local file I/O failed (asset upload, crash report storage).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Not available on this platform (e.g. `gh` token discovery on web).
    #[error("not supported on this platform: {0}")]
    Unsupported(&'static str),
    /// Local storage failure (web crash-report store).
    #[error("storage error: {0}")]
    Storage(String),
}

impl GithubError {
    /// HTTP status for [`GithubError::Api`] (or transport errors carrying one).
    pub fn status(&self) -> Option<u16> {
        match self {
            GithubError::Api { status, .. } => Some(*status),
            GithubError::Http(e) => e.status().map(|s| s.as_u16()),
            _ => None,
        }
    }

    /// `true` for HTTP 404.
    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }
}
