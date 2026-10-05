// This project is licensed under Apache 2.0
use anyhow::Context;
use rig::providers::gemini::completion::gemini_api_types;
use rig::providers::openai::{OpenAICompatibleProvider, OpenAIRequestParams, OpenAIResponsesExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use uuid::Uuid;

use core::panic;
use diffy::{Patch, apply as diffy_apply};
use dotenv::dotenv;
use rig::memory::InMemoryConversationMemory;

use rig::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use rig::{prelude::*, providers};
use rig_compose::{KernelError, LocalTool, ToolRegistry, ToolSchema};
use rig_core::providers::openai;
use rig_mcp::{LoopbackTransport, McpTool, McpTransport};
use std::{env, result::Result, sync::Arc};

#[tokio::main]
async fn main() {
    let user = get_jwt().await;
    // let id = uuid!("b4fbbad7-a13c-4dc2-b1f3-9776f6f47e2d");
    // getget_chatid chats
    let id = get_chat_id(&user).await;

    let agent = setup_agent().await;
    let fn_with_agent_stat = async |txt| -> String { call_ai(txt, &agent).await.unwrap() };
    chat_loop(&user, &id, |txt| {
        let agent = &agent;
        async move { call_ai(&txt, agent).await.unwrap() }
    })
    .await
    .unwrap();
}

async fn setup_agent() -> rig::Agent {
    dotenv().ok();

    let api_key_name = "AI_ENG";
    let api_key: String = match env::var(api_key_name) {
        Ok(val) => val.trim().to_string(),
        Err(e) => {
            println!("couldn't interpret {api_key_name}: {e}");
            format!("{}", e)
        }
    };
    let client = openai::Client::new(api_key).expect("no api key! add one to env");
    let memory = InMemoryConversationMemory::new();
    let mcp_tools = setup_mcp_tools().await.expect("failed to set up MCP tools");
    let mut agent = client
        .agent("gpt-3.5-turbo")
        .preamble(
            "You are a helpful assistant with access to tools that can read and edit files, \
             list directories, and execute bash commands. Use these tools to help the user \
             with their requests.",
        )
        .dynamic_tools(mcp_tools)
        .memory(memory)
        .build();
    agent
}

// async fn chat_loop(f: fn(str) -> str) -> Result<(), anyhow::Error> {todo()!}

async fn get_chat_id(user: &LoginPayload) -> uuid::Uuid {
    let chats = get_chats(user).await.unwrap();

    let chats = if chats.status == "success" {
        chats.payload
    } else {
        println!("status: {}", chats.status);
        panic!()
    };
    // ask user to chat name
    let chat_name = cool_cli_input::get_input("what is the name of the chat you want to lisen in");
    let chat_name = chat_name.trim();
    // gett uuid

    // ****************  BAD CODE ****************** //
    let mut id: Option<Uuid> = None;
    for c in chats {
        if let SendibleContent::Title(m) = c.content {
            let name = m.title;
            if name == chat_name {
                id = Some(c.message_id);
            }
        }
    }
    match id {
        Some(id) => id,
        _ => {
            println!("error happend");
            panic!();
            // get_chat_id(user).await
        }
    }
}

async fn chat_loop<F, Fut>(
    user: &LoginPayload,
    id: &Uuid,
    response_fn: F,
) -> Result<(), anyhow::Error>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = String>,
{
    let agent = setup_agent().await;

    let mut last = get_message(&user, &id).await.unwrap();
    loop {
        let new = get_message(user, id).await.unwrap();

        let new_text = match &new.content {
            SendibleContent::Text(t) => Some(t.text.clone()),
            _ => None,
        };
        let last_text = match &last.content {
            SendibleContent::Text(t) => Some(t.text.clone()),
            _ => None,
        };

        if new_text != last_text {
            println!(
                "1, received: {}",
                new_text.as_deref().unwrap_or("(non-text)")
            );

            if new.sender_name != user.username
                && let Some(_text) = &new_text
            {
                //&new_text.clone().unwrap()
                let response = response_fn(new_text.clone().unwrap()).await;
                let echo = SendMessage {
                    sender_name: user.username.clone(),
                    parent_id: Some(*id), //TODO: scary code, should chage
                    content: serde_json::json!({ "text": response}),
                };
                match send_message(&user, &echo).await {
                    Ok(res) => println!("echo sent (id: {:?})", res.data),
                    Err(e) => println!("failed to send echo: {}", e),
                }
            }
        }
        last = new;
    }
}

#[derive(Deserialize)]
struct LoginResponse {
    payload: LoginPayload,
    status: String,
}
#[derive(Clone, Deserialize, Debug)]
struct LoginPayload {
    token: String,
    username: String,
}

#[derive(Debug, serde::Deserialize)]
enum LoginInfo {
    Loggedin { info: LoginPayload },
    NotLoggedin,
}

#[derive(Debug, serde::Deserialize, Clone)]
pub struct Message {
    #[serde(default)]
    pub message_id: Uuid,
    pub sender_name: String,
    pub parent: Option<Uuid>,
    pub content: SendibleContent,
    #[serde(default)]
    pub sent_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, serde::Deserialize)]
