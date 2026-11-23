use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Parser, Subcommand};
use drawbridge_appservice::{AppServiceState, router};
use drawbridge_core::secret;
use drawbridge_store::Store;
use meshcore_mautrix::{
    config::Config,
    meshcore::MeshCoreClient,
    runtime::{self, Bridge},
};
use rand::RngCore;
use std::path::PathBuf;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "MeshCore to Matrix bridge on Drawbridge")]
struct Cli {
    #[arg(long, default_value = "config.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the application service and MeshCore poller.
    Run,
    /// Validate configuration, registration, database key and sidecar access.
    Check,
    /// List bound Matrix rooms.
    Rooms,
    /// Send a test message through the sidecar without involving Matrix.
    Send {
        #[arg(long)]
        channel: Option<u32>,
        #[arg(long)]
        contact: Option<String>,
        #[arg(long)]
        text: String,
    },
    /// Generate a base64 database key.
    GenerateKey,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    match cli.command {
        Command::Run => run(config).await,
        Command::Check => runtime::check(&config).await,
        Command::Rooms => rooms(&config),
        Command::Send {
            channel,
            contact,
            text,
        } => send(&config, channel, contact, text).await,
        Command::GenerateKey => {
            let mut bytes = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut bytes);
            println!("{}", STANDARD.encode(bytes));
            Ok(())
        }
    }
}

async fn run(config: Config) -> Result<()> {
    let listen = config.listen;
    let poll_seconds = config.poll_seconds;
    let (bridge, hs_token) = Bridge::new(config).await?;
    let state = AppServiceState::new(bridge.clone(), hs_token);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    info!(%listen, poll_seconds, "meshcore-mautrix listening");
    let poller = tokio::spawn(bridge.clone().poll_loop());
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await;
    poller.abort();
    result.context("HTTP server failed")
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    warn!("shutdown requested");
}

fn rooms(config: &Config) -> Result<()> {
    let key = secret(&config.encryption_key_env)?;
    let store = Store::open(&config.database, &key, "")?;
    for binding in store.bindings(&config.tenant)? {
        println!(
            "{}\t{}\t{}\t{}",
            binding.key, binding.display, binding.room, binding.ghost
        );
    }
    Ok(())
}

async fn send(
    config: &Config,
    channel: Option<u32>,
    contact: Option<String>,
    text: String,
) -> Result<()> {
    let token = secret(&config.meshcore.token_env)?;
    let mesh = MeshCoreClient::new(config.meshcore.url.clone(), token)?;
    let ack = match (channel, contact) {
        (Some(channel), None) => mesh.send_channel(channel, &text).await?,
        (None, Some(contact)) => mesh.send_contact(&contact, &text).await?,
        _ => bail!("pass exactly one of --channel or --contact"),
    };
    println!("{} {}", ack.request_id, ack.state);
    Ok(())
}
