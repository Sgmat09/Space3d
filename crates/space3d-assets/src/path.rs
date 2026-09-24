//! Normalized asset paths.

use std::fmt;

/// A normalized, optionally source-qualified asset path.
///
/// * `textures/wall.png` — resolved against the default source (the asset root).
/// * `memory://textures/wall.png` — resolved against the source registered as `memory`.
///
/// Backslashes become `/`, `.` segments are dropped and `..` segments are collapsed
/// where possible. A path that still starts with `..` escapes its root and is rejected
/// by [`crate::FileSource`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AssetPath {
    source: Option<String>,
    path: String,
}

impl AssetPath {
    /// Parse `path`, splitting off an optional `source://` prefix.
    pub fn new(path: &str) -> Self {
        match path.split_once("://") {
            Some((source, rest)) if !source.is_empty() && !source.contains('/') => Self {
                source: Some(source.to_string()),
                path: normalize(rest),
            },
            _ => Self {
                source: None,
                path: normalize(path),
            },
        }
    }

    /// Build a path that explicitly targets the named source.
    pub fn with_source(source: impl Into<String>, path: &str) -> Self {
        Self {
            source: Some(source.into()),
            path: normalize(path),
        }
    }

    /// Name of the source (`None` = default source).
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// The normalized path within its source, using `/` separators.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Final path segment.
    pub fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or("")
    }

    /// Lower-cased extension of the file name (text after the last `.`), if any.
    pub fn extension(&self) -> Option<String> {
        let name = self.file_name();
        let (stem, ext) = name.rsplit_once('.')?;
        (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_ascii_lowercase())
    }

    /// Whether the file name ends with `.ext` (case-insensitive; `ext` may be compound like `tar.gz`).
    pub fn has_extension(&self, ext: &str) -> bool {
        let name = self.file_name().to_ascii_lowercase();
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        name.len() > ext.len() + 1 && name.ends_with(&ext) && name.as_bytes()[name.len() - ext.len() - 1] == b'.'
    }

    /// The directory containing this path (same source).
    pub fn parent(&self) -> AssetPath {
        let dir = self.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        Self {
            source: self.source.clone(),
            path: dir.to_string(),
        }
    }

    /// Resolve `relative` against this path's directory (e.g. a glTF buffer URI).
    ///
    /// A leading `/` makes it relative to the source root; a `name://` prefix switches source.
    pub fn resolve(&self, relative: &str) -> AssetPath {
        if relative.contains("://") {
            return AssetPath::new(relative);
        }
        if let Some(abs) = relative.strip_prefix('/') {
            return Self {
                source: self.source.clone(),
                path: normalize(abs),
            };
        }
        let dir = self.parent();
        let joined = if dir.path.is_empty() {
            relative.to_string()
        } else {
            format!("{}/{}", dir.path, relative)
        };
        Self {
            source: self.source.clone(),
            path: normalize(&joined),
        }
    }

    /// True if the path climbs above its source root (starts with `..`).
    pub fn escapes_root(&self) -> bool {
        self.path == ".." || self.path.starts_with("../")
    }
}

fn normalize(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                if matches!(out.last(), Some(last) if *last != "..") {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

impl fmt::Display for AssetPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Some(s) => write!(f, "{s}://{}", self.path),
            None => f.write_str(&self.path),
        }
    }
}

impl From<&str> for AssetPath {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}
impl From<String> for AssetPath {
    fn from(s: String) -> Self {
        Self::new(&s)
    }
}
impl From<&String> for AssetPath {
    fn from(s: &String) -> Self {
        Self::new(s)
    }
}
impl From<&AssetPath> for AssetPath {
    fn from(p: &AssetPath) -> Self {
        p.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalizes() {
        let p = AssetPath::new("./textures\\wall.PNG");
        assert_eq!(p.path(), "textures/wall.PNG");
        assert_eq!(p.source(), None);
        assert_eq!(p.extension().as_deref(), Some("png"));
        assert_eq!(p.file_name(), "wall.PNG");
        let m = AssetPath::new("memory://a/b/../c.gltf");
        assert_eq!(m.source(), Some("memory"));
        assert_eq!(m.path(), "a/c.gltf");
        assert_eq!(m.to_string(), "memory://a/c.gltf");
        assert!(AssetPath::new("x.tar.gz").has_extension("tar.gz"));
        assert!(!AssetPath::new("gz").has_extension("gz"));
        assert_eq!(AssetPath::new(".hidden").extension(), None);
    }

    #[test]
    fn resolves_relative() {
        let p = AssetPath::new("models/ship/ship.gltf");
        assert_eq!(p.resolve("ship.bin").path(), "models/ship/ship.bin");
        assert_eq!(p.resolve("../tex/a.png").path(), "models/tex/a.png");
        assert_eq!(p.resolve("/root.png").path(), "root.png");
        assert!(AssetPath::new("a.gltf").resolve("../../x").escapes_root());
        let m = AssetPath::new("memory://m.obj");
        assert_eq!(m.resolve("m.mtl").to_string(), "memory://m.mtl");
    }
}