pub struct MessageResponse {
    pub payload: Vec<Message>,
    pub status: String,
}
#[derive(Debug, serde::Deserialize, Clone)]
struct TextMessage {
    text: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
#[derive(Clone)]
pub enum SendibleContent {
    Img(ImgMessage),
    Text(TextMessage),
    Title(TitleMessage),
}

#[derive(serde::Deserialize)]
struct Img {
    url: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SendMessage {
    pub sender_name: String,
    pub parent_id: Option<Uuid>,
    pub content: serde_json::Value,
}
#[derive(Debug, serde::Deserialize, Clone)]
struct ImgMessage {
    url: String,
}
#[derive(Debug, serde::Deserialize, Clone)]
struct TitleMessage {
    title: String,
}

const BASE_URL: &str = "https://bens-chat.team-stingray.com";

// const BASE_URL: &str = "http://localhost:8081";

async fn get_message(login: &LoginPayload, chat_id: &Uuid) -> Result<Message, reqwest::Error> {
    let url = format!("{BASE_URL}/messages?parent={}", chat_id);

    let client = reqwest::Client::new();

    let res = client
        .get(url)
        .bearer_auth(login.token.clone())
        .send()
        .await?;
    let text = res.text().await?;
    let message_response: MessageResponse = serde_json::from_str(&text)
        .map_err(|e| {
            println!("JSON parsing error in get_messages: {}", e);
            panic!("Failed to parse messages JSON");
        })
        .unwrap();

    let _status = message_response.status;
    let messages = message_response.payload;

    let end_msg = messages.last().unwrap().clone();

    Ok(end_msg)
}

async fn get_jwt() -> LoginPayload {
    let url = format!("{BASE_URL}/auth/login");
    let payload = serde_json::json!({
        "username": "test0",
        "password": "aaa",
    });

    let client = reqwest::Client::new();

    let res = match client.post(url).json(&payload).send().await {
        Ok(res) => res,
        Err(e) => {
            println!("Network or request error: {e}. Please try again.");
            panic!();
        }
    };

    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        println!(
            "Login failed (status {}): {}. Please try again.",
            status, body
        );
        panic!();
    }

    let text = match res.text().await {
        Ok(text) => text,
        Err(e) => {
            println!("Failed to read response: {e}. Please try again.");
            panic!();
        }
    };

    let data: LoginResponse = match serde_json::from_str(&text) {
        Ok(data) => data,
        Err(e) => {
            println!("Could not parse login response ({e}). Please try again.");
            panic!();
        }
    };

    data.payload

