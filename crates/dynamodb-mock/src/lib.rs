//! In-memory DynamoDB emulator speaking the JSON 1.0 wire protocol
//! (`X-Amz-Target: DynamoDB_20120810.<Operation>`).
//!
//! The `dynamodb-mock` binary serves it on a fixed loopback port; tests embed it
//! with [`start`] on an ephemeral port. State lives in memory only. Requests are
//! accepted with any (or no) SigV4 signature.

mod engine;
mod expr;
mod num;
mod value;

pub use engine::{Database, DdbError};

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, Response, StatusCode, body::Incoming, header, service::service_fn};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
};
use tokio::{net::TcpListener, task::JoinHandle};

const TARGET_PREFIX: &str = "DynamoDB_20120810.";
const DEFAULT_REGION: &str = "us-east-1";

type Reply = Response<Full<Bytes>>;

fn reply(status: StatusCode, value: &Value) -> Reply {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/x-amz-json-1.0")
        .header("x-amzn-RequestId", uuid::Uuid::new_v4().to_string())
        .body(Full::new(Bytes::from(serde_json::to_vec(value).unwrap())))
        .unwrap()
}

fn error_reply(error: &DdbError) -> Reply {
    reply(StatusCode::BAD_REQUEST, &error.body())
}

/// The region from a SigV4 credential scope, so ARNs match the caller's region.
fn region(request: &Request<Incoming>) -> String {
    request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|auth| auth.split("Credential=").nth(1))
        .and_then(|credential| credential.split('/').nth(2))
        .filter(|region| !region.is_empty())
        .unwrap_or(DEFAULT_REGION)
        .to_owned()
}

async fn handle(db: Arc<Mutex<Database>>, request: Request<Incoming>) -> Reply {
    if request.method() == Method::GET {
        return reply(
            StatusCode::OK,
            &json!({"service": "dynamodb-mock", "status": "ok"}),
        );
    }
    if request.method() != Method::POST {
        return error_reply(&DdbError::new(
            "UnknownOperationException",
            "DynamoDB accepts POST requests",
        ));
    }
    let operation = request
        .headers()
        .get("x-amz-target")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix(TARGET_PREFIX))
        .unwrap_or("")
        .to_owned();
    let region = region(&request);
    let data = match request.into_body().collect().await {
        Ok(body) => body.to_bytes(),
        Err(_) => {
            return error_reply(&DdbError::new(
                "SerializationException",
                "Could not read request",
            ));
        }
    };
    let body: Value = if data.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&data) {
            Ok(body @ Value::Object(_)) => body,
            _ => {
                return error_reply(&DdbError::new(
                    "SerializationException",
                    "Start of structure or map found where not expected",
                ));
            }
        }
    };
    let result = db.lock().unwrap().handle(&operation, &body, &region);
    match result {
        Ok(value) => reply(StatusCode::OK, &value),
        Err(error) => error_reply(&error),
    }
}

/// Serve on a bound loopback listener until the task is dropped.
pub async fn serve(listener: TcpListener) -> Result<()> {
    let address = listener.local_addr().context("read listener address")?;
    anyhow::ensure!(
        address.ip().is_loopback(),
        "DynamoDB mock must listen on loopback"
    );
    let db = Arc::new(Mutex::new(Database::new()));
    loop {
        let (stream, _) = listener.accept().await?;
        let db = Arc::clone(&db);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let db = Arc::clone(&db);
                async move { Ok::<_, Infallible>(handle(db, request).await) }
            });
            if let Err(error) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                eprintln!("DynamoDB mock connection ended: {error}");
            }
        });
    }
}

/// An emulator running on an ephemeral loopback port.
pub struct Running {
    address: SocketAddr,
    task: JoinHandle<Result<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Running {
    /// The endpoint to configure as `AWS_ENDPOINT_URL_DYNAMODB`.
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn stop(self) {
        self.task.abort();
    }
}

/// Start the emulator on `127.0.0.1:0` inside the current Tokio runtime.
pub async fn start() -> Result<Running> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .context("bind listener")?;
    let address = listener.local_addr().context("read listener address")?;
    let task = tokio::spawn(serve(listener));
    Ok(Running { address, task })
}
