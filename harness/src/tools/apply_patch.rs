//! `apply_patch` tool — apply an OpenAI-format patch via `hanzo-apply-patch`.
//!
//! The model sends the full `apply_patch` body (the text from
//! `*** Begin Patch` through `*** End Patch`).  We parse it with the
//! `hanzo-apply-patch` crate, resolve every file path against the workspace
//! root, apply the changes to disk, and return a summary that includes a
//! `similar`-generated unified diff for each affected file.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use hanzo_apply_patch::{Hunk, apply_hunks, parse_patch};
use rig_core::completion::ToolDefinition;
use serde::Deserialize;

use crate::agent_loop::ToolEntry;
use crate::tools::{WorkspaceRoot, write_file::unified_diff};

#[derive(Debug, Deserialize)]
struct ApplyPatchArgs {
    patch: String,
}

/// Resolve a path from the patch to an absolute, workspace-root-safe path.
fn resolve_path(root: &WorkspaceRoot, p: &Path) -> Result<std::path::PathBuf> {
    root.resolve_safe(&p.to_string_lossy())
}

/// Information needed to diff a file that the patch touches.
struct FileDiff {
    path: String,
    old: String,
    new: String,
}

/// Apply the patch and return a summary with diffs.
pub fn execute(root: &WorkspaceRoot, args: serde_json::Value) -> Result<String> {
    let args: ApplyPatchArgs = serde_json::from_value(args)?;

    let parsed = parse_patch(&args.patch)
        .map_err(|e| anyhow!("apply_patch: failed to parse patch: {e}"))?;

    if parsed.hunks.is_empty() {
        return Ok("No file changes in patch.".to_string());
    }

    // --- Resolve paths to absolute, workspace-root-safe paths ---
    let mut resolved: Vec<Hunk> = Vec::new();
    let mut affected_paths: Vec<String> = Vec::new();

    for hunk in &parsed.hunks {
        match hunk {
            Hunk::AddFile { path, contents } => {
                let abs = resolve_path(root, path)?;
                affected_paths.push(path.to_string_lossy().to_string());
                resolved.push(Hunk::AddFile {
                    path: abs,
                    contents: contents.clone(),
                });
            }
            Hunk::DeleteFile { path } => {
                let abs = resolve_path(root, path)?;
                affected_paths.push(path.to_string_lossy().to_string());
                resolved.push(Hunk::DeleteFile { path: abs });
            }
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                let abs = resolve_path(root, path)?;
                let abs_move = match move_path {
                    Some(mp) => Some(resolve_path(root, mp)?),
                    None => None,
                };
                affected_paths.push(path.to_string_lossy().to_string());
                resolved.push(Hunk::UpdateFile {
                    path: abs,
                    move_path: abs_move,
                    chunks: chunks.clone(),
                });
            }
        }
    }

    // --- Snapshot old content (read before applying) ---
    let mut snapshots: Vec<FileDiff> = Vec::new();
    for hunk in &resolved {
        let (abs_path, rel_label, old) = match hunk {
            Hunk::AddFile { path, .. } => {
                let old = std::fs::read_to_string(path).unwrap_or_default();
                (path.clone(), path.to_string_lossy().to_string(), old)
            }
            Hunk::DeleteFile { path } => {
                let old = std::fs::read_to_string(path)
                    .unwrap_or_else(|_| "[unreadable]".to_string());
                (path.clone(), path.to_string_lossy().to_string(), old)
            }
            Hunk::UpdateFile { path, .. } => {
                let old = std::fs::read_to_string(path).unwrap_or_default();
                (path.clone(), path.to_string_lossy().to_string(), old)
            }
        };
        let _ = abs_path;
        snapshots.push(FileDiff {
            path: rel_label,
            old,
            new: String::new(), // filled in after apply
        });
    }

    // --- Apply to disk ---
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    apply_hunks(&resolved, &mut stdout, &mut stderr)
        .context("apply_patch: failed to apply hunks")?;

    // --- Read new content (post-apply) ---
    for snap in &mut snapshots {
        let abs = root.resolve_safe(&snap.path)?;
        snap.new = std::fs::read_to_string(&abs).unwrap_or_default();
    }

    // --- Build output ---
    let summary = String::from_utf8_lossy(&stdout);
    let err = String::from_utf8_lossy(&stderr);
    let mut out = String::new();
    if !summary.trim().is_empty() {
        out.push_str(summary.trim());
        out.push('\n');
    }
    if !err.trim().is_empty() {
        out.push_str("stderr: ");
        out.push_str(err.trim());
        out.push('\n');
    }

    out.push_str("\n--- Diffs ---\n");
    for snap in &snapshots {
        let diff = unified_diff(&snap.old, &snap.new, &snap.path);
        out.push_str(&diff);
        out.push('\n');
    }

    Ok(out)
}