    // return Ok(user_info);
}

#[derive(Deserialize, Debug)]
struct PostMsgData {
    message_id: Uuid,
}

#[derive(Deserialize, Debug)]
struct PostMsgRes {
    data: PostMsgData,
}

async fn send_message(
    login: &LoginPayload,
    message: &SendMessage,
) -> Result<PostMsgRes, reqwest::Error> {
    let url = format!("{BASE_URL}/messages");
    let client = reqwest::Client::new();

    let response = client
        .post(url)
        .json(message)
        .bearer_auth(login.token.clone())
        .send()
        .await?;

    // Check if the request itself was successful (e.g., 200 OK)
    // If you expect specific HTTP error codes for certain backend errors, you can check them here.
    response.error_for_status_ref()?;

    let parsed_res = response.json::<PostMsgRes>().await?;

    Ok(parsed_res)
}

#[derive(Deserialize)]
struct ChatResponce {
    payload: Vec<Message>,
    status: String,
}

async fn get_chats(user_info: &LoginPayload) -> Result<ChatResponce, reqwest::Error> {
    let url = format!("{BASE_URL}/user-chats?username={}", user_info.username);

    let client = reqwest::Client::new();
    let res = client.get(url).bearer_auth(&user_info.token).send().await?;
    let text = res.text().await?;
    let chats: ChatResponce = serde_json::from_str(&text).unwrap_or_else(|e| {
        print!("error: {e}");
        panic!();
    });

    Ok(chats)
}

async fn call_ai(question: &str, agent: &rig::Agent) -> Result<String, anyhow::Error> {
    let response = agent.prompt(question).max_turns(10).await?;
    Ok(response)
}

async fn setup_mcp_tools() -> Result<Vec<DynamicTool>, anyhow::Error> {
    let registry = ToolRegistry::new();

    registry.register(Arc::new(LocalTool::new(
        ToolSchema {
            name: "run_bash".into(),
            description: "Execute a bash command and return the combined stdout/stderr output."
                .into(),
            args_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The bash command to execute"
                    }
                },
                "required": ["command"]
            }),
            result_schema: serde_json::json!({"type": "string"}),
        },
        |args| async move {
            let command = args
                .get("command")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    KernelError::InvalidArgument("run_bash requires 'command'".into())
                })?;
            let output = run_bash(command)
                .await
                .map_err(|e| KernelError::ToolFailed(e.to_string()))?;
            Ok(serde_json::json!(output))
        },
    )));

    registry.register(Arc::new(LocalTool::new(
        ToolSchema {
            name: "read_file".into(),
            description: "Read and return the contents of a file at the given path.".into(),
            args_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The file path to read"
                    }
                },
                "required": ["path"]
            }),
            result_schema: serde_json::json!({"type": "string"}),
        },
        |args| async move {
            let path = args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| KernelError::InvalidArgument("read_file requires 'path'".into()))?;
            let content = read_file(path)
                .await
                .map_err(|e| KernelError::ToolFailed(e.to_string()))?;
            Ok(serde_json::json!(content))
        },
    )));

    registry.register(Arc::new(LocalTool::new(
        ToolSchema {
            name: "apply_diff".into(),
            description: "Apply a unified diff to a file at the given path.".into(),
            args_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The file path to modify"
                    },
                    "diff": {
                        "type": "string",
                        "description": "The unified diff to apply"
                    }
                },
                "required": ["path", "diff"]
            }),
            result_schema: serde_json::Value::Null,
        },
        |args| async move {
            let path = args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| KernelError::InvalidArgument("apply_diff requires 'path'".into()))?;
            let diff = args
                .get("diff")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| KernelError::InvalidArgument("apply_diff requires 'diff'".into()))?;
            apply_diff(path, diff)
                .await
                .map_err(|e| KernelError::ToolFailed(e.to_string()))?;
            Ok(serde_json::Value::Null)
        },
    )));

    registry.register(Arc::new(LocalTool::new(
        ToolSchema {
            name: "list_dir".into(),
            description: "List the contents of a directory at the given path.".into(),
            args_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The directory path to list"
                    }
                },
                "required": ["path"]
            }),
            result_schema: serde_json::json!({"type": "string"}),
        },
        |args| async move {
            let path = args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| KernelError::InvalidArgument("list_dir requires 'path'".into()))?;
            let entries = list_dir(path)
                .await
                .map_err(|e| KernelError::ToolFailed(e.to_string()))?;
            Ok(serde_json::json!(entries))
        },
    )));

    let transport: Arc<dyn McpTransport> =
        Arc::new(LoopbackTransport::new("loopback://local-tools", registry));

    let mcp_tools = McpTool::from_transport(transport).await?;

    let mut tools = Vec::new();
    for mcp_tool in mcp_tools {
        let schema = mcp_tool.schema();
        tools.push(DynamicTool::new(
            schema.name,
            schema.description,
            schema.args_schema,
            move |_ctx: &mut ToolContext, args: serde_json::Value| {
                let tool = mcp_tool.clone();
                Box::pin(async move {
                    match tool.invoke(args).await {
                        Ok(value) => Ok(ToolOutput::json(value)),
                        Err(e) => Err(ToolExecutionError::provider(format!("{}", e))),
                    }
                })
            },
        ));
    }
    Ok(tools)
}

async fn run_bash(command: &str) -> Result<String, anyhow::Error> {
    use std::process::Command;

    let output = Command::new("bash")
        .arg("-c")
        .arg(command)
        .output()
        .context(format!("Failed to execute bash command: {}", command))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let result = if !stderr.is_empty() {
        format!("STDOUT:\n{}\nSTDERR:\n{}", stdout, stderr)
    } else {
        stdout
    };

    Ok(result)
}

async fn read_file(path: &str) -> Result<String, anyhow::Error> {
    let content = tokio::fs::read_to_string(path)
        .await
        .context(format!("Failed to read file: {}", path))?;
    Ok(content)
}

async fn apply_diff(path: &str, diff: &str) -> Result<(), anyhow::Error> {
    eprintln!("Applying diff to path: {}", path);
    eprintln!("Received diff content:\n{}", diff);

    let original_content = tokio::fs::read_to_string(path)
        .await
        .context(format!("Failed to read file for diff: {}", path))?;

    let patch = Patch::from_str(diff).context("Failed to parse diff string")?;

    let patched_content = diffy_apply(&original_content, &patch).context("Failed to apply diff")?;

    eprintln!("Patched content generated:\n{}", patched_content);

    tokio::fs::write(path, patched_content)
        .await
        .context(format!("Failed to write patched file: {}", path))?;
    Ok(())
}

async fn list_dir(path: &str) -> Result<String, anyhow::Error> {
    let mut entries = tokio::fs::read_dir(path)
        .await
        .context(format!("Failed to read directory: {}", path))?;

    let mut file_list = String::new();
    while let Some(entry) = entries.next_entry().await? {
        let file_name = entry.file_name();
        file_list.push_str(&format!("{}\n", file_name.to_string_lossy()));
    }
    Ok(file_list)
}
