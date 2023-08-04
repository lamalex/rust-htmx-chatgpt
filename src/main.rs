use askama::Template;

use axum::{
    async_trait,
    extract::{FromRequestParts, Path, Query, State},
    handler::HandlerWithoutStateExt,
    http::{request::Parts, StatusCode},
    response::sse::Event,
    response::{IntoResponse, Response},
    routing::{get, post},
    Form, RequestPartsExt, Router,
};
use axum_macros::debug_handler;
use futures_util::{stream, Stream, StreamExt, TryStreamExt};
use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use std::{
    borrow::Cow,
    collections::HashMap,
    fs,
    net::SocketAddr,
    path::{self, PathBuf},
    pin::Pin,
    time::Duration,
};
use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
};
use strum::Display;
use tokio::{sync::broadcast, time::sleep};
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rossgpt=debug,tower_http=debug,reqwest=trace".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let assets_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");
    let static_files_service = ServeDir::new(assets_dir)
        .append_index_html_on_directories(true)
        .not_found_service(handler_404.into_service());

    let state = AppState::new();

    let app = Router::new()
        .fallback_service(static_files_service)
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/api/:version/new_chat", post(new_chat))
        .route("/api/:version/load_chat", get(load_chat))
        .route("/api/:version/send_message", post(send_message))
        .route("/api/:version/get_response", get(get_response))
        .route(
            "/api/:version/message_events/:message_id",
            get(message_events),
        )
        .route("/api/:version/conversations", get(get_conversations))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // run it
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::debug!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn index() -> impl IntoResponse {
    IndexTemplate {}
}

async fn new_chat() -> impl IntoResponse {
    if !path::Path::new("chats").exists() {
        fs::create_dir("chats").expect("Failed to create chats/ dir");
    }

    let chat: Chat<Message> = Chat {
        name: "Unnamed chat".to_string(),
        id: ChatId(uuid::Uuid::new_v4()),
        messages: vec![],
    };

    let chat_json = serde_json::to_string(&chat).expect("Failed to serialize chat to JSON");
    fs::write(format!("chats/{}.json", chat.id), chat_json).expect("Failed to write chat to file");

    ButtonTemplate {
        button_label: chat.name,
        chat_id: chat.id,
    }
}

#[derive(Debug, serde::Deserialize)]
struct LoadChatQuery {
    chat_id: ChatId,
}

async fn load_chat(Query(params): Query<LoadChatQuery>) -> impl IntoResponse {
    let chat_json = fs::read_to_string(format!("chats/{}.json", params.chat_id))
        .expect("Failed to read chat from file");
    let chat = serde_json::from_str::<Chat<Message>>(&chat_json)
        .expect("Failed to deserialize chat from JSON");

    ChatTemplate { chat: chat.into() }
}

async fn send_message(Form(form): Form<SendMessageFormBody>) -> impl IntoResponse {
    let chat_json = fs::read_to_string(format!("chats/{}.json", form.chat_id))
        .expect("Failed to read chat from file");
    let mut chat = serde_json::from_str::<Chat<Message>>(&chat_json)
        .expect("Failed to deserialize chat from JSON");

    let new_message = Message {
        content: form.message.into(),
        role: Role::User,
    };

    chat.messages.push(new_message.clone());

    let chat_json = serde_json::to_string(&chat).expect("Failed to serialize chat to JSON");
    fs::write(format!("chats/{}.json", chat.id), chat_json).expect("Failed to write chat to file");

    MessageTemplate {
        message: Triggerable::Loaded((chat.id, MessageTrigger::User(new_message))),
    }
}
type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

