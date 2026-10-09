//! `write_file` tool — write or overwrite a file atomically.
//!
//! Uses `similar` to embed a unified diff in the tool output so the
//! frontend/model can see what changed.
//!
//! Schema fields:
//! - `path`    — path relative to (or absolute within) workspace root
//! - `content` — new file contents (full text, not a patch)

use anyhow::Result;
use rig_core::completion::ToolDefinition;
use serde::Deserialize;
use similar::TextDiff;

use crate::agent_loop::ToolEntry;
use crate::tools::WorkspaceRoot;

#[derive(Debug, Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

/// Generate a unified diff string between `old` and `new` text.
pub fn unified_diff(old: &str, new: &str, path: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut unified = diff.unified_diff();
    unified.header(&format!("{path} (before)"), &format!("{path} (after)"));
    let _ = unified.to_writer(&mut buf);
    String::from_utf8(buf.into_inner()).unwrap_or_else(|_| String::new())
}

/// Execute the write_file logic synchronously.
pub fn execute(root: &WorkspaceRoot, args: serde_json::Value) -> Result<String> {
    let args: WriteFileArgs = serde_json::from_value(args)?;

    let abs_path = root.resolve_safe(&args.path)?;

    // Read existing content for diff (empty string if new file).
    let old_content = std::fs::read_to_string(&abs_path).unwrap_or_default();

    // Create parent directories if needed.
    if let Some(parent) = abs_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Atomic write: write to a temp file next to the target, then rename.
    let tmp_path = abs_path.with_extension("__harness_tmp__");
    std::fs::write(&tmp_path, &args.content)
        .map_err(|e| anyhow::anyhow!("write_file: could not write temp file: {e}"))?;
    std::fs::rename(&tmp_path, &abs_path)
        .map_err(|e| anyhow::anyhow!("write_file: could not rename temp file: {e}"))?;

    let bytes = args.content.len();
    let diff = unified_diff(&old_content, &args.content, &args.path);

    Ok(format!("wrote {bytes} bytes to `{}`\n\n{diff}", args.path))
}

/// Build the `ToolEntry` for `write_file`.
pub fn entry(root: WorkspaceRoot) -> ToolEntry {
    ToolEntry {
        definition: ToolDefinition {
            name: "write_file".into(),
            description:
                "Write (or overwrite) a file in the workspace with the given content. \
                 Creates parent directories automatically. \
                 The output includes a unified diff showing what changed. \
                 Use apply_patch for surgical edits; use write_file when replacing the whole file."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the workspace root."
                    },
                    "content": {
                        "type": "string",
                        "description": "Full new content of the file."
                    }
                },
                "required": ["path", "content"]
            }),
        },
        requires_approval: false,
        executor: Box::new(move |args| execute(&root, args)),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, WorkspaceRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = WorkspaceRoot::new(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn writes_new_file() {
        let (dir, root) = setup();
        let result = execute(
            &root,
            serde_json::json!({"path": "hello.txt", "content": "hello world\n"}),
        )
        .unwrap();
        let on_disk = std::fs::read_to_string(dir.path().join("hello.txt")).unwrap();
        assert_eq!(on_disk, "hello world\n");
        assert!(result.contains("wrote"));
        // Diff should show the new content as additions
        assert!(result.contains('+'));
    }

    #[test]
    fn overwrites_existing_file() {
        let (dir, root) = setup();
        std::fs::write(dir.path().join("file.txt"), "old content\n").unwrap();
        let result = execute(
            &root,
            serde_json::json!({"path": "file.txt", "content": "new content\n"}),
        )
        .unwrap();
        let on_disk = std::fs::read_to_string(dir.path().join("file.txt")).unwrap();
        assert_eq!(on_disk, "new content\n");
        assert!(result.contains("new content"));
        // Diff shows removal of old and addition of new
        assert!(result.contains('-'));
        assert!(result.contains('+'));
    }

    #[test]
    fn creates_parent_dirs() {
        let (dir, root) = setup();
        let result = execute(
            &root,
            serde_json::json!({"path": "a/b/c/new.txt", "content": "nested\n"}),
        )
        .unwrap();
        let on_disk = std::fs::read_to_string(dir.path().join("a/b/c/new.txt")).unwrap();
        assert_eq!(on_disk, "nested\n");
        assert!(result.contains("wrote"));
    }

    #[test]
    fn path_escape_rejected() {
        let (_dir, root) = setup();
        let err = execute(
            &root,
            serde_json::json!({"path": "../escape.txt", "content": "bad"}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("escapes"));
    }

    #[test]
    fn diff_shows_changes() {
        let diff = unified_diff("foo\nbar\n", "foo\nbaz\n", "test.txt");
        assert!(diff.contains("-bar"));
        assert!(diff.contains("+baz"));
        assert!(diff.contains(" foo"));
    }

    #[test]
    fn diff_new_file_all_additions() {
        let diff = unified_diff("", "hello\nworld\n", "new.txt");
        assert!(diff.contains("+hello"));
        assert!(diff.contains("+world"));
    }
}
