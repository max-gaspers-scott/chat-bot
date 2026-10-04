//! `harness-cli` — minimal CLI for testing the harness library.
//!
//! Provides: signup, login, chat (hello-world completion), whoami, logout.
//! Not intended as the final UX — real TUI/web UIs come later.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use harness::client::{ClientBuilder, LoginCredentials};
use rig::AgentBuilder;
use serde::Serialize;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

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
        Commands::Whoami => handle_whoami().await,
        Commands::Logout => handle_logout().await,
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
