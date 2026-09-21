use anyhow::Context;
use serde::Deserialize;
use uuid::Uuid;

use dotenv::dotenv;
use rig::memory::InMemoryConversationMemory;
use rig::prelude::*;
use rig_core::providers::openai;
use std::{env, result::Result};
use diffy::{apply as diffy_apply, Patch};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let user = get_jwt().await;
    // let id = uuid!("b4fbbad7-a13c-4dc2-b1f3-9776f6f47e2d");
    // get chats
    let chats = get_chats(&user).await.unwrap();
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
    let id = match id {
        Some(id) => id,
        _ => panic!(),
    };

    dotenv().ok();
    let api_key_name = "AI_ENG";
    let api_key: String = match env::var(api_key_name) {
        Ok(val) => val.trim().to_string(),
        Err(e) => {
            println!("couldn't interpret {api_key_name}: {e}");
            format!("{}", e)
        }
    };
    let client = openai::Client::new(api_key)?;
    let memory = InMemoryConversationMemory::new();
    let mut agent = client
        .agent("gpt-3.5-turbo")
        .preamble("You are a helpful assistant.")
        .memory(memory)
        .build();

    let mut last = get_message(&user, &id).await.unwrap();

    loop {
        let new = get_message(&user, &id).await.unwrap();

        let new_text = match &new.content {
            SendibleContent::Text(t) => Some(t.text.clone()),
            _ => None,
        };
        let last_text = match &last.content {
            SendibleContent::Text(t) => Some(t.text.clone()),
            _ => None,
        };

        if new_text != last_text {
            println!("received: {}", new_text.as_deref().unwrap_or("(non-text)"));

            let ai_response = call_ai(&new_text.clone().unwrap(), &mut agent).await.unwrap();

            if new.sender_name != user.username
                && let Some(_text) = &new_text
            {
                let echo = SendMessage {
                    sender_name: user.username.clone(),
                    parent_id: Some(id),
                    content: serde_json::json!({ "text": ai_response}),
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
    pub message_id: uuid::Uuid,
    pub sender_name: String,
    pub parent: Option<uuid::Uuid>,
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
    pub parent_id: Option<uuid::Uuid>,
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
    message_id: uuid::Uuid,
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
    let chats: ChatResponce = serde_json::from_str(&text)
        .map_err(|e| {
            println!("JSON parsing error in get_chats: {}", e);
            panic!("Failed to parse chats JSON");
        })
        .unwrap();

    Ok(chats)
}

async fn call_ai(
    question: &str,
    agent: &mut rig::Agent,
) -> Result<String, anyhow::Error> {
    enum AgentState {
        Think,
        Act,
        Observe,
        Done,
    }

    let mut state = AgentState::Think;
    let mut thought = String::new();
    let mut observation = String::new();
    let mut parsed_thought: Option<serde_json::Value> = None;

    loop {
        match state {
            AgentState::Think => {
                // Generate a thought based on the question and previous observations
                thought = agent
                    .prompt(&format!(
                        "You are a coding agent. Your goal is to make changes to code based on user requests.\n                        You have the following tools available:\n                        - `read_file(path: &str)`: Reads the content of a file.\n                        - `apply_diff(path: &str, diff: &str)`: Applies a diff to a file.\n\n                        User request: {}\n                        Previous observation: {}\n\n                        What is your next thought and action? Respond in a JSON format with 'thought' and 'action' fields.\n                        The 'action' field should be a call to one of the available tools, or 'None' if you are done.\n                        Example:\n                        {{\"thought\": \"I need to read the file first.\", \"action\": \"read_file('src/main.rs')\"}}\n                        {{\"thought\": \"I have applied the diff and finished the task.\", \"action\": \"None\"}}",
                        question, observation
                    ))
                    .conversation("coding-agent")
                    .await
                    .context("Failed to get thought from model")?;

                state = AgentState::Act;
            }
            AgentState::Act => {
                // Parse the thought and execute the action
                let current_thought: serde_json::Value = serde_json::from_str(&thought)?;
                let action = current_thought["action"].as_str().unwrap_or("None");
                parsed_thought = Some(current_thought.clone());

                if action == "None" {
                    state = AgentState::Done;
                } else if action.starts_with("read_file") {
                    let path = action
                        .trim_start_matches("read_file('")
                        .trim_end_matches("')");
                    observation = read_file(path).await?;
                    state = AgentState::Observe;
                } else if action.starts_with("apply_diff") {
                    let parts: Vec<&str> = action.split("', '").collect();
                    let path = parts[0].trim_start_matches("apply_diff('");
                    let diff = parts[1].trim_end_matches("')");
                    apply_diff(path, diff).await?;                       
                    observation = format!("Successfully applied diff to {}", path);
                    state = AgentState::Observe;
                } else {
                    observation = format!("Unknown action: {}", action);
                    state = AgentState::Observe;
                }
            }
            AgentState::Observe => {
                // The observation is already set in the Act state
                state = AgentState::Think;
            }
            AgentState::Done => {
                // The task is complete
                return Ok(parsed_thought.unwrap()["thought"].as_str().unwrap_or("Task completed.").to_string());
            }
        }
    }
}


async fn read_file(path: &str) -> Result<String, anyhow::Error> {
    let content = tokio::fs::read_to_string(path)
        .await
        .context(format!("Failed to read file: {}", path))?;
    Ok(content)
}

async fn apply_diff(path: &str, diff: &str) -> Result<(), anyhow::Error> {
    let original_content = tokio::fs::read_to_string(path)
        .await
        .context(format!("Failed to read file for diff: {}", path))?;
    
    let patch = diffy::Patch::from_str(diff)
        .context("Failed to parse diff string")?;

    let patched_content = diffy_apply(&original_content, &patch) 
        .context("Failed to apply diff")?;

    tokio::fs::write(path, patched_content)
        .await
        .context(format!("Failed to write patched file: {}", path))?;
    Ok(())
}
