//! `harness-cli` — minimal CLI for testing the harness library.
//!
//! Provides: signup, login, chat (hello-world completion), run (agent loop),
//! whoami, logout.
//! Not intended as the final UX — real TUI/web UIs come later.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use harness::agent_loop::{LoopConfig, TaskOutcome, run_task};
use harness::client::{ClientBuilder, LoginCredentials};
use harness::events::HarnessEvent;
use harness::memory::{InMemoryConversation, Memory};
use harness::tools::all_tools;
use rig::AgentBuilder;
use serde::Serialize;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MODEL: &str = "gemini-2.5-flash";
const PREAMBLE: &str = "You are a helpful assistant.";

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "harness-cli",
    about = "CLI for the mgs-ai-proxy — sign up, log in, and chat with Gemini",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    /// Base URL of the mgs-ai-proxy backend
    #[arg(
        long,
        env = "MGS_API_URL",
        default_value = "https://cloud-wolf.team-stingray.com"
    )]
    api_url: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new account
    Signup {
        #[arg(short, long)]
        username: String,
        #[arg(short, long)]
        email: String,
        #[arg(short, long)]
        password: String,
        #[arg(long, default_value_t = false)]
        pro: bool,
    },
    /// Log in and save a JWT token locally
    Login {
        #[arg(short, long)]
        email: String,
        #[arg(short, long)]
        password: String,
    },
    /// Send a message to Gemini (requires login first)
    Chat {
        /// The message to send (joined with spaces if multiple args)
        message: Vec<String>,
    },
    /// Run the Phase-2 agent loop against the real proxy (requires login first)
    Run {
        /// The task to send (joined with spaces if multiple args)
        task: Vec<String>,
        /// Maximum model turns before giving up
        #[arg(long, default_value_t = 20)]
        max_turns: usize,
        /// System prompt override
        #[arg(long)]
        system: Option<String>,
        /// Write a JSONL transcript to this directory
        #[arg(long)]
        transcript_dir: Option<PathBuf>,
        /// Workspace root directory (tools are restricted to this path)
        #[arg(short, long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Show the currently saved token path
    Whoami,
    /// Delete the saved token (log out)
    Logout,
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SignupPayload {
    username: String,
    email: String,
    password: String,
    is_pro: bool,
}

// ---------------------------------------------------------------------------
// Token persistence
// ---------------------------------------------------------------------------

fn token_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("could not determine config directory")?;
    let dir = config_dir.join("mgs-cli");
    fs::create_dir_all(&dir).context("could not create config directory")?;
    Ok(dir.join("token"))
}

