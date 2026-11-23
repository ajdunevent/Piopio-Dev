//! The bridge between GTK's main loop and the daemon.
//!
//! zbus runs on a small tokio runtime of its own. The UI awaits results with
//! [`run`], and daemon signals reach the main loop through channels made by
//! [`forward`], so no GTK object ever leaves the main thread.

use std::future::Future;
use std::sync::OnceLock;

use futures_util::{Stream, StreamExt};
use tokio::runtime::Runtime;

pub fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Runs `future` on the D-Bus runtime and waits for it from the main loop.
pub async fn run<F>(future: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime().spawn(future).await.expect("D-Bus task panicked")
}

/// Pumps a stream from the D-Bus runtime into a channel the main loop reads.
pub fn forward<S, T, Fut>(make: impl FnOnce() -> Fut + Send + 'static) -> async_channel::Receiver<T>
where
    Fut: Future<Output = Option<S>> + Send + 'static,
    S: Stream<Item = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = async_channel::unbounded();
    runtime().spawn(async move {
        let Some(stream) = make().await else { return };
        let mut stream = std::pin::pin!(stream);
        while let Some(item) = stream.next().await {
            if tx.send(item).await.is_err() {
                break;
            }
        }
    });
    rx
}

/// The session bus connection, opened on first use.
pub async fn bus() -> zbus::Result<zbus::Connection> {
    static BUS: tokio::sync::OnceCell<zbus::Connection> = tokio::sync::OnceCell::const_new();
    run(async {
        BUS.get_or_try_init(zbus::Connection::session)
            .await
            .cloned()
    })
    .await
}

/// A short, human message for a D-Bus error.
pub fn explain(error: &zbus::Error) -> String {
    match error {
        zbus::Error::MethodError(_, Some(text), _) => text.clone(),
        zbus::Error::FDO(e) => match e.as_ref() {
            zbus::fdo::Error::InvalidArgs(t)
            | zbus::fdo::Error::Failed(t)
            | zbus::fdo::Error::AccessDenied(t) => t.clone(),
            other => other.to_string(),
        },
        other => other.to_string(),
    }
}
