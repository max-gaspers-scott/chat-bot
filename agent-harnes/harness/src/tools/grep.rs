//! `grep` tool — in-process regex search using `grep-searcher` + `grep-regex`.
//!
//! Walks the workspace tree (respecting `.gitignore` via the `ignore` crate)
//! and searches each file for lines matching a regex pattern.
//!
//! Schema fields:
//! - `pattern`          — the regex pattern
//! - `path`             — subdirectory to search (default: workspace root)
//! - `case_sensitive`   — case-sensitive search (default: false)
//! - `limit`            — max number of matches to return (default: 100)

use anyhow::{Result, anyhow};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::Searcher;
use grep_searcher::sinks::UTF8;
use ignore::WalkBuilder;
use rig_core::completion::ToolDefinition;
use serde::Deserialize;

use crate::agent_loop::ToolEntry;
use crate::tools::WorkspaceRoot;
use crate::tools::truncate::truncate_output;

const DEFAULT_LIMIT: usize = 100;
const MAX_HEAD: usize = 50;
const MAX_TAIL: usize = 50;

#[derive(Debug, Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    limit: Option<usize>,
}

pub fn execute(root: &WorkspaceRoot, args: serde_json::Value) -> Result<String> {
    let args: GrepArgs = serde_json::from_value(args)?;
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);

    let mut builder = RegexMatcherBuilder::new();
    builder.case_insensitive(!args.case_sensitive);
    let matcher = builder
        .build(&args.pattern)
        .map_err(|e| anyhow!("grep: invalid regex pattern: {e}"))?;

    let search_root = if args.path.as_deref().unwrap_or("").is_empty() {
        root.path().to_path_buf()
    } else {
        root.resolve_safe(args.path.as_deref().unwrap_or(".").trim_start_matches('/'))?
    };

    let mut searcher = Searcher::new();

    let mut matches: Vec<String> = Vec::new();
    let mut count = 0usize;

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

        if count >= limit {
            break;
        }

        let path = entry.path();
        let rel = path.strip_prefix(root.path()).unwrap_or(path).to_path_buf();

        let mut file_matches: Vec<(u64, String)> = Vec::new();

        let sink = UTF8(|line_num, line| {
            file_matches.push((line_num, line.to_string()));
            count += 1;
            Ok(count < limit)
        });

        let file = std::fs::File::open(path)
            .map_err(|e| anyhow!("grep: cannot open {}: {e}", path.display()))?;

        if let Err(e) = searcher.search_file(&matcher, &file, sink) {
            // Binary file or read error — skip.
            let _ = e;
            continue;
        }

        for (line_num, line) in &file_matches {
            matches.push(format!(
                "{}:{}: {}",
                rel.display(),
                line_num,
                line.trim_end()
            ));
        }
    }

    if matches.is_empty() {
        return Ok(format!("no matches for pattern `{}`", args.pattern));
    }

    let output = matches.join("\n");
    let truncated = truncate_output(&output, MAX_HEAD, MAX_TAIL);
    Ok(format!(
        "{} match(es) found (showing up to {limit}):\n{truncated}",
        matches.len()
    ))
}

pub fn entry(root: WorkspaceRoot) -> ToolEntry {
    ToolEntry {
        definition: ToolDefinition {
            name: "grep".into(),
            description:
                "Search for a regex pattern in files under the workspace. \
                 Respects .gitignore. Returns matching lines with file path \
                 and line number. Output is truncated to head/tail if very long."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "The regex pattern to search for."
                    },
                    "path": {
                        "type": "string",
                        "description": "Subdirectory to search (relative to workspace root). Defaults to root."
                    },
                    "case_sensitive": {
                        "type": "boolean",
                        "description": "Whether the search is case-sensitive (default: false).",
                        "default": false
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of matches to return (default: 100).",
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
    fn finds_matching_lines() {
        let (dir, root) = setup();
        write_file(&dir, "a.txt", "hello\nworld\nhello again\n");

        let result = execute(&root, serde_json::json!({"pattern": "hello"})).unwrap();
        assert!(result.contains("a.txt:1: hello"));
        assert!(result.contains("a.txt:3: hello again"));
    }

    #[test]
    fn case_insensitive() {
        let (dir, root) = setup();
        write_file(&dir, "a.txt", "Hello\nworld\n");

        let result = execute(
            &root,
            serde_json::json!({"pattern": "hello", "case_sensitive": false}),
        )
        .unwrap();
        assert!(result.contains("Hello"));
    }

    #[test]
    fn case_sensitive_no_match() {
        let (dir, root) = setup();
        write_file(&dir, "a.txt", "Hello\nworld\n");

        let result = execute(
            &root,
            serde_json::json!({"pattern": "hello", "case_sensitive": true}),
        )
        .unwrap();
        assert!(result.contains("no matches"));
    }

    #[test]
    fn no_matches() {
        let (dir, root) = setup();
        write_file(&dir, "a.txt", "hello\nworld\n");

        let result = execute(&root, serde_json::json!({"pattern": "xyz"})).unwrap();
        assert!(result.contains("no matches"));
    }

    #[test]
    fn searches_subdirectory() {
        let (dir, root) = setup();
        write_file(&dir, "src/main.rs", "fn main() {}\n");
        write_file(&dir, "README.md", "nothing here\n");

        let result = execute(
            &root,
            serde_json::json!({"pattern": "fn main", "path": "src"}),
        )
        .unwrap();

        assert!(result.contains("main.rs"));
        assert!(!result.contains("README"));
    }

    #[test]
    fn limit_truncates_results() {
        let (dir, root) = setup();
        for i in 0..10 {
            write_file(
                &dir,
                &format!("f{i}.txt"),
                "match\nmatch\nmatch\nmatch\nmatch\n",
            );
        }

        let result = execute(
            &root,
            serde_json::json!({"pattern": "match", "limit": 5}),
        )
        .unwrap();

        assert!(result.contains("5 match(es)"));
    }

    #[test]
    fn path_escape_rejected() {
        let (_dir, root) = setup();
        let err = execute(
            &root,
            serde_json::json!({"pattern": "test", "path": "../escape"}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("escapes"));
    }

    #[test]
    fn invalid_regex_rejected() {
        let (_dir, root) = setup();
        let err = execute(&root, serde_json::json!({"pattern": "[invalid"})).unwrap_err();
        assert!(err.to_string().contains("invalid regex"));
    }
}
