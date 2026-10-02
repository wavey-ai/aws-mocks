//! `cognito-mock`: serve the Cognito user pool fixture on a given address.

use anyhow::{Context, Result};
use clap::Parser;
use cognito_mock::{Config, DEFAULT_NAME, DEFAULT_RESET_CODE};
use std::{net::SocketAddr, path::PathBuf};
use tokio::net::TcpListener;
use tracing::info;

#[derive(Parser)]
#[command(about = "Loopback-only Amazon Cognito user pool fixture")]
struct Args {
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long)]
    pool_id: String,
    #[arg(long)]
    client_id: String,
    /// Keep accounts and groups (not keys or sessions) in this file across restarts.
    #[arg(long)]
    state_file: Option<PathBuf>,
    /// Allow a non-loopback listen address (a container network).
    #[arg(long)]
    allow_container_network: bool,
    /// The token issuer; default `http://<listen>/<pool id>`.
    #[arg(long)]
    issuer: Option<String>,
    /// Compatibility mode: `ListUserPools`/`ListUserPoolClients` discovery, `AdminEnableUser`,
    /// email aliases for `Username`, a caller-chosen `sub` and `TemporaryPassword` on
    /// `AdminCreateUser` (accepting `--reset-code`), `sub` in `AdminGetUser`, the
    /// `REFRESH_TOKEN` flow alias, a `fixture` name in `/health`, and a fresh key id on every
    /// start.
    #[arg(long)]
    compat_mode: bool,
    /// The pool name `ListUserPools` reports in compatibility mode.
    #[arg(long, default_value = DEFAULT_NAME)]
    pool_name: String,
    /// The client name `ListUserPoolClients` reports in compatibility mode.
    #[arg(long, default_value = DEFAULT_NAME)]
    client_name: String,
    /// The `ConfirmForgotPassword` code for accounts created in compatibility mode.
    #[arg(long, default_value = DEFAULT_RESET_CODE)]
    reset_code: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("cognito_mock=info,warn")
        .init();
    let args = Args::parse();
    anyhow::ensure!(
        args.allow_container_network || args.listen.ip().is_loopback(),
        "fixture must bind loopback"
    );
    let config = Config {
        issuer: args.issuer,
        state_file: args.state_file,
        compat_mode: args.compat_mode,
        pool_name: args.pool_name,
        client_name: args.client_name,
        reset_code: args.reset_code,
        ..Config::new(args.pool_id, args.client_id)
    };
    let listener = TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("bind {}", args.listen))?;
    info!(listen = %args.listen, "Rust Cognito fixture ready");
    cognito_mock::serve(listener, config).await
}
