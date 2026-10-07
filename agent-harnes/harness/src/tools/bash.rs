//! `bash` tool — run a shell command with process-group kill and approval.
//!
//! Uses `command_group::AsyncCommandGroup` to spawn the command in its own
//! process group so that a timeout or cancellation kills the entire tree,
//! not just the parent shell.
//!
//! Schema fields:
//! - `command`  — the shell command to run (passed to `sh -c`)
//! - `timeout`  — optional timeout in seconds (default: 30)

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use command_group::AsyncCommandGroup;
use rig_core::completion::ToolDefinition;
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use std::process::Stdio;

use crate::agent_loop::ToolEntry;
use crate::tools::WorkspaceRoot;
use crate::tools::truncate::truncate_output;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_HEAD: usize = 50;
const MAX_TAIL: usize = 50;

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

/// Run a command in its own process group with a timeout.
async fn run_command(
    command: &str,
    cwd: &std::path::Path,
    timeout: Duration,
) -> Result<(i32, String, String)> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .group_spawn()
        .with_context(|| format!("bash: failed to spawn: {command}"))?;

    let outcome = tokio::time::timeout(timeout, async {
        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();

        if let Some(mut out) = child.inner().stdout.take() {
            out.read_to_end(&mut out_buf).await.unwrap_or(0);
        }
        if let Some(mut err) = child.inner().stderr.take() {
            err.read_to_end(&mut err_buf).await.unwrap_or(0);
        }

        let status = child.wait().await?;
        let stdout = String::from_utf8_lossy(&out_buf).into_owned();
        let stderr = String::from_utf8_lossy(&err_buf).into_owned();
        Ok::<_, anyhow::Error>((status, stdout, stderr))
    })
    .await;

    match outcome {
        Ok(Ok((status, stdout, stderr))) => {
            let code = status.code().unwrap_or(1);
            Ok((code, stdout, stderr))
        }
        Ok(Err(e)) => Err(anyhow!("bash: process error: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            Err(anyhow!(
                "command timed out after {}s and was killed",
                timeout.as_secs()
            ))
        }
    }
}

/// Execute the bash tool synchronously.
///
/// The `ToolEntry::executor` is a sync `Box<dyn Fn>`, but the command logic
/// is async (tokio process + timeout). We drive it on a *separate* thread
/// with its own current-thread runtime rather than `block_on`ing inside the
/// agent loop's runtime — `block_on` panics when called from within a runtime.
pub fn execute(root: &WorkspaceRoot, args: serde_json::Value) -> Result<String> {
    let args: BashArgs = serde_json::from_value(args)?;
    let timeout = Duration::from_secs(args.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let cwd = root.path().to_path_buf();
    let command = args.command.clone();

    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        match rt {
            Ok(rt) => rt.block_on(run_command(&command, &cwd, timeout)),
            Err(e) => Err(anyhow!("bash: failed to create async runtime: {e}")),
        }
    });

    let (code, stdout, stderr) = handle
        .join()
        .map_err(|e| anyhow!("bash: executor thread panicked: {e:?}"))??;

    if stdout.is_empty() && stderr.is_empty() {
        return Ok(format!("[exit code: {code}] (no output)"));
    }

    let mut out = String::new();
    if !stdout.is_empty() {
        out.push_str(&truncate_output(&stdout, MAX_HEAD, MAX_TAIL));
    }
    if !stderr.is_empty() {
        out.push_str("\n--- stderr ---\n");
        out.push_str(&truncate_output(&stderr, MAX_HEAD, MAX_TAIL));
    }
    Ok(format!("[exit code: {code}]\n{out}"))
}

/// Build the `ToolEntry` for `bash`.
pub fn entry(root: WorkspaceRoot) -> ToolEntry {
    ToolEntry {
        definition: ToolDefinition {
            name: "bash".into(),
            description:
                "Execute a shell command in the workspace directory. The command runs in its own \
                 process group so that timeouts and cancellations kill the entire process tree. \
                 Output is truncated to head/tail if very large. Requires approval every time."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute (run via sh -c)."
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 30).",
                        "minimum": 1
                    }
                },
                "required": ["command"]
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

    fn setup() -> (tempfile::TempDir, WorkspaceRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = WorkspaceRoot::new(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn runs_simple_command() {
        let (_dir, root) = setup();
        let result = execute(&root, serde_json::json!({"command": "echo hello"})).unwrap();
        assert!(result.contains("hello"));
    }

    #[test]
    fn exit_code_zero() {
        let (_dir, root) = setup();
        let result = execute(&root, serde_json::json!({"command": "exit 0"})).unwrap();
        assert!(result.contains("exit code: 0"));
    }

    #[test]
    fn nonzero_exit_code() {
        let (_dir, root) = setup();
        let result = execute(&root, serde_json::json!({"command": "exit 3"})).unwrap();
        assert!(result.contains("exit code: 3"));
    }

    #[test]
    fn timeout_kills_process() {
        let (_dir, root) = setup();
        let result = execute(
            &root,
            serde_json::json!({"command": "sleep 10", "timeout": 1}),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[test]
    fn timeout_kills_process_tree() {
        let (_dir, root) = setup();
        let result = execute(
            &root,
            serde_json::json!({"command": "sh -c 'sleep 5 & sleep 10'", "timeout": 1}),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[test]
    fn output_truncation() {
        let (_dir, root) = setup();
        let result = execute(
            &root,
            serde_json::json!({"command": "for i in $(seq 1 200); do echo line$i; done"}),
        )
        .unwrap();
        assert!(result.contains("line1"));
        assert!(result.contains("line200"));
        assert!(result.contains("lines omitted"));
    }

    #[test]
    fn runs_in_workspace() {
        let (dir, root) = setup();
        let result = execute(&root, serde_json::json!({"command": "pwd"})).unwrap();
        assert!(result.contains(dir.path().to_str().unwrap()));
    }

    #[test]
    fn stderr_captured() {
        let (_dir, root) = setup();
        let result = execute(&root, serde_json::json!({"command": "echo oops >&2"})).unwrap();
        assert!(result.contains("stderr"));
        assert!(result.contains("oops"));
    }
}
