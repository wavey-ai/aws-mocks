//! `sqs-mock`: serve the loopback SQS fixture.

use anyhow::{Context, Result};
use clap::Parser;
use sqs_mock::DEFAULT_QUEUE;
use std::{net::SocketAddr, path::PathBuf};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(about = "Loopback-only Amazon SQS fixture")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8010")]
    listen: SocketAddr,
    /// Where queue state (`state.json`) persists.
    #[arg(long, default_value = ".local/sqs")]
    directory: PathBuf,
    /// A queue name to serve; repeat for several. The first one also receives messages from
    /// a single-queue `state.json`.
    #[arg(long = "queue", default_value = DEFAULT_QUEUE)]
    queues: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    anyhow::ensure!(
        args.listen.ip().is_loopback(),
        "SQS fixture must listen on loopback"
    );
    let listener = TcpListener::bind(args.listen)
        .await
        .context("bind queue listener")?;
    eprintln!("Rust SQS fixture ready on {}", args.listen);
    sqs_mock::serve(listener, &args.directory, args.queues).await
}
