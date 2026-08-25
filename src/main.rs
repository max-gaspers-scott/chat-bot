
use serde::Deserialize;
use reqwest::Response;
use serde_json::json;
use uuid::{uuid, Uuid};

#[tokio::main]
async fn main() {
    let user = get_jwt().await;
    let id = uuid!("f641c623-1ab7-41fe-b10a-7268b96d1467");

    let mut msg = match get_message(&user, &id).await.unwrap().content {
        SendibleContent::Text(text) => text.text,
        _=> String::from("no messages"),
    };

    loop {
        let new_msg = get_if_text(&msg.clone(), &user, &id).await;

        if new_msg != msg {
            println!("+------------------------+");
            println!("+                        +");
            println!("+    mesage was sent     +");
            println!("+                        +");
            println!("+------------------------+");
        }
        msg = new_msg;

     }
}

async fn get_if_text(msg: &str, user: &LoginPayload, id: &Uuid) -> String {
match get_message(user, id).await.unwrap().content {
        SendibleContent::Text(text) => text.text,
        _=> msg.to_string(),
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

#[derive(Debug, serde::Deserialize)]
 #[derive(Clone)]
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
pub struct MessageResponce {
    pub payload: Vec<Message>,
    pub status: String,
}
#[derive(Debug, serde::Deserialize)]
#[derive(Clone)]
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

#[derive(Debug, serde::Serialize)]
pub struct SendMesage {
    pub sender_name: String,
    pub parent: Option<uuid::Uuid>,
    pub content: serde_json::Value,
}
#[derive(Debug, serde::Deserialize)]
#[derive(Clone)]
struct ImgMessage {
    url: String,
}
#[derive(Debug, serde::Deserialize)]
 #[derive(Clone)]
struct TitleMessage {
    title: String,
}

const BASE_URL: &str = "https://bens-chat.team-stingray.com";


async fn get_message(
    login: &LoginPayload,
    chat_id: &Uuid,
) -> Result<Message, reqwest::Error> {
    let url = format!("{BASE_URL}/messages?parent={}", chat_id);

    let client = reqwest::Client::new();

    let res = client
        .get(url)
        .bearer_auth(login.token.clone())
        .send()
        .await?;
    let text = res.text().await?;
    let message_responce: MessageResponce = serde_json::from_str(&text)
        .map_err(|e| {
            println!("JSON parsing error in get_messages: {}", e);
            panic!("Failed to parse messages JSON");
        })
        .unwrap();

    let messages = message_responce.payload;

    let end_msg = messages.last().unwrap().clone();

    Ok(end_msg)

}


async fn get_jwt() -> LoginPayload {
        let url = format!("{BASE_URL}/auth/login");
        let payload = serde_json::json!({
            "username": "MGS",
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
