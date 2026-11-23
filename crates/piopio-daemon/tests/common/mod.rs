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
    pub data: std::sync::Arc<tempfile::TempDir>,
    _bus: Child,
}

impl TestDaemon {
    /// Starts a bus and a daemon with a fresh data directory, or returns None
    /// when dbus-daemon is missing.
    pub async fn start() -> Option<Self> {
        Self::start_with(std::sync::Arc::new(tempfile::tempdir().unwrap())).await
    }

    /// Starts a daemon on `data` (to test what survives a restart).
    pub async fn start_with(data: std::sync::Arc<tempfile::TempDir>) -> Option<Self> {
        Self::start_with_env(data, &[]).await
    }

    /// Starts a daemon that only speaks legacy OMEMO, like most clients today.
    pub async fn start_legacy_only() -> Option<Self> {
        Self::start_with_env(
            std::sync::Arc::new(tempfile::tempdir().unwrap()),
            &[("PIOPIO_TEST_LEGACY_OMEMO_ONLY", "1")],
        )
        .await
    }

    pub async fn start_with_env(
        data: std::sync::Arc<tempfile::TempDir>,
        env: &[(&str, &str)],
    ) -> Option<Self> {
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
            .env("PIOPIO_DATA_DIR", data.path())
            .env("PIOPIO_DB_KEY", "test-key")
            .env("PIOPIO_ALLOW_PLAINTEXT", "1")
            .envs(env.iter().copied())
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
            data,
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

/// The local test server (tests/server/run.sh), if one is running.
pub fn test_server() -> Option<String> {
    std::env::var("PIOPIO_TEST_SERVER").ok()
}

impl TestDaemon {
    /// Adds `user@localhost` on the local test server and waits until it is connected.
    pub async fn add_local(
        &self,
        user: &str,
    ) -> (zbus::zvariant::OwnedObjectPath, AccountProxy<'static>) {
        let server = test_server().expect("PIOPIO_TEST_SERVER is set");
        let mut options = std::collections::HashMap::new();
        options.insert(
            "test-plaintext-address",
            zbus::zvariant::Value::from(server),
        );
        let path = self
            .daemon()
            .await
            .add_account(&format!("{user}@localhost"), "password", options)
            .await
            .expect("AddAccount");
        let account = self.account(path.clone()).await;
        wait_until_connected(&account).await;
        (path, account)
    }

    pub async fn roster(
        &self,
        path: zbus::zvariant::OwnedObjectPath,
    ) -> piopio_dbus::RosterProxy<'static> {
        piopio_dbus::RosterProxy::builder(&self.connection)
            .path(path)
            .unwrap()
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap()
    }

    pub async fn conversation(
        &self,
        path: zbus::zvariant::OwnedObjectPath,
    ) -> piopio_dbus::ConversationProxy<'static> {
        piopio_dbus::ConversationProxy::builder(&self.connection)
            .path(path)
            .unwrap()
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap()
    }

    /// Object paths of the account's conversations, from the ObjectManager.
    pub async fn conversations(
        &self,
        account: &zbus::zvariant::OwnedObjectPath,
    ) -> Vec<zbus::zvariant::OwnedObjectPath> {
        let manager = zbus::fdo::ObjectManagerProxy::builder(&self.connection)
            .destination(piopio_dbus::DAEMON_BUS_NAME)
            .unwrap()
            .path(piopio_dbus::ROOT_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();
        let prefix = format!("{}/conversation/", account.as_str());
        let mut paths: Vec<_> = manager
            .get_managed_objects()
            .await
            .unwrap()
            .into_keys()
            .filter(|p| p.as_str().starts_with(&prefix))
            .collect();
        paths.sort_by_key(|p| p.as_str().to_owned());
        paths
    }
}

impl TestDaemon {
    pub async fn omemo(
        &self,
        path: zbus::zvariant::OwnedObjectPath,
    ) -> piopio_dbus::OmemoProxy<'static> {
        piopio_dbus::OmemoProxy::builder(&self.connection)
            .path(path)
            .unwrap()
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap()
    }

    /// The account's conversation with `peer`, if there is one yet.
    pub async fn conversation_with(
        &self,
        account: &zbus::zvariant::OwnedObjectPath,
        peer: &str,
    ) -> Option<zbus::zvariant::OwnedObjectPath> {
        for path in self.conversations(account).await {
            if self.conversation(path.clone()).await.peer().await.ok()? == peer {
                return Some(path);
            }
        }
        None
    }

    /// Waits until the account's OMEMO device is published; returns its id.
    pub async fn wait_published(&self, path: &zbus::zvariant::OwnedObjectPath) -> u32 {
        let omemo = self.omemo(path.clone()).await;
        wait_for(20, "the OMEMO device to be published", || async {
            omemo.published().await.unwrap().then_some(())
        })
        .await;
        omemo.device_id().await.unwrap()
    }

    /// Waits until this account knows `device` of `jid`; returns its entry.
    pub async fn wait_device(
        &self,
        path: &zbus::zvariant::OwnedObjectPath,
        jid: &str,
        device: u32,
    ) -> piopio_dbus::Dict {
        let omemo = self.omemo(path.clone()).await;
        wait_for(20, "an OMEMO device to become known", || async {
            omemo
                .get_devices(jid)
                .await
                .unwrap()
                .into_iter()
                .find(|d| d.get("device-id").and_then(|v| u32::try_from(v).ok()) == Some(device))
        })
        .await
    }
}

/// The message with `id` in a list from GetMessages.
pub fn message<'a>(messages: &'a [piopio_dbus::Dict], id: &str) -> Option<&'a piopio_dbus::Dict> {
    messages.iter().find(|m| text(m, "id") == id)
}

pub async fn wait_until_connected(account: &AccountProxy<'_>) {
    let state = wait_for(30, "the account to connect or fail", || async {
        let state = account.state().await.unwrap();
        (state == "connected" || state == "error").then_some(state)
    })
    .await;
    let error = account.error().await.unwrap();
    assert_eq!(state, "connected", "login failed: {error}");
}

/// The string value of `key` in a D-Bus dictionary.
pub fn text(dict: &piopio_dbus::Dict, key: &str) -> String {
    dict.get(key)
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default()
}
