//! Loopback Amazon SQS fixture serving a fixed set of named standard queues.
//!
//! It speaks the AWS JSON protocol (`X-Amz-Target: AmazonSQS.*`) for `GetQueueUrl`,
//! `SendMessage`, `ReceiveMessage` (with long polling), `ChangeMessageVisibility` and
//! `DeleteMessage`, plus `CreateQueue` (of a configured queue), `GetQueueAttributes` and
//! `PurgeQueue` for inspection. Messages persist in `state.json` under the state directory.
//!
//! The `sqs-mock` binary serves it on a fixed port; tests embed it with [`start`] on an
//! ephemeral port.

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, body::Incoming, header};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    convert::Infallible,
    fs,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpListener, sync::Notify, task::JoinHandle, time::sleep};
use uuid::Uuid;

/// The queue served when the caller names none.
pub const DEFAULT_QUEUE: &str = "local-queue";
/// The account id in every queue URL.
pub const ACCOUNT_ID: &str = "000000000000";

#[derive(Clone, Serialize, Deserialize)]
struct Message {
    id: String,
    body: String,
    sent_ms: u64,
    visible_ms: u64,
    receives: u32,
    receipt: Option<String>,
}

/// Every queue's messages, keyed by queue name.
#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    queues: BTreeMap<String, VecDeque<Message>>,
    /// A single-queue state file's messages, read into the first queue.
    #[serde(default, skip_serializing)]
    messages: VecDeque<Message>,
}

struct App {
    state: Mutex<State>,
    file: PathBuf,
    /// Queue URL to queue name; the first configured queue's URL is also kept first.
    queues: Vec<(String, String)>,
    notify: Notify,
}

type Reply = Response<Full<Bytes>>;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn reply(status: StatusCode, value: Value) -> Reply {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/x-amz-json-1.0")
        .body(Full::new(Bytes::from(serde_json::to_vec(&value).unwrap())))
        .unwrap()
}

fn error(code: &str, message: &str) -> Reply {
    reply(
        StatusCode::BAD_REQUEST,
        json!({"__type": format!("com.amazonaws.sqs#{code}"), "message": message}),
    )
}

fn required<'a>(body: &'a Value, name: &str) -> Result<&'a str, Box<Reply>> {
    body.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Box::new(error("InvalidParameterValue", &format!("Missing {name}"))))
}

fn seconds(body: &Value, name: &str, default: u64, max: u64) -> Result<u64, Box<Reply>> {
    match body.get(name) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|number| *number <= max)
            .ok_or_else(|| Box::new(error("InvalidParameterValue", &format!("Invalid {name}")))),
    }
}

impl App {
    fn persist(&self, state: &State) -> Result<()> {
        let temp = self.file.with_extension("json.tmp");
        fs::write(&temp, serde_json::to_vec(state)?)?;
        fs::rename(temp, &self.file)?;
        Ok(())
    }

    /// The name of the queue `body`'s `QueueUrl` names.
    fn queue(&self, body: &Value) -> Result<String, Box<Reply>> {
        let url = required(body, "QueueUrl")?;
        self.queues
            .iter()
            .find(|(known, _)| known == url)
            .map(|(_, name)| name.clone())
            .ok_or_else(|| {
                Box::new(error(
                    "AWS.SimpleQueueService.NonExistentQueue",
                    "Unknown queue",
                ))
            })
    }

    fn url_of(&self, name: &str) -> Option<&str> {
        self.queues
            .iter()
            .find(|(_, known)| known == name)
            .map(|(url, _)| url.as_str())
    }

    fn send(&self, body: &Value) -> Result<Value, Box<Reply>> {
        let queue = self.queue(body)?;
        let content = required(body, "MessageBody")?;
        let delay = seconds(body, "DelaySeconds", 0, 900)?;
        let id = Uuid::now_v7().to_string();
        let now = now_ms();
        let mut state = self.state.lock().unwrap();
        state.queues.entry(queue).or_default().push_back(Message {
            id: id.clone(),
            body: content.to_owned(),
            sent_ms: now,
            visible_ms: now + delay * 1000,
            receives: 0,
            receipt: None,
        });
        self.persist(&state)
            .map_err(|_| Box::new(error("InternalError", "Could not persist queue")))?;
        self.notify.notify_waiters();
        Ok(json!({"MessageId": id}))
    }

