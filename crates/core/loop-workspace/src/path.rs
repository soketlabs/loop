//! Workspace-relative paths.
//!
//! A [`WorkspacePath`] names a file inside a Workspace, relative to its root.
//! It never names a host file. It is the path type for protocol methods
//! (file list, upload, download) and for chunked transfer, because a remote
//! Workspace has no host path. The driver maps it to a path in its own
//! namespace with [`WorkspacePath::to_native`].

use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::WorkspaceError;

/// A normalised path inside a Workspace, relative to the Workspace root.
///
/// Rules: `/`-separated; no leading `/`; no `.` or empty segments; no `..`
/// segment; no NUL byte. The empty path is the root. Lexical only: a
/// symbolic link inside the Workspace can still point outside it, so the
/// driver must check the opened file (see the design note).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkspacePath(String);

impl WorkspacePath {
    /// The Workspace root.
    pub fn root() -> Self {
        Self(String::new())
    }

    /// Parse and normalise `raw`. `.` segments and repeated `/` are removed;
    /// a `..` that stays inside the root is folded; an absolute path, a `..`
    /// above the root and a NUL byte are errors.
    pub fn parse(raw: &str) -> Result<Self, WorkspaceError> {
        let invalid = |reason| WorkspaceError::InvalidPath {
            path: raw.to_owned(),
            reason,
        };
        if raw.contains('\0') {
            return Err(invalid("contains a NUL byte"));
        }
        if raw.starts_with('/') {
            return Err(invalid(
                "is absolute; use a path relative to the workspace root",
            ));
        }
        let mut parts: Vec<&str> = Vec::new();
        for segment in raw.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    if parts.pop().is_none() {
                        return Err(invalid("leaves the workspace root"));
                    }
                }
                other => parts.push(other),
            }
        }
        Ok(Self(parts.join("/")))
    }

    /// The path of `native` relative to `root`, both in the driver's own
    /// namespace. Lexical: `.` and `..` are folded first, links are not
    /// followed. An error when `native` is not inside `root`.
    pub fn from_native(root: &Path, native: &Path) -> Result<Self, WorkspaceError> {
        let outside = || WorkspaceError::InvalidPath {
            path: native.display().to_string(),
            reason: "is not inside the workspace root",
        };
        let root = lexical(root);
        let native = lexical(&root.join(native));
        let rest = native.strip_prefix(&root).map_err(|_| outside())?;
        let text = rest.to_str().ok_or(WorkspaceError::InvalidPath {
            path: native.display().to_string(),
            reason: "is not valid UTF-8",
        })?;
        Self::parse(text)
    }

    /// The path as a `/`-separated string; empty for the root.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the Workspace root.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// `self` joined with the relative path `child` (parsed by the same rules).
    pub fn join(&self, child: &str) -> Result<Self, WorkspaceError> {
        if self.is_root() {
            Self::parse(child)
        } else {
            Self::parse(&format!("{}/{}", self.0, child))
        }
    }

    /// The parent path; `None` for the root.
    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        Some(match self.0.rsplit_once('/') {
            Some((parent, _)) => Self(parent.to_owned()),
            None => Self::root(),
        })
    }

    /// The last segment; `None` for the root.
    pub fn file_name(&self) -> Option<&str> {
        if self.is_root() {
            return None;
        }
        self.0.rsplit('/').next()
    }

    /// The segments, root first.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|s| !s.is_empty())
    }

    /// This path under `root`, in the driver's own namespace (a host path
    /// for local drivers, a guest path for a remote one).
    pub fn to_native(&self, root: &Path) -> PathBuf {
        let mut out = root.to_path_buf();
        for segment in self.segments() {
            out.push(segment);
        }
        out
    }
}

fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

impl fmt::Display for WorkspacePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            f.write_str(".")
        } else {
            f.write_str(&self.0)
        }
    }
}

impl TryFrom<String> for WorkspacePath {
    type Error = WorkspaceError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<WorkspacePath> for String {
    fn from(value: WorkspacePath) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_normalises() {
        assert_eq!(WorkspacePath::parse("a//./b/").unwrap().as_str(), "a/b");
        assert_eq!(WorkspacePath::parse("a/../b").unwrap().as_str(), "b");
        assert!(WorkspacePath::parse("").unwrap().is_root());
        assert!(WorkspacePath::parse(".").unwrap().is_root());
    }

    #[test]
    fn parse_rejects_escapes() {
        for bad in ["/etc/passwd", "..", "a/../../b", "a\0b"] {
            assert!(
                matches!(
                    WorkspacePath::parse(bad),
                    Err(WorkspaceError::InvalidPath { .. })
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn native_round_trip() {
        let root = Path::new("/w/project");
        let p = WorkspacePath::parse("src/main.rs").unwrap();
        assert_eq!(p.to_native(root), Path::new("/w/project/src/main.rs"));
        assert_eq!(
            WorkspacePath::from_native(root, Path::new("/w/project/src/../src/main.rs")).unwrap(),
            p
        );
        assert!(WorkspacePath::from_native(root, Path::new("/w/other")).is_err());
        assert!(WorkspacePath::from_native(root, Path::new("../x")).is_err());
        assert_eq!(
            WorkspacePath::from_native(root, Path::new("src"))
                .unwrap()
                .as_str(),
            "src"
        );
    }

    #[test]
    fn join_parent_and_name() {
        let p = WorkspacePath::root().join("a/b").unwrap();
        assert_eq!(p.file_name(), Some("b"));
        assert_eq!(p.parent().unwrap().as_str(), "a");
        assert!(p.parent().unwrap().parent().unwrap().is_root());
        assert!(p.join("../../..").is_err());
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"a/b\"");
        assert!(serde_json::from_str::<WorkspacePath>("\"/abs\"").is_err());
    }
}
