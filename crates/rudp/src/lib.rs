//! Reliable UDP transport for Noita Online.
//!
//! * [`connection::Connection`]: transport-agnostic reliability core (acks, resend,
//!   ordering, fragmentation) driven by explicit timestamps.
//! * [`endpoint::Server`] / [`endpoint::Client`]: non-blocking UDP endpoints with a
//!   challenge/response handshake.
//! * [`linksim`]: loss/latency injection used by tests and the `--netsim` option.

pub mod connection;
pub mod endpoint;
pub mod linksim;
pub mod packet;
pub mod seq;
pub mod wire;

pub use connection::{ChannelKind, Config, Connection, SendError, Stats};
pub use endpoint::{Client, ClientState, Clock, DisconnectReason, Server, ServerEvent};
pub use linksim::LinkConditions;
