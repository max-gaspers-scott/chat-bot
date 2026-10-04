use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rig::AgentBuilder;
use rig::providers::openai;
use rig::providers::openai::wire::{self, BodyRewrite, Dialect};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

/// Model requested from the proxy for every chat turn.
const MODEL: &str = "gemini-2.5-flash";

/// System prompt sent ahead of the user's message.
const PREAMBLE: &str = "You are a helpful assistant.";

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "mgs",
    about = "CLI for the mgs-ai-proxy — sign up, log in, and chat with Gemini",
    version = "0.1.0"
)]
struct Cli {
    /// Base URL of the mgs-ai-proxy backend (default: http://localhost:8081)
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
        /// Your username
        #[arg(short, long)]
        username: String,
        /// Your email address
        #[arg(short, long)]
        email: String,
        /// Your password
        #[arg(short, long)]
        password: String,
        /// Sign up as a pro user
        #[arg(long, default_value_t = false)]
        pro: bool,
    },
    /// Log in and save a JWT token locally
    Login {
        /// Your email address
        #[arg(short, long)]
        email: String,
        /// Your password
        #[arg(short, long)]
        password: String,
    },
    /// Send a message to Gemini (requires login first)
    Chat {
        /// The message to send
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

#[derive(Serialize)]
struct LoginPayload {
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct LoginResponse {
    res: String,
    token: Option<String>,
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
    // Restrict permissions to owner-only on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn load_token() -> Result<String> {
    let path = token_path()?;
    let raw = fs::read_to_string(&path).context("no saved token — run `mgs login` first")?;
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

fn handle_signup(
    api_url: &str,
    username: String,
    email: String,
    password: String,
    is_pro: bool,
) -> Result<()> {
    let client = reqwest::blocking::Client::new();
    let payload = SignupPayload {
        username,
        email,
        password,
        is_pro,
    };

    let resp = client
        .post(format!("{api_url}/api/post_user"))
        .json(&payload)
        .send()
        .context("could not reach the server")?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().context("server returned invalid JSON")?;

    if status.is_success() {
        if body["res"] == "success" {
            println!("✓ Account created! Run `mgs login` to get started.");
        } else {
            bail!(
                "signup failed: {}",
                body["res"].as_str().unwrap_or("unknown error")
            );
        }
    } else {
        bail!(
            "signup failed (HTTP {}): {}",
            status,
            body["res"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(())
}

fn handle_login(api_url: &str, email: String, password: String) -> Result<()> {
    let client = reqwest::blocking::Client::new();
    let payload = LoginPayload { email, password };

    let resp = client
        .post(format!("{api_url}/api/login"))
        .json(&payload)
        .send()
        .context("could not reach the server")?;

    let status = resp.status();
    let body: LoginResponse = resp.json().context("server returned invalid JSON")?;

    if status.is_success() {
        if let Some(token) = body.token {
            save_token(&token)?;
            let path = token_path()?;
            println!("✓ Logged in. Token saved to {}", path.display());
        } else {
            bail!("login failed: {}", body.res);
        }
    } else {
        bail!("login failed (HTTP {}): {}", status, body.res);
    }
    Ok(())
}

/// OpenAI's dialect, with the one body rewrite the proxy needs.
///
/// rig serializes a system message's `content` as an array of text parts —
/// legal for OpenAI, but the proxy's deserializer types `content` as a
/// string, so it answers 422. The `Mira` rewrite flattens content-part arrays
/// back to plain strings; every other quirk stays OpenAI's, which keeps the
/// `/chat/completions` path and bearer auth we already depend on.
fn proxy_dialect() -> Dialect {
    let mut quirks = wire::OPENAI.quirks;
    quirks.rewrite = BodyRewrite::Mira;
    Dialect {
        quirks,
        ..wire::OPENAI
    }
}

fn handle_chat(api_url: &str, message: Vec<String>) -> Result<()> {
    // If message words were passed as args, join them; otherwise prompt interactively.
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

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start the async runtime")?;

    let reply = rt.block_on(async {
        // The proxy speaks the OpenAI wire protocol, so aim rig's OpenAI
        // provider at it instead of api.openai.com. `.chat()` keeps it on
        // POST /chat/completions rather than the newer /responses route.
        let client = openai::OpenAIConfig::with_key(&proxy_dialect(), &token)
            .with_base_url(format!("{api_url}/v1"))
            .client();

        let agent = AgentBuilder::new(client.chat(MODEL))
            .preamble(PREAMBLE)
            .build();

        agent.prompt(text).await
    });

    match reply {
        Ok(response) => {
            println!("\nGemini: {}", response.output);
            Ok(())
        }
        Err(e) => {
            let rendered = format!("{e}");
            if rendered.contains("401") || rendered.contains("InvalidAuthentication") {
                bail!("token expired or invalid — run `mgs login` again");
            }
            Err(anyhow::Error::new(e).context("chat failed"))
        }
    }
}

fn handle_whoami() -> Result<()> {
    let path = token_path()?;
    if path.exists() {
        println!("Token file: {}", path.display());
    } else {
        println!("No token saved. Run `mgs login` first.");
    }
    Ok(())
}

fn handle_logout() -> Result<()> {
    delete_token()?;
    println!("✓ Logged out.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Signup {
            username,
            email,
            password,
            pro,
        } => handle_signup(&cli.api_url, username, email, password, pro),
        Commands::Login { email, password } => handle_login(&cli.api_url, email, password),
        Commands::Chat { message } => handle_chat(&cli.api_url, message),
        Commands::Whoami => handle_whoami(),
        Commands::Logout => handle_logout(),
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
