//! Low-level protocol logic for the SCTP protocol
//!
//! sctp-proto contains a fully deterministic implementation of SCTP protocol logic. It contains
//! no networking code and does not get any relevant timestamps from the operating system. Most
//! users may want to use the futures-based sctp-async API instead.
//!
//! The sctp-proto API might be of interest if you want to use it from a C or C++ project
//! through C bindings or if you want to use a different event loop than the one tokio provides.
//!
//! The most important types are `Endpoint`, which conceptually represents the protocol state for
//! a single socket and mostly manages configuration and dispatches incoming datagrams to the
//! related `Association`. `Association` types contain the bulk of the protocol logic related to
//! managing a single association and all the related state (such as streams).

#![warn(rust_2018_idioms)]
#![allow(dead_code)]
#![allow(clippy::too_many_arguments)]

use crate::shared::EcnCodepoint;

use crate::packet::PartialDecode;
use bytes::Bytes;
use std::time::Instant;
use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    ops,
};

pub mod association;
pub mod chunk;
pub mod config;
pub mod endpoint;
pub mod error;
pub mod packet;
pub mod param;
pub mod shared;
pub mod stream;

pub(crate) mod queue;
pub(crate) mod util;

/// Whether an endpoint was the initiator of a connection
#[cfg_attr(feature = "arbitrary", derive(Arbitrary))]
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Side {
    /// The initiator of a connection
    Client = 0,
    /// The acceptor of a connection
    Server = 1,
}

impl Default for Side {
    fn default() -> Self {
        Side::Client
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            Side::Client => "Client",
            Side::Server => "Server",
        };
        write!(f, "{}", s)
    }
}

impl Side {
    #[inline]
    /// Shorthand for `self == Side::Client`
    pub fn is_client(self) -> bool {
        self == Side::Client
    }

    #[inline]
    /// Shorthand for `self == Side::Server`
    pub fn is_server(self) -> bool {
        self == Side::Server
    }
}

impl ops::Not for Side {
    type Output = Side;
    fn not(self) -> Side {
        match self {
            Side::Client => Side::Server,
            Side::Server => Side::Client,
        }
    }
}

#[derive(Debug)]
pub enum Payload {
    PartialDecode(PartialDecode),
    RawEncode(Vec<Bytes>),
}

/// Incoming/outgoing Transmit
#[derive(Debug)]
pub struct Transmit {
    /// Received/Sent time
    pub now: Instant,
    /// The socket this datagram should be sent to
    pub remote: SocketAddr,
    /// Explicit congestion notification bits to set on the packet
    pub ecn: Option<EcnCodepoint>,
    /// Payload of the datagram
    pub payload: Payload,
    /// Optional local IP address for the datagram
    pub local_ip: Option<IpAddr>,
}
