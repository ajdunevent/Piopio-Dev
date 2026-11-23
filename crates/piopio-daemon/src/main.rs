//! Piopio daemon: owns the XMPP connections and exposes them on the session bus.
//!
//! The daemon never loads GTK. It is D-Bus activatable as
//! `net.dunevent.Piopio.Daemon` and keeps running when no window is open.

mod access;
mod account;
mod daemon;
mod login;

use std::sync::Arc;

use anyhow::Context;
use piopio_dbus::{DAEMON_BUS_NAME, ROOT_PATH};
use tokio::sync::{Mutex, watch};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PIOPIO_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let (quit_tx, mut quit_rx) = watch::channel(false);
    let accounts = Arc::new(Mutex::new(daemon::Accounts::default()));

    let _connection = zbus::connection::Builder::session()?
        .serve_at(ROOT_PATH, daemon::Daemon::new(accounts.clone(), quit_tx))?
        .serve_at(ROOT_PATH, zbus::fdo::ObjectManager)?
        .name(DAEMON_BUS_NAME)?
        .build()
        .await
        .with_context(|| format!("could not claim {DAEMON_BUS_NAME} on the session bus"))?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "{DAEMON_BUS_NAME} is running"
    );

    tokio::select! {
        _ = quit_rx.wait_for(|quit| *quit) => tracing::info!("quit requested over D-Bus"),
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
    }

    accounts.lock().await.stop_all().await;
    Ok(())
}