/// Build the `ToolEntry` for `apply_patch`.
pub fn entry(root: WorkspaceRoot) -> ToolEntry {
    ToolEntry {
        definition: ToolDefinition {
            name: "apply_patch".into(),
            description:
                "Apply a structured patch to one or more files using the OpenAI apply_patch \
                 format. The patch begins with *** Begin Patch and ends with *** End Patch. \
                 Supported operations: Add File, Delete File, Update File (with line-level \
                 replace), and Move File. All file paths are resolved relative to the \
                 workspace root. Requires approval."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "Patch text in OpenAI apply_patch format (from *** Begin Patch to *** End Patch)."
                    }
                },
                "required": ["patch"]
            }),
        },
        requires_approval: true,
        executor: Box::new(move |args| execute(&root, args)),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn setup() -> (TempDir, WorkspaceRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = WorkspaceRoot::new(dir.path()).unwrap();
        (dir, root)
    }

    fn write_file(dir: &TempDir, name: &str, content: &str) {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    fn patch(p: &str) -> serde_json::Value {
        serde_json::json!({"patch": p})
    }

    #[test]
    fn adds_new_file() {
        let (dir, root) = setup();
        let result = execute(
            &root,
            patch("*** Begin Patch\n*** Add File: new.txt\n+hello world\n*** End Patch"),
        )
        .unwrap();

        let on_disk = std::fs::read_to_string(dir.path().join("new.txt")).unwrap();
        assert_eq!(on_disk, "hello world\n");
        assert!(result.contains("Success"));
    }

    #[test]
    fn updates_existing_file() {
        let (dir, root) = setup();
        write_file(&dir, "file.txt", "old line\n");

        let result = execute(
            &root,
            patch(
                "*** Begin Patch\n*** Update File: file.txt\n@@\n-old line\n+new line\n*** End Patch",
            ),
        )
        .unwrap();

        let on_disk = std::fs::read_to_string(dir.path().join("file.txt")).unwrap();
        assert_eq!(on_disk, "new line\n");
        assert!(result.contains("file.txt"));
        assert!(result.contains("-old line"));
        assert!(result.contains("+new line"));
    }

    #[test]
    fn deletes_file() {
        let (dir, root) = setup();
        write_file(&dir, "to_delete.txt", "bye");

        let result = execute(
            &root,
            patch("*** Begin Patch\n*** Delete File: to_delete.txt\n*** End Patch"),
        )
        .unwrap();

        assert!(!dir.path().join("to_delete.txt").exists());
        assert!(result.contains("Success"));
    }

    #[test]
    fn path_escape_rejected() {
        let (_dir, root) = setup();
        let err = execute(
            &root,
            patch(
                "*** Begin Patch\n*** Add File: ../escape.txt\n+bad\n*** End Patch",
            ),
        )
        .unwrap_err();
        assert!(err.to_string().contains("escapes"));
    }

    #[test]
    fn invalid_patch_returns_error() {
        let (_dir, root) = setup();
        let err = execute(&root, patch("this is not a patch")).unwrap_err();
        assert!(err.to_string().contains("apply_patch"));
    }

    #[test]
    fn missing_required_field() {
        let (_dir, root) = setup();
        let err = execute(&root, serde_json::json!({"wrong_field": "x"})).unwrap_err();
        assert!(err.to_string().contains("missing") || err.to_string().contains("patch"));
    }
}
