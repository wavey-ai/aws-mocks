//! `transcribe-mock`: serve the loopback speech fixture that stands in for AWS Transcribe.

use anyhow::{Context, Result};
use clap::Parser;
use std::net::{Ipv4Addr, SocketAddr};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(about = "Loopback-only deterministic speech fixture standing in for AWS Transcribe")]
struct Args {
    /// Listen address. Defaults to 127.0.0.1 on `TRANSCRIBE_PROXY_PORT`, or 8005 when that
    /// variable is unset.
    #[arg(long)]
    listen: Option<SocketAddr>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("transcribe_mock=info,warn")),
        )
        .init();
    let args = Args::parse();
    let listen = match args.listen {
        Some(listen) => listen,
        None => {
            let port = match std::env::var("TRANSCRIBE_PROXY_PORT") {
                Ok(port) => port.trim().parse().context("parse TRANSCRIBE_PROXY_PORT")?,
                Err(_) => transcribe_mock::DEFAULT_PORT,
            };
            SocketAddr::from((Ipv4Addr::LOCALHOST, port))
        }
    };
    anyhow::ensure!(
        listen.ip().is_loopback(),
        "the transcribe fixture must listen on loopback"
    );
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind the transcribe fixture on {listen}"))?;
    let address = listener.local_addr()?;
    // Scripts wait for this line.
    println!("Local Transcribe proxy ready: http://{address}");
    transcribe_mock::serve(listener).await?;
    Ok(())
}