fn save_token(token: &str) -> Result<()> {
    let path = token_path()?;
    fs::write(&path, token).context("could not write token file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn load_token() -> Result<String> {
    let path = token_path()?;
    let raw = fs::read_to_string(&path).context("no saved token — run `harness-cli login` first")?;
    Ok(raw.trim().to_string())
}

fn delete_token() -> Result<()> {
    let path = token_path()?;
    if path.exists() {
        fs::remove_file(&path).context("could not delete token file")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

async fn handle_signup(
    api_url: &str,
    username: String,
    email: String,
    password: String,
    is_pro: bool,
) -> Result<()> {
    let client = reqwest::Client::new();
    let payload = SignupPayload { username, email, password, is_pro };

    let resp = client
        .post(format!("{api_url}/api/post_user"))
        .json(&payload)
        .send()
        .await
        .context("could not reach the server")?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.context("server returned invalid JSON")?;

    if status.is_success() && body["res"] == "success" {
        println!("✓ Account created! Run `harness-cli login` to get started.");
    } else {
        bail!(
            "signup failed (HTTP {status}): {}",
            body["res"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(())
}

async fn handle_login(api_url: &str, email: String, password: String) -> Result<()> {
    let credentials = LoginCredentials { email, password };
    // Use ClientBuilder which hits /api/login and parses the response.
    let builder = ClientBuilder::new(api_url)
        .login(credentials)
        .await
        .context("login failed")?;

    // Extract the token from the store so we can persist it.
    let (_client, store) = builder.build().await?;
    let token = store.read().await.token.clone();
    save_token(&token)?;

    let path = token_path()?;
    println!("✓ Logged in. Token saved to {}", path.display());
    Ok(())
}

/// Hello-world completion: load the saved token, build the client, send one
/// message, print the reply.
async fn handle_chat(api_url: &str, message: Vec<String>) -> Result<()> {
    let text = if message.is_empty() {
        print!("You: ");
        io::stdout().flush()?;
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        buf.trim().to_string()
    } else {
        message.join(" ")
    };

    if text.is_empty() {
        bail!("message cannot be empty");
    }

    let token = load_token()?;

    // Build the harness client from the saved token without re-authenticating.
    // We construct a minimal in-memory store from the persisted token and use
    // HarnessHttpClient directly (no background refresh — user reruns `login`
    // if the token expires and a 401 occurs).
    let store = {
        use harness::client::TokenState;
        use std::time::{Duration, Instant};
        use tokio::sync::RwLock;

        Arc::new(RwLock::new(TokenState {
            token: token.clone(),
            // Treat the token as valid for 24h; the 401 retry handles actual expiry.
            expires_at: Instant::now() + Duration::from_secs(24 * 3600),
        }))
    };

    // Build a minimal HarnessHttpClient directly.
    // For the CLI we don't have credentials to proactively refresh, so we
    // skip the background task and rely on 401 retry (which will fail gracefully
    // if credentials aren't available — user just re-runs `login`).
    let refresher = {
        // Dummy credentials — won't be called unless we hit a 401.
        harness::client::TokenRefresher::new_for_existing_token(api_url.to_string())
    };

    let transport = harness::client::HarnessHttpClient::from_parts(
        Arc::clone(&store),
        refresher,
    );

    let rig_client = harness::client::mira_openai_client(api_url, transport);

    let agent = AgentBuilder::new(rig_client.chat(MODEL))
        .preamble(PREAMBLE)
        .build();

    match agent.prompt(text).await {
        Ok(response) => {
            println!("\nGemini: {}", response.output);
            Ok(())
        }
        Err(e) => {
            let rendered = format!("{e}");
            if rendered.contains("401") || rendered.contains("InvalidAuthentication") {
                bail!("token expired or invalid — run `harness-cli login` again");
            }
            Err(anyhow::Error::new(e).context("chat failed"))
        }
    }
}

// ---------------------------------------------------------------------------
// Shared client builder from saved token
// ---------------------------------------------------------------------------

fn build_transport(
    api_url: &str,
    token: &str,
) -> (harness::client::HarnessHttpClient, Arc<tokio::sync::RwLock<harness::client::TokenState>>) {
    use harness::client::TokenState;
    use std::time::{Duration, Instant};
    use tokio::sync::RwLock;

    let store = Arc::new(RwLock::new(TokenState {
        token: token.to_string(),
        expires_at: Instant::now() + Duration::from_secs(24 * 3600),
    }));

    let refresher = harness::client::TokenRefresher::new_for_existing_token(api_url.to_string());
    let transport = harness::client::HarnessHttpClient::from_parts(Arc::clone(&store), refresher);
    (transport, store)
}

// ---------------------------------------------------------------------------
// handle_run — drives the Phase-2 agent loop in an interactive REPL
// ---------------------------------------------------------------------------

async fn handle_run(
    api_url: &str,
    task: Vec<String>,
    max_turns: usize,
    system: Option<String>,
    transcript_dir: Option<PathBuf>,
    workspace: PathBuf,
) -> Result<()> {
    let token = load_token()?;
    let (transport, _store) = build_transport(api_url, &token);
    let rig_client = harness::client::mira_openai_client(api_url, transport);

    // Erase to DynModel<Completion> — what run_task expects.
    use rig_core::operation::Completion;
    let model: rig_core::DynModel<Completion> = rig_client.chat(MODEL).erase();

    // Shared memory across all turns — this is what we're testing.
    let mut memory = InMemoryConversation::new();

    let session_id = format!(
        "cli-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    );

    // Resolve workspace to absolute path
    let workspace_root = std::fs::canonicalize(&workspace)
        .with_context(|| format!("could not resolve workspace path: {}", workspace.display()))?;

    let config = LoopConfig {
        max_turns,
        system_prompt: system.or_else(|| Some(PREAMBLE.to_string())),
        transcript_dir,
        session_id,
        model_id: Some(MODEL.to_string()),
        tools: all_tools(workspace_root),
        ..LoopConfig::default()
    };

    // Get the first message from the CLI args, or drop into the REPL immediately.
    let first = if task.is_empty() { None } else { Some(task.join(" ")) };

    eprintln!("Interactive agent loop. Type your message and press Enter.");
    eprintln!("Commands: :quit or :q to exit, :history to show memory, :clear to wipe memory.");
    eprintln!("─────────────────────────────────────────────────────────");

    let mut turn_number: usize = 0;

    loop {
        // Read input: use CLI arg on the very first iteration, then prompt.
        let text = if turn_number == 0 {
            if let Some(first_msg) = first.clone() {
                eprintln!("You: {first_msg}");
                first_msg
            } else {
                read_line("You: ")?
            }
        } else {
            read_line("You: ")?
        };

        // Handle REPL commands.
        match text.trim() {
            ":quit" | ":q" | "" => {
                eprintln!("Bye.");
                break;
            }
            ":history" => {
                let history = memory.load();
                if history.is_empty() {
                    eprintln!("[history is empty]");
                } else {
                    eprintln!("[history — {} messages]", history.len());
                    for (i, msg) in history.iter().enumerate() {
                        let role = match msg {
                            harness::memory::Message::User { .. } => "user     ",
                            harness::memory::Message::Assistant { .. } => "assistant",
                            harness::memory::Message::System { .. } => "system   ",
                        };
                        eprintln!("  [{i}] {role}: {}", msg_text(msg));
                    }
                }
                continue;
            }
            ":clear" => {
                memory.replace(vec![]);
                eprintln!("[memory cleared]");
                continue;
            }
            _ => {}
        }

        turn_number += 1;
        eprint!("Gemini: ");
        io::stdout().flush()?;

        // Fresh channel and cancel token for each user turn.
        let (tx, mut rx) = mpsc::channel::<HarnessEvent>(64);
        let cancel = CancellationToken::new();

        // Print events inline as they arrive.
        let printer = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    HarnessEvent::TextDelta { delta } => {
                        print!("{delta}");
                        let _ = io::stdout().flush();
                    }
                    HarnessEvent::ToolCallStarted { name, arguments, .. } => {
                        eprintln!("\n  [tool →] {name}({arguments})");
                    }
                    HarnessEvent::ToolCallFinished { name, output, .. } => {
                        eprintln!("  [tool ←] {name}: {output}");
                        eprint!("Gemini: ");
                        let _ = io::stdout().flush();
                    }
                    HarnessEvent::ApprovalRequested { name, preview, .. } => {
                        eprintln!("\n  [approval needed] {name}\n  {preview}");
                    }
                    HarnessEvent::TurnFinished { .. }
                    | HarnessEvent::TaskComplete { .. } => {}
                    HarnessEvent::MaxTurnsReached { max_turns } => {
                        eprintln!("\n[max turns ({max_turns}) reached]");
                    }
                    HarnessEvent::Cancelled => {
                        eprintln!("\n[cancelled]");
                    }
                    HarnessEvent::Error { message, fatal } => {
                        eprintln!(
                            "\n[error{f}] {message}",
                            f = if fatal { " (fatal)" } else { "" }
                        );
                    }
                }
            }
        });

        let outcome = run_task(&model, text, &mut memory, &tx, cancel, &config).await;

        drop(tx);
        let _ = printer.await;
        println!(); // newline after the model's response

        match outcome {
            Ok(TaskOutcome::Completed { .. }) => {}
            Ok(TaskOutcome::MaxTurnsReached) => {
                eprintln!("[max turns reached — memory has {} messages]", memory.load().len());
            }
            Ok(TaskOutcome::Cancelled) => {
                eprintln!("[cancelled]");
            }
            Err(e) => {
                eprintln!("[error] {e:#}");
                // Don't exit — let the user try again or inspect history.
            }
        }

        eprintln!(
            "  (memory: {} messages)",
            memory.load().len()
        );
        eprintln!("─────────────────────────────────────────────────────────");
    }

    Ok(())
}

/// Print `prompt` and read a line from stdin.
fn read_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

/// Extract the first text content from a message for display.
fn msg_text(msg: &harness::memory::Message) -> String {
    use harness::memory::Message;
    match msg {
        Message::System { content } => content.chars().take(80).collect(),
        Message::User { content } => content
            .iter()
            .find_map(|c| {
                if let harness::memory::UserContent::Text(t) = c {
                    Some(t.text.chars().take(80).collect())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "[non-text]".into()),
        Message::Assistant { content, .. } => content
            .iter()
            .find_map(|c| {
                if let harness::memory::AssistantContent::Text(t) = c {
                    Some(t.text.chars().take(80).collect())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "[tool call]".into()),
    }
}

async fn handle_whoami() -> Result<()> {
    let path = token_path()?;
    if path.exists() {
        println!("Token file: {}", path.display());
    } else {
        println!("No token saved. Run `harness-cli login` first.");
    }
    Ok(())
}

async fn handle_logout() -> Result<()> {
    delete_token()?;
    println!("✓ Logged out.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Signup { username, email, password, pro } => {
            handle_signup(&cli.api_url, username, email, password, pro).await
        }
        Commands::Login { email, password } => handle_login(&cli.api_url, email, password).await,
        Commands::Chat { message } => handle_chat(&cli.api_url, message).await,
        Commands::Run { task, max_turns, system, transcript_dir, workspace } => {
            handle_run(&cli.api_url, task, max_turns, system, transcript_dir, workspace).await
        }
        Commands::Whoami => handle_whoami().await,
        Commands::Logout => handle_logout().await,
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
