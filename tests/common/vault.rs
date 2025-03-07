use std::process::ExitStatus;

use anyhow::{Context, Result};
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Duration};
use url::Url;
use vaultrs::client::{Client, VaultClient, VaultClientSettingsBuilder};
use vaultrs::sys::ServerStatus;

use super::{free_port, spawn_server};

/// Start Hashicorp Vault as a subprocess on a random port
pub async fn start_vault(
    token: impl AsRef<str>,
) -> Result<(
    JoinHandle<Result<ExitStatus>>,
    oneshot::Sender<()>,
    Url,
    VaultClient,
)> {
    let bin_path = std::env::var("TEST_VAULT_BIN").unwrap_or("vault".to_string());
    let port = free_port()
        .await
        .context("failed to find open port for NATS")?;
    let host = "127.0.0.1";
    let (server, stop_tx) = spawn_server(Command::new(bin_path).args([
        "server",
        "-dev",
        "-dev-listen-address",
        &format!("{host}:{port}"),
        "-dev-root-token-id",
        token.as_ref(),
        "-dev-no-store-token",
    ]))
    .await
    .context("failed to start test Vault instance")?;
    let url = format!("http://{host}:{port}");

    // Create a vault client for use, while waiting for server to start taking connections
    let vault_client = VaultClient::new(
        VaultClientSettingsBuilder::default()
            .address(&url)
            .token(token.as_ref())
            .build()
            .context("failed to build vault client settings")?,
    )
    .context("failed to build vault client")?;
    let vault_client = timeout(Duration::from_secs(3), async move {
        loop {
            if let Ok(ServerStatus::OK) = vault_client.status().await {
                return vault_client;
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .context("failed to ensure connection to vault server")?;

    Ok((
        server,
        stop_tx,
        url.parse().context("failed to create URL from vault URL")?,
        vault_client,
    ))
}
