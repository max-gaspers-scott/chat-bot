//! `read_file` tool — read a file with optional offset/limit and line numbers.
//!
//! Schema fields:
//! - `path`   — path relative to (or absolute within) workspace root
//! - `offset` — 1-based line number to start from (default: 1)
//! - `limit`  — max lines to return (default: 200)

use anyhow::Result;
use rig_core::completion::ToolDefinition;
use serde::Deserialize;

use crate::agent_loop::ToolEntry;
use crate::tools::truncate::truncate_output;
use crate::tools::WorkspaceRoot;

/// Default maximum lines returned per call.
const DEFAULT_LIMIT: usize = 200;
/// Head/tail cap when truncating oversized outputs.
const HEAD: usize = 50;
const TAIL: usize = 50;

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

/// Execute the read_file logic synchronously (called from the ToolEntry executor).
pub fn execute(root: &WorkspaceRoot, args: serde_json::Value) -> Result<String> {
    let args: ReadFileArgs = serde_json::from_value(args)?;

    let abs_path = root.resolve_safe(&args.path)?;
    let content = std::fs::read_to_string(&abs_path)
        .map_err(|e| anyhow::anyhow!("read_file: cannot read `{}`: {e}", abs_path.display()))?;

    let lines: Vec<&str> = content.split('\n').collect();
    let total = lines.len();

    // offset is 1-based; default to 1.
    let start = args.offset.unwrap_or(1).max(1).min(total + 1);
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT);

    // Convert to 0-based index.
    let from = start - 1;
    let to = (from + limit).min(total);

    if from >= total {
        return Ok(format!(
            "[file has {total} lines; offset {start} is past end of file]"
        ));
    }

    let slice = &lines[from..to];
    // Build numbered output.
    let numbered: String = slice
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6} | {line}", from + i + 1))
        .collect::<Vec<_>>()
        .join("\n");

    let result = if to < total {
        // There are more lines after the slice.
        let remaining = total - to;
        format!(
            "{numbered}\n[... {remaining} more lines. \
             Use read_file with offset={} to continue ...]",
            to + 1
        )
    } else {
        numbered
    };

    Ok(truncate_output(&result, HEAD, TAIL))
}

/// Build the `ToolEntry` for `read_file`.
pub fn entry(root: WorkspaceRoot) -> ToolEntry {
    ToolEntry {
        definition: ToolDefinition {
            name: "read_file".into(),
            description:
                "Read a file from the workspace, returning its contents with line numbers. \
                 Use `offset` (1-based line number) and `limit` to page through large files. \
                 Default limit is 200 lines. Output is capped to head/tail if very long."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the workspace root."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "1-based line number to start reading from (default: 1).",
                        "minimum": 1
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to return (default: 200).",
                        "minimum": 1
                    }
                },
                "required": ["path"]
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
    use std::io::Write;
    use tempfile::TempDir;

    fn setup() -> (TempDir, WorkspaceRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = WorkspaceRoot::new(dir.path()).unwrap();
        (dir, root)
    }

    fn write_file(dir: &TempDir, name: &str, content: &str) {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn reads_all_lines() {
        let (dir, root) = setup();
        write_file(&dir, "test.txt", "alpha\nbeta\ngamma");
        let out = execute(&root, serde_json::json!({"path": "test.txt"})).unwrap();
        assert!(out.contains("alpha"));
        assert!(out.contains("beta"));
        assert!(out.contains("gamma"));
        // Line numbers present
        assert!(out.contains("     1 |"));
        assert!(out.contains("     3 |"));
    }

    #[test]
    fn offset_skips_lines() {
        let (dir, root) = setup();
        write_file(&dir, "test.txt", "line1\nline2\nline3\nline4");
        let out = execute(&root, serde_json::json!({"path": "test.txt", "offset": 3})).unwrap();
        assert!(!out.contains("line1"));
        assert!(!out.contains("line2"));
        assert!(out.contains("line3"));
        assert!(out.contains("line4"));
    }

    #[test]
    fn limit_truncates() {
        let (dir, root) = setup();
        write_file(&dir, "test.txt", "a\nb\nc\nd\ne");
        let out = execute(&root, serde_json::json!({"path": "test.txt", "limit": 2})).unwrap();
        assert!(out.contains("a"));
        assert!(out.contains("b"));
        // Should mention more lines remaining
        assert!(out.contains("more lines"));
    }

    #[test]
    fn offset_past_end() {
        let (dir, root) = setup();
        write_file(&dir, "test.txt", "one\ntwo");
        let out = execute(&root, serde_json::json!({"path": "test.txt", "offset": 100})).unwrap();
        assert!(out.contains("past end of file"));
    }

    #[test]
    fn path_escape_rejected() {
        let (_dir, root) = setup();
        let err = execute(&root, serde_json::json!({"path": "../escape.txt"})).unwrap_err();
        assert!(err.to_string().contains("escapes"));
    }

    #[test]
    fn missing_file_clear_error() {
        let (_dir, root) = setup();
        let err = execute(&root, serde_json::json!({"path": "doesnotexist.txt"})).unwrap_err();
        assert!(err.to_string().contains("read_file:"));
    }
}