    async fn receive(&self, body: &Value) -> Result<Value, Box<Reply>> {
        let queue = self.queue(body)?;
        let wait = seconds(body, "WaitTimeSeconds", 0, 20)?;
        let visibility = seconds(body, "VisibilityTimeout", 30, 43200)?;
        let limit = seconds(body, "MaxNumberOfMessages", 1, 10)?;
        if limit == 0 {
            return Err(Box::new(error(
                "InvalidParameterValue",
                "Invalid MaxNumberOfMessages",
            )));
        }
        let deadline = Instant::now() + Duration::from_secs(wait);
        loop {
            {
                let mut state = self.state.lock().unwrap();
                let now = now_ms();
                let mut delivered = Vec::new();
                for message in state
                    .queues
                    .entry(queue.clone())
                    .or_default()
                    .iter_mut()
                    .filter(|item| item.visible_ms <= now)
                    .take(limit as usize)
                {
                    let receipt = Uuid::now_v7().to_string();
                    message.receipt = Some(receipt.clone());
                    message.receives += 1;
                    message.visible_ms = now + visibility * 1000;
                    delivered.push(json!({
                        "MessageId": message.id,
                        "ReceiptHandle": receipt,
                        "Body": message.body,
                        "Attributes": {
                            "ApproximateReceiveCount": message.receives.to_string(),
                            "SentTimestamp": message.sent_ms.to_string(),
                        },
                    }));
                }
                if !delivered.is_empty() {
                    self.persist(&state)
                        .map_err(|_| Box::new(error("InternalError", "Could not persist queue")))?;
                    return Ok(json!({"Messages": delivered}));
                }
            }
            if Instant::now() >= deadline {
                return Ok(json!({}));
            }
            tokio::select! {
                _ = self.notify.notified() => {},
                _ = sleep(Duration::from_millis(100)) => {},
            }
        }
    }

    fn delete(&self, body: &Value) -> Result<Value, Box<Reply>> {
        let queue = self.queue(body)?;
        let receipt = required(body, "ReceiptHandle")?;
        let mut state = self.state.lock().unwrap();
        let messages = state.queues.entry(queue).or_default();
        let Some(index) = messages
            .iter()
            .position(|item| item.receipt.as_deref() == Some(receipt))
        else {
            return Err(Box::new(error(
                "ReceiptHandleIsInvalid",
                "Unknown receipt handle",
            )));
        };
        messages.remove(index);
        self.persist(&state)
            .map_err(|_| Box::new(error("InternalError", "Could not persist queue")))?;
        Ok(json!({}))
    }

    fn visibility(&self, body: &Value) -> Result<Value, Box<Reply>> {
        let queue = self.queue(body)?;
        let receipt = required(body, "ReceiptHandle")?;
        let timeout = seconds(body, "VisibilityTimeout", 30, 43200)?;
        let mut state = self.state.lock().unwrap();
        let Some(message) = state
            .queues
            .entry(queue)
            .or_default()
            .iter_mut()
            .find(|item| item.receipt.as_deref() == Some(receipt))
        else {
            return Err(Box::new(error(
                "ReceiptHandleIsInvalid",
                "Unknown receipt handle",
            )));
        };
        message.visible_ms = now_ms() + timeout * 1000;
        self.persist(&state)
            .map_err(|_| Box::new(error("InternalError", "Could not persist queue")))?;
        if timeout == 0 {
            self.notify.notify_waiters();
        }
        Ok(json!({}))
    }
}

async fn handle(app: Arc<App>, request: Request<Incoming>) -> Reply {
    if request.method() == Method::GET && request.uri().path() == "/health" {
        return reply(StatusCode::OK, json!({"fixture": "rust-sqs"}));
    }
    if request.method() != Method::POST {
        return error("InvalidAction", "SQS accepts POST requests");
    }
    let action = request
        .headers()
        .get("x-amz-target")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("AmazonSQS."))
        .unwrap_or("")
        .to_owned();
    let data = match request.into_body().collect().await {
        Ok(body) => body.to_bytes(),
        Err(_) => return error("InvalidParameterValue", "Could not read request"),
    };
    let body: Value = match serde_json::from_slice(&data) {
        Ok(body) => body,
        Err(_) => return error("InvalidParameterValue", "Expected JSON request"),
    };
    let result = match action.as_str() {
        "CreateQueue" | "GetQueueUrl" => {
            if let Some(url) = required(&body, "QueueName")
                .ok()
                .and_then(|name| app.url_of(name))
            {
                Ok(json!({"QueueUrl": url}))
            } else {
                Err(Box::new(error(
                    "AWS.SimpleQueueService.NonExistentQueue",
                    "Unknown queue",
                )))
            }
        }
        "SendMessage" => app.send(&body),
        "ReceiveMessage" => app.receive(&body).await,
        "DeleteMessage" => app.delete(&body),
        "ChangeMessageVisibility" => app.visibility(&body),
        "GetQueueAttributes" => match app.queue(&body) {
            Err(error) => Err(error),
            Ok(queue) => {
                let state = app.state.lock().unwrap();
                let visible = state
                    .queues
                    .get(&queue)
                    .into_iter()
                    .flatten()
                    .filter(|item| item.visible_ms <= now_ms())
                    .count();
                Ok(json!({"Attributes": {"ApproximateNumberOfMessages": visible.to_string()}}))
            }
        },
        "PurgeQueue" => match app.queue(&body) {
            Err(error) => Err(error),
            Ok(queue) => {
                let mut state = app.state.lock().unwrap();
                state.queues.entry(queue).or_default().clear();
                if app.persist(&state).is_err() {
                    Err(Box::new(error("InternalError", "Could not persist queue")))
                } else {
                    Ok(json!({}))
                }
            }
        },
        _ => Err(Box::new(error("InvalidAction", "Unsupported SQS action"))),
    };
    match result {
        Ok(value) => reply(StatusCode::OK, value),
        Err(error) => *error,
    }
}

