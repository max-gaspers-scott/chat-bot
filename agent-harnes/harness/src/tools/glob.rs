//! `glob` tool — file pattern matching using `globset` + `ignore` walker.
//!
//! Walks the workspace tree (respecting `.gitignore` via the `ignore` crate)
//! and returns files whose basename matches the given glob pattern(s).
//!
//! Schema fields:
//! - `pattern` — a glob pattern (e.g. `"*.rs"`, `"src/**/*.txt"`)
//! - `path`    — subdirectory to search (default: workspace root)
//! - `limit`   — max number of results to return (default: 200)

use anyhow::{Result, anyhow};
use globset::GlobSetBuilder;
use ignore::WalkBuilder;
use rig_core::completion::ToolDefinition;
use serde::Deserialize;

use crate::agent_loop::ToolEntry;
use crate::tools::WorkspaceRoot;

const DEFAULT_LIMIT: usize = 200;

#[derive(Debug, Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

pub fn execute(root: &WorkspaceRoot, args: serde_json::Value) -> Result<String> {
    let args: GlobArgs = serde_json::from_value(args)?;
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);

    // Build a GlobSet from the pattern.  We also handle the `**/` recursive
    // prefix by converting patterns that start with `**/` so they match at
    // any depth.
    let glob = globset::Glob::new(&args.pattern)
        .map_err(|e| anyhow!("glob: invalid pattern `{}`: {e}", args.pattern))?;
    let globset = GlobSetBuilder::new().add(glob).build()?;

    let search_root = if args.path.as_deref().unwrap_or("").is_empty() {
        root.path().to_path_buf()
    } else {
        root.resolve_safe(args.path.as_deref().unwrap_or(".").trim_start_matches('/'))?
    };

    let mut results: Vec<String> = Vec::new();

    for result in WalkBuilder::new(&search_root)
        .standard_filters(true)
        .hidden(true)
        .build()
    {
        let entry = match result {
            Ok(entry) => entry,
            Err(_) => continue,
        };

        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }

        let file_name = entry.file_name();
        if !globset.is_match(file_name) {
            // Also try matching the full relative path for `**/` patterns.
            let rel = entry
                .path()
                .strip_prefix(root.path())
                .unwrap_or(entry.path());
            if !globset.is_match(rel.to_string_lossy().as_ref()) {
                continue;
            }
        }

        let rel = entry
            .path()
            .strip_prefix(root.path())
            .unwrap_or(entry.path());
        results.push(rel.to_string_lossy().to_string());

        if results.len() >= limit {
            break;
        }
    }

    if results.is_empty() {
        return Ok(format!("no files matching `{}`", args.pattern));
    }

    let output = results.join("\n");
    if results.len() >= limit {
        Ok(format!(
            "{} match(es) found (showing first {limit}):\n{output}",
            results.len()
        ))
    } else {
        Ok(format!("{} match(es) found:\n{output}", results.len()))
    }
}

pub fn entry(root: WorkspaceRoot) -> ToolEntry {
    ToolEntry {
        definition: ToolDefinition {
            name: "glob".into(),
            description:
                "Find files by glob pattern in the workspace. Respects .gitignore. \
                 Returns file paths relative to the workspace root."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern (e.g. '*.rs', 'src/**/*.txt')."
                    },
                    "path": {
                        "type": "string",
                        "description": "Subdirectory to search (relative to workspace root). Defaults to root."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 200).",
                        "minimum": 1
                    }
                },
                "required": ["pattern"]
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn finds_matching_files() {
        let (dir, root) = setup();
        write_file(&dir, "a.rs", "// rust\n");
        write_file(&dir, "b.rs", "// rust\n");
        write_file(&dir, "c.txt", "not rust\n");

        let result = execute(&root, serde_json::json!({"pattern": "*.rs"})).unwrap();
        assert!(result.contains("a.rs"));
        assert!(result.contains("b.rs"));
        assert!(!result.contains("c.txt"));
        assert!(result.contains("2 match(es)"));
    }

    #[test]
    fn recursive_glob() {
        let (dir, root) = setup();
        write_file(&dir, "src/main.rs", "main\n");
        write_file(&dir, "src/utils.rs", "utils\n");
        write_file(&dir, "README.md", "readme\n");

        let result = execute(&root, serde_json::json!({"pattern": "*.rs"})).unwrap();
        assert!(result.contains("main.rs"));
        assert!(result.contains("utils.rs"));
        assert!(!result.contains("README"));
    }

    #[test]
    fn no_matches() {
        let (dir, root) = setup();
        write_file(&dir, "a.txt", "hello\n");

        let result = execute(&root, serde_json::json!({"pattern": "*.rs"})).unwrap();
        assert!(result.contains("no files"));
    }

    #[test]
    fn searches_subdirectory() {
        let (dir, root) = setup();
        write_file(&dir, "src/main.rs", "main\n");
        write_file(&dir, "README.md", "readme\n");

        let result = execute(
            &root,
            serde_json::json!({"pattern": "*.rs", "path": "src"}),
        )
        .unwrap();
        assert!(result.contains("main.rs"));
        assert!(!result.contains("README"));
    }

    #[test]
    fn limit_truncates_results() {
        let (dir, root) = setup();
        for i in 0..10 {
            write_file(&dir, &format!("f{i}.txt"), "data\n");
        }

        let result = execute(
            &root,
            serde_json::json!({"pattern": "*.txt", "limit": 5}),
        )
        .unwrap();

        assert!(result.contains("5 match(es)"));
    }

    #[test]
    fn path_escape_rejected() {
        let (_dir, root) = setup();
        let err = execute(
            &root,
            serde_json::json!({"pattern": "*.txt", "path": "../escape"}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("escapes"));
    }

    #[test]
    fn invalid_pattern_rejected() {
        let (_dir, root) = setup();
        let err = execute(&root, serde_json::json!({"pattern": "["})).unwrap_err();
        assert!(err.to_string().contains("invalid pattern"));
    }
}