async fn get_response(
    State(state): State<AppState>,
    Query(params): Query<LoadChatQuery>,
) -> impl IntoResponse {
    let chat_json = fs::read_to_string(format!("chats/{}.json", params.chat_id))
        .expect("Failed to read chat from file");
    let mut chat = serde_json::from_str::<Chat<Message>>(&chat_json)
        .expect("Failed to deserialize chat from JSON");

    let client = Client::new();
    let message_id = MessageId(Uuid::new_v4());

    let (tx, _rx) = broadcast::channel(1000);
    let tx = Arc::new(Mutex::new(tx));

    state
        .senders
        .lock()
        .unwrap()
        .insert(message_id.clone(), tx.clone()); // Store the sender in the state

    let openai_api_key = SecretString::new(env!("OPENAI_API_KEY").to_string());

    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(openai_api_key.expose_secret())
        .json(&serde_json::json!({
            "model": "gpt-3.5-turbo",
            "stream": true,
            "messages": [
                {"role": "system", "content": "You are a helpful business analyst."},
                {"role": "user", "content": chat.messages.last().unwrap().content}
            ]
        }))
        .send()
        .await;

    if let Err(e) = resp {
        return MessageTemplate {
            message: Triggerable::Unloaded(Message {
                content: format!("Failed to send message to OpenAI: {}", e).into(),
                role: Role::Assistant,
            }),
        };
    }

    let resp = resp.unwrap();

    let new_message = Message {
        content: "".into(),
        role: Role::Assistant,
    };

    chat.messages.push(new_message.clone());

    let chat_json = serde_json::to_string(&chat).expect("Failed to serialize chat to JSON");
    fs::write(format!("chats/{}.json", chat.id), chat_json).expect("Failed to write chat to file");

    // The client must support text/event-stream content type for SSE
    let mut stream = resp.bytes_stream().map_err(Error::from);
    // Spawn a new task to listen for the server-sent events
    tokio::spawn(async move {
        sleep(Duration::from_secs(1)).await;
        while let Some(item) = stream.next().await {
            sleep(Duration::from_millis(69)).await;
            match item {
                Ok(data) => {
                    // Broadcast the data received from the server to the clients
                    let _ = tx
                        .lock()
                        .unwrap()
                        .send(String::from_utf8_lossy(&data).to_string());
                }
                Err(e) => {
                    eprintln!("error occurred while reading stream: {}", e);
                }
            }
        }
    });

    MessageTemplate {
        message: Triggerable::Loaded((
            params.chat_id,
            MessageTrigger::Assistant((message_id, new_message)),
        )),
    }
}

#[derive(Debug, serde::Deserialize)]
struct ChatGptResponse {
    choices: Vec<ChatGptChoice>,
}

#[derive(Debug, serde::Deserialize)]
struct ChatGptChoice {
    delta: Option<ChatGptDelta>,
}

#[derive(Debug, serde::Deserialize)]
enum ChatGptFinishReason {
    Stop,
}

#[derive(Debug, serde::Deserialize)]
struct ChatGptDelta {
    content: Option<String>,
}

#[debug_handler]
async fn message_events(
    State(state): State<AppState>,
    _version: Version,
    message_id: MessageId,
) -> impl IntoResponse {
    let senders = state.senders.lock().unwrap();

    let stream: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        match senders.get(&message_id) {
            Some(tx) => match tx.lock() {
                Ok(tx) => {
                    let rx = tx.subscribe();
                    let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
                        .filter_map(|res| async move { res.ok() })
                        .map(|message| {
                            dbg!(&message["data:".len()..]);
                            let gptresponse =
                                serde_json::from_str::<ChatGptResponse>(&message["data:".len()..])
                                    .map(|message| {
                                        let choices = message.choices;
                                        let first_choice = choices.first().unwrap();
                                        let delta = first_choice.delta.as_ref().unwrap();
                                        let content = delta.content.as_ref().unwrap();
                                        content.clone()
                                    });
                            dbg!(&gptresponse);
                            gptresponse
                        })
                        .filter_map(|content| async move { content.ok() })
                        .map(|content| Event::default().data(format!("<span>{content}</span>")))
                        .map(Ok)
                        .inspect(|event| {
                            eprintln!("Sending event: {:?}", event);
                        });
                    Box::pin(stream)
                }
                Err(e) => {
                    eprintln!("Failed to lock sender: {}", e);
                    Box::pin(stream::empty())
                }
            },
            None => {
                eprintln!("No sender found for message_id: {:?}", message_id);
                Box::pin(stream::empty())
            }
        };

    axum::response::Sse::new(stream)
}

