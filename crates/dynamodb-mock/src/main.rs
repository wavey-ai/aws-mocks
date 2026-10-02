//! `dynamodb-mock`: serve the in-memory DynamoDB emulator on a loopback port.

use anyhow::{Context, Result};
use clap::Parser;
use std::net::SocketAddr;
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(about = "Loopback-only in-memory DynamoDB emulator (JSON 1.0 protocol)")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8003")]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    anyhow::ensure!(
        args.listen.ip().is_loopback(),
        "DynamoDB mock must listen on loopback"
    );
    let listener = TcpListener::bind(args.listen)
        .await
        .context("bind DynamoDB listener")?;
    let address = listener.local_addr().context("read listener address")?;
    eprintln!("DynamoDB mock ready on {address}");
    dynamodb_mock::serve(listener).await
}
