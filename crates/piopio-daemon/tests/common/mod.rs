//! Helpers shared by the integration tests.
//!
//! Every test gets its own private session bus, so tests can run in parallel
//! without their daemons fighting over the bus name.

#![allow(dead_code)]

use std::process::Stdio;
use std::time::Duration;

use piopio_dbus::{AccountProxy, DAEMON_BUS_NAME, DaemonProxy};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};
use zbus::proxy::CacheProperties;

/// A private bus with a daemon on it; both are killed when dropped.
pub struct TestDaemon {
    pub child: Child,
    pub connection: zbus::Connection,
    _bus: Child,
}

impl TestDaemon {
    /// Starts a bus and the daemon, or returns None when dbus-daemon is missing.
    pub async fn start() -> Option<Self> {
        let Ok(mut bus) = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        else {
            eprintln!("dbus-daemon not found; skipping");
            return None;
        };
        let mut address = String::new();
        BufReader::new(bus.stdout.take().unwrap())
            .read_line(&mut address)
            .await
            .expect("dbus-daemon prints its address");
        let address = address.trim().to_owned();

        let child = Command::new(env!("CARGO_BIN_EXE_piopio-daemon"))
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("daemon starts");

        let connection = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .await
            .expect("test connects to its bus");
        let dbus = zbus::fdo::DBusProxy::new(&connection).await.unwrap();
        let name: zbus::names::BusName<'_> = DAEMON_BUS_NAME.try_into().unwrap();
        timeout(Duration::from_secs(10), async {
            while !dbus.name_has_owner(name.clone()).await.unwrap() {
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("daemon claims its bus name");

        Some(Self {
            child,
            connection,
            _bus: bus,
        })
    }

    pub async fn daemon(&self) -> DaemonProxy<'static> {
        DaemonProxy::builder(&self.connection)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap()
    }

    pub async fn account(&self, path: zbus::zvariant::OwnedObjectPath) -> AccountProxy<'static> {
        AccountProxy::builder(&self.connection)
            .path(path)
            .unwrap()
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap()
    }
}

/// Polls `check` until it returns Some, or panics after `secs` seconds.
pub async fn wait_for<T, F, Fut>(secs: u64, what: &str, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    timeout(Duration::from_secs(secs), async {
        loop {
            if let Some(v) = check().await {
                return v;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}
