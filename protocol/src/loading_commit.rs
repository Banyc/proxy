//! Aggregation for a listener loader's per-kind commit chain.
//!
//! A listener loader commits several independent kinds: the access server's
//! tcp/udp/http/socks5 kinds, the proxy server's per-transport kinds, or the
//! reverse tunnel's initiator and responders. The kinds share no live state —
//! each is its own [`common::loading::Loader`] with its own handle map and its
//! own prepared ops — so one kind losing a listener must not suppress a kind
//! whose listeners are healthy and whose commit cannot fail. Every kind is
//! attempted, and the failures are reported together, each named, so the error
//! means "these kinds lost a handler update" rather than "the reload stopped
//! here".

use common::error::{AnyError, AnyResult};

/// Fold the per-kind commit failures into one error. No failures is success;
/// otherwise the error names every kind that lost a handler update and
/// preserves each kind's own error text.
pub(crate) fn commit_failures(failures: Vec<(&'static str, AnyError)>) -> AnyResult {
    if failures.is_empty() {
        return Ok(());
    }
    let detail = failures
        .iter()
        .map(|(kind, error)| format!("{kind}: {error}"))
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!(
        "reload commit lost handler updates in {} kind(s): {detail}",
        failures.len()
    )
    .into())
}
