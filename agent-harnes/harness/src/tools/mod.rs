//! In-process tools for the agent harness.
//!
//! All tools are surfaced as [`crate::agent_loop::ToolEntry`] values.
//! Every tool that touches the filesystem is sandboxed to a [`WorkspaceRoot`].

pub mod apply_patch;
pub mod bash;
pub mod glob;
pub mod grep;
pub mod read_file;
pub mod truncate;
pub mod write_file;

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::agent_loop::ToolEntry;

// ── WorkspaceRoot ─────────────────────────────────────────────────────────────

/// A canonicalized workspace root that all tool paths are restricted to.
#[derive(Debug, Clone)]
pub struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    /// Create a new `WorkspaceRoot` by canonicalizing `root`.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let canonical = std::fs::canonicalize(root.as_ref())?;
        Ok(Self(canonical))
    }

    /// Return the canonicalized root path.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Resolve `rel` against this root and confirm the result is still inside.
    ///
    /// Accepts relative paths like `"src/main.rs"` and absolute paths that
    /// start with the workspace root.  Rejects anything that would escape
    /// (e.g. `"../secret"` or `"/etc/passwd"`).
    pub fn resolve_safe(&self, rel: &str) -> Result<PathBuf> {
        // Strip a leading workspace-root prefix so callers can pass either
        // relative or already-absolute paths that happen to be inside root.
        let candidate = if Path::new(rel).is_absolute() {
            PathBuf::from(rel)
        } else {
            self.0.join(rel)
        };

        // Normalise without requiring the path to exist yet (for writes).
        let normalised = normalise_path(&candidate);

        // The normalised path must start with the workspace root.
        if !normalised.starts_with(&self.0) {
            bail!(
                "path `{}` escapes the workspace root `{}`",
                rel,
                self.0.display()
            );
        }

        Ok(normalised)
    }
}

/// Normalise a path by resolving `.` and `..` components lexically,
/// without requiring the path to exist on disk.
fn normalise_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

// ── all_tools ─────────────────────────────────────────────────────────────────

/// Build all six in-process tools pre-configured for `workspace_root`.
///
/// Pass the returned `Vec<ToolEntry>` into [`crate::agent_loop::LoopConfig::tools`].
pub fn all_tools(workspace_root: PathBuf) -> Vec<ToolEntry> {
    let root = match WorkspaceRoot::new(&workspace_root) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%e, "could not canonicalize workspace root — tools disabled");
            return Vec::new();
        }
    };

    vec![
        read_file::entry(root.clone()),
        write_file::entry(root.clone()),
        apply_patch::entry(root.clone()),
        bash::entry(root.clone()),
        grep::entry(root.clone()),
        glob::entry(root),
    ]
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_root() -> (TempDir, WorkspaceRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = WorkspaceRoot::new(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn resolve_safe_allows_relative() {
        let (_dir, root) = temp_root();
        let resolved = root.resolve_safe("src/main.rs").unwrap();
        assert!(resolved.starts_with(root.path()));
        assert!(resolved.ends_with("src/main.rs"));
    }

    #[test]
    fn resolve_safe_allows_nested() {
        let (_dir, root) = temp_root();
        let resolved = root.resolve_safe("a/b/c/d.txt").unwrap();
        assert!(resolved.starts_with(root.path()));
    }

    #[test]
    fn resolve_safe_rejects_dotdot_escape() {
        let (_dir, root) = temp_root();
        let err = root.resolve_safe("../etc/passwd").unwrap_err();
        assert!(err.to_string().contains("escapes"));
    }

    #[test]
    fn resolve_safe_rejects_absolute_escape() {
        let (_dir, root) = temp_root();
        let err = root.resolve_safe("/etc/passwd").unwrap_err();
        assert!(err.to_string().contains("escapes"));
    }

    #[test]
    fn resolve_safe_allows_absolute_inside_root() {
        let (_dir, root) = temp_root();
        // Build an absolute path that is inside the root.
        let inside = root.path().join("foo/bar.rs");
        let inside_str = inside.to_str().unwrap();
        let resolved = root.resolve_safe(inside_str).unwrap();
        assert!(resolved.starts_with(root.path()));
    }

    #[test]
    fn resolve_safe_dot_component_normalised() {
        let (_dir, root) = temp_root();
        let resolved = root.resolve_safe("./src/./lib.rs").unwrap();
        assert!(resolved.ends_with("src/lib.rs"));
    }
}
