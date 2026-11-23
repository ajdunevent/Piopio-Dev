//! Piopio daemon: owns the XMPP connections and exposes them on the session bus.
//!
//! The daemon never loads GTK. It is D-Bus activatable as
//! `net.dunevent.Piopio.Daemon` and keeps running when no window is open.

mod access;
mod account;
mod activity;
mod background;
mod connect;
mod daemon;
mod disco;
mod login;
mod messages;
mod notify;
mod roster;
mod session;
mod store;

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

    let key = store::database_key().await?;
    let dir = store::data_dir();
    let store = store::Store::open(&dir, &key)?;
    tracing::debug!(dir = %dir.display(), "database open");

    let (quit_tx, mut quit_rx) = watch::channel(false);
    let accounts = Arc::new(Mutex::new(daemon::Accounts::new(store)));

    let connection = zbus::connection::Builder::session()?
        .serve_at(ROOT_PATH, daemon::Daemon::new(accounts.clone(), quit_tx))?
        .serve_at(ROOT_PATH, zbus::fdo::ObjectManager)?
        .build()
        .await
        .context("could not connect to the session bus")?;

    activity::watch_ui(&connection).await?;
    accounts.lock().await.load(&connection).await?;
    // Claim the name last, so activated callers find the accounts already exported.
    connection
        .request_name(DAEMON_BUS_NAME)
        .await
        .with_context(|| {
            format!("could not claim {DAEMON_BUS_NAME}; is another daemon running?")
        })?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "{DAEMON_BUS_NAME} is running"
    );

    tokio::spawn(background::request(connection.clone()));

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = quit_rx.wait_for(|quit| *quit) => tracing::info!("quit requested over D-Bus"),
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
        _ = terminate.recv() => tracing::info!("terminated"),
    }

    accounts.lock().await.stop_all().await;
    Ok(())
}
