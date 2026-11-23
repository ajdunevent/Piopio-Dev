//! Starts the real daemon binary on a session bus and talks to it like the UI would.
//!
//! Needs a session bus: run with `dbus-run-session -- cargo test`. Without one
//! the test is skipped.

use std::process::Stdio;
use std::time::Duration;

use piopio_dbus::{DAEMON_API_VERSION, DAEMON_BUS_NAME, DaemonProxy};
use tokio::process::Command;
use tokio::time::{sleep, timeout};

#[tokio::test]
async fn daemon_answers_and_quits() {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        eprintln!("no session bus; skipping (run under dbus-run-session)");
        return;
    }

    let mut child = Command::new(env!("CARGO_BIN_EXE_piopio-daemon"))
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("daemon starts");

    let connection = zbus::Connection::session().await.expect("session bus");
    let dbus = zbus::fdo::DBusProxy::new(&connection).await.unwrap();
    let name: zbus::names::BusName<'_> = DAEMON_BUS_NAME.try_into().unwrap();
    timeout(Duration::from_secs(10), async {
        while !dbus.name_has_owner(name.clone()).await.unwrap() {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("daemon claims its bus name");

    let daemon = DaemonProxy::new(&connection).await.unwrap();
    assert_eq!(daemon.api_version().await.unwrap(), DAEMON_API_VERSION);

    daemon.quit().await.unwrap();
    let status = timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("daemon exits after Quit")
        .unwrap();
    assert!(status.success());
}