fn names<I, S>(queues: I) -> Result<Vec<String>>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut names: Vec<String> = Vec::new();
    for name in queues {
        let name = name.into();
        anyhow::ensure!(
            !name.is_empty()
                && name.len() <= 80
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "invalid queue name {name:?}"
        );
        if !names.contains(&name) {
            names.push(name);
        }
    }
    anyhow::ensure!(
        !names.is_empty(),
        "the SQS fixture needs at least one queue"
    );
    Ok(names)
}

fn load(directory: &Path, address: SocketAddr, queues: Vec<String>) -> Result<Arc<App>> {
    anyhow::ensure!(
        address.ip().is_loopback(),
        "SQS fixture must listen on loopback"
    );
    fs::create_dir_all(directory).context("create queue directory")?;
    let file = directory.join("state.json");
    let mut state: State = if file.exists() {
        serde_json::from_slice(&fs::read(&file).context("read queue state")?)
            .context("decode queue state")?
    } else {
        State::default()
    };
    let legacy = std::mem::take(&mut state.messages);
    state
        .queues
        .entry(queues[0].clone())
        .or_default()
        .extend(legacy);
    Ok(Arc::new(App {
        state: Mutex::new(state),
        file,
        queues: queues
            .into_iter()
            .map(|name| (queue_url(address, &name), name))
            .collect(),
        notify: Notify::new(),
    }))
}

/// The URL of queue `name` on a fixture listening on `address`.
pub fn queue_url(address: SocketAddr, name: &str) -> String {
    format!("http://{address}/{ACCOUNT_ID}/{name}")
}

async fn accept(listener: TcpListener, app: Arc<App>) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let app = Arc::clone(&app);
                async move { Ok::<_, Infallible>(handle(app, request).await) }
            });
            if let Err(error) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                eprintln!("SQS connection ended: {error}");
            }
        });
    }
}

/// Serve `queues` on a bound loopback listener until the task is dropped.
/// Queue state persists as `state.json` under `directory`.
pub async fn serve<I, S>(listener: TcpListener, directory: &Path, queues: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let address = listener
        .local_addr()
        .context("read queue listener address")?;
    let app = load(directory, address, names(queues)?)?;
    accept(listener, app).await
}

/// A fixture running on an ephemeral loopback port.
pub struct RunningQueue {
    address: SocketAddr,
    queues: Vec<String>,
    task: JoinHandle<Result<()>>,
}

impl RunningQueue {
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The endpoint to configure as `AWS_ENDPOINT_URL_SQS`.
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The first queue's URL.
    pub fn queue_url(&self) -> String {
        queue_url(self.address, &self.queues[0])
    }

    /// Queue `name`'s URL, if the fixture serves it.
    pub fn queue_url_of(&self, name: &str) -> Option<String> {
        self.queues
            .iter()
            .any(|queue| queue == name)
            .then(|| queue_url(self.address, name))
    }

    pub fn stop(self) {
        self.task.abort();
    }
}

/// Start the fixture serving `queues` on `127.0.0.1:0` inside the current Tokio runtime.
pub async fn start<I, S>(directory: &Path, queues: I) -> Result<RunningQueue>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let queues = names(queues)?;
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .context("bind queue listener")?;
    let address = listener
        .local_addr()
        .context("read queue listener address")?;
    // Load up front so a bad state file fails the caller, not the task.
    let app = load(directory, address, queues.clone())?;
    let task = tokio::spawn(accept(listener, app));
    Ok(RunningQueue {
        address,
        queues,
        task,
    })
}
