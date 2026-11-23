//! The single gate every D-Bus method call passes through.
//!
//! Milestone 1 accepts any caller on the session bus, like most session
//! services. Because every method calls [`check`], callers can later be limited
//! to the Piopio UI (by app ID, from the caller's credentials) without changing
//! the API.

use zbus::message::Header;

pub fn check(header: &Header<'_>) -> zbus::fdo::Result<()> {
    tracing::trace!(sender = ?header.sender(), member = ?header.member(), "D-Bus call");
    Ok(())
}
