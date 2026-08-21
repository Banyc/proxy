//! Process-lifetime supervision, teardown, listener accept-loop, and
//! system-suspend detection.
//!
//! Root-owned actors (connector drivers, the retention actor, server
//! listeners, the system-suspend checker) are expected to run for the whole
//! process; any unexpected completion or panic is a fatal supervision error.
//!
//! - [`process`] — supervision types marking a root task's exit, and the
//!   fatal-on-unexpected-completion handling.
//! - [`task_scope`] — epilog reaping for a `JoinSet` that never swallows a
//!   child's panic as cancellation.
//! - [`serve_loop`] — wraps a listener's accept loop with bounded exponential
//!   backoff and a fatal-vs-transient error split.
//! - [`retention`] — keeps teardown guards (sockets/sessions) alive past
//!   their owner's drop until an explicit `until` instant.
//! - [`suspend`] — detects laptop/VM sleep by watching for a wall/monotonic
//!   clock gap and signals connectors to rebuild.
//!
//! `suspend`'s signal is wired into the connector-reset path, so a machine
//! waking from sleep tears down and rebuilds its outbound connectors instead
//! of sending through a now-stale route.

pub mod process;
pub mod retention;
pub mod serve_loop;
pub mod suspend;
pub mod task_scope;
