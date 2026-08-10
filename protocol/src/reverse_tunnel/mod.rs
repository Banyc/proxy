//! Reverse tunnels: a named connection registered by an initiator with a
//! responder, over a mux transport (TCP or RTP mux). The initiator keeps
//! reconnecting until the responder accepts its registration; accepted
//! streams are dispatched to the initiator's stream/UDP proxy handlers so
//! the wire format is identical to a regular mux proxy hop.
//!
//! Module layout:
//! - `wire` — registration wire protocol plus shared session plumbing.
//! - `initiator` — the connecting side (handler, reconnect loop, builders).
//! - `responder` — the accepting side (registration, named connectors,
//!   TCP/RTP listeners, builders).
//! - `loading` — config types, the loader/prepare pipeline, and build
//!   errors.

mod initiator;
mod loading;
mod responder;
mod wire;

pub use initiator::{
    ReverseTunnelInitiator, ReverseTunnelInitiatorBuilder, ReverseTunnelInitiatorHandler,
};
pub use loading::{
    BuildError, PreparedReverseTunnel, ReverseTunnelConfig, ReverseTunnelInitiatorConfig,
    ReverseTunnelLoader, ReverseTunnelResponderConfig, prepare,
};
pub use responder::{
    ReverseTunnelResponderHandler, RtpReverseTunnelResponder, RtpReverseTunnelResponderBuilder,
    TcpReverseTunnelResponder, TcpReverseTunnelResponderBuilder,
};

#[cfg(test)]
mod tests;