async fn get_conversations() -> impl IntoResponse {
    let mut conversations = vec![];

    for entry in fs::read_dir("chats").expect("Failed to read chats/ dir") {
        let entry = entry.expect("Failed to read entry");
        let chat_json = fs::read_to_string(entry.path()).expect("Failed to read chat from file");
        let chat = serde_json::from_str::<Chat<Message>>(&chat_json)
            .expect("Failed to deserialize chat from JSON");

        conversations.push((chat.name, chat.id));
    }

    ChatListTemplate { conversations }
}

async fn handler_404() -> impl IntoResponse {
    (StatusCode::OK, NotFoundTemplate)
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
enum Triggerable<L, U> {
    Loaded(L),
    Unloaded(U),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
enum MessageTrigger<'a> {
    User(Message<'a>),
    Assistant((MessageId, Message<'a>)),
}

#[derive(Debug, serde::Deserialize)]
struct SendMessageFormBody {
    chat_id: ChatId,
    message: String,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {}

#[derive(Template)]
#[template(path = "chat.html")]
struct ChatTemplate<'a> {
    chat: Chat<Triggerable<(ChatId, MessageTrigger<'a>), Message<'a>>>,
}

#[derive(Template)]
#[template(path = "chat_list.html")]
struct ChatListTemplate {
    conversations: Vec<(String, ChatId)>,
}

#[derive(Template)]
#[template(path = "message.html")]
struct MessageTemplate<'a> {
    message: Triggerable<(ChatId, MessageTrigger<'a>), Message<'a>>,
}

#[derive(Template)]
#[template(path = "chat_button.html")]
struct ButtonTemplate {
    button_label: String,
    chat_id: ChatId,
}

#[derive(Template)]
#[template(path = "404.html")]
struct NotFoundTemplate;

#[derive(Debug)]
enum Version {
    V1,
}

#[async_trait]
impl<S> FromRequestParts<S> for Version
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let params: Path<HashMap<String, String>> =
            parts.extract().await.map_err(IntoResponse::into_response)?;

        let version = params
            .get("version")
            .ok_or_else(|| (StatusCode::NOT_FOUND, "version param missing").into_response())?;

        match version.as_str() {
            "v1" => Ok(Version::V1),
            _ => Err((StatusCode::NOT_FOUND, "unknown version").into_response()),
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct ChatId(pub Uuid);

impl std::fmt::Display for ChatId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Debug, serde::Deserialize, serde::Serialize)]
struct MessageId(pub Uuid);

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for MessageId {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let params: Path<HashMap<String, String>> =
            parts.extract().await.map_err(IntoResponse::into_response)?;

        let message_id = params
            .get("message_id")
            .ok_or_else(|| (StatusCode::NOT_FOUND, "message_id param missing").into_response())?;

        let message_id = message_id
            .parse::<Uuid>()
            .map_err(|_| (StatusCode::NOT_FOUND, "invalid message_id").into_response())?;

        Ok(MessageId(message_id))
    }
}

#[derive(Clone)]
struct AppState {
    pub senders: Arc<Mutex<HashMap<MessageId, Arc<Mutex<broadcast::Sender<String>>>>>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            senders: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Chat<T> {
    name: String,
    id: ChatId,
    messages: Vec<T>,
}

impl<'a> From<Chat<Message<'a>>> for Chat<Triggerable<(ChatId, MessageTrigger<'a>), Message<'a>>> {
    fn from(chat: Chat<Message<'a>>) -> Self {
        Self {
            name: chat.name,
            id: chat.id,
            messages: chat
                .messages
                .into_iter()
                .map(Triggerable::Unloaded)
                .collect(),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Message<'a> {
    content: Cow<'a, str>,
    role: Role,
}

#[derive(Display, Clone, Debug, serde::Serialize, serde::Deserialize)]
enum Role {
    System,
    Assistant,
    User,
}
