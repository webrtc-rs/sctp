use std::{
    collections::{HashMap, VecDeque},
    //fmt, iter,
    net::SocketAddr, //IpAddr
    ops::{Index, IndexMut},
    sync::Arc,
    //time::{Instant, SystemTime},
};

use crate::config::{EndpointConfig, ServerConfig};
use crate::shared::{AssociationEvent, AssociationId};
use crate::util::AssociationIdGenerator;
use crate::Transmit;

//use bytes::{BufMut, Bytes, BytesMut};
use crate::association::Association;
use fxhash::FxHashMap;
use rand::rngs::StdRng; //, Rng, RngCore, SeedableRng};
use slab::Slab;
use thiserror::Error;
//use tracing::{debug, trace, warn};

/// The main entry point to the library
///
/// This object performs no I/O whatsoever. Instead, it generates a stream of packets to send via
/// `poll_transmit`, and consumes incoming packets and connection-generated events via `handle` and
/// `handle_event`.
pub struct Endpoint {
    rng: StdRng,
    transmits: VecDeque<Transmit>,
    /// Identifies connections based on the initial DCID the peer utilized
    ///
    /// Uses a standard `HashMap` to protect against hash collision attacks.
    connection_ids_initial: HashMap<AssociationId, AssociationHandle>,
    /// Identifies connections based on locally created CIDs
    ///
    /// Uses a cheaper hash function since keys are locally created
    connection_ids: FxHashMap<AssociationId, AssociationHandle>,
    /// Identifies connections with zero-length CIDs
    ///
    /// Uses a standard `HashMap` to protect against hash collision attacks.
    connection_remotes: HashMap<SocketAddr, AssociationHandle>,
    /// Reset tokens provided by the peer for the CID each connection is currently sending to
    ///
    /// Incoming stateless resets do not have correct CIDs, so we need this to identify the correct
    /// recipient, if any.
    //TODO: connection_reset_tokens: ResetTokenTable,
    connections: Slab<AssociationMeta>,
    local_cid_generator: Box<dyn AssociationIdGenerator>,
    config: Arc<EndpointConfig>,
    server_config: Option<Arc<ServerConfig>>,
    /// Whether incoming connections should be unconditionally rejected by a server
    ///
    /// Equivalent to a `ServerConfig.accept_buffer` of `0`, but can be changed after the endpoint is constructed.
    reject_new_connections: bool,
}

#[derive(Debug)]
pub(crate) struct AssociationMeta {
    init_cid: AssociationId,
    /// Number of local connection IDs that have been issued in NEW_CONNECTION_ID frames.
    cids_issued: u64,
    loc_cids: FxHashMap<u64, AssociationId>,
    /// Remote address the connection began with
    ///
    /// Only needed to support connections with zero-length CIDs, which cannot migrate, so we don't
    /// bother keeping it up to date.
    initial_remote: SocketAddr,
    // Reset token provided by the peer for the CID we're currently sending to, and the address
    // being sent to
    // TODO: reset_token: Option<(SocketAddr, ResetToken)>,
}

/// Internal identifier for a `Connection` currently associated with an endpoint
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct AssociationHandle(pub usize);

impl From<AssociationHandle> for usize {
    fn from(x: AssociationHandle) -> usize {
        x.0
    }
}

impl Index<AssociationHandle> for Slab<AssociationMeta> {
    type Output = AssociationMeta;
    fn index(&self, ch: AssociationHandle) -> &AssociationMeta {
        &self[ch.0]
    }
}

impl IndexMut<AssociationHandle> for Slab<AssociationMeta> {
    fn index_mut(&mut self, ch: AssociationHandle) -> &mut AssociationMeta {
        &mut self[ch.0]
    }
}

/// Event resulting from processing a single datagram
#[allow(clippy::large_enum_variant)] // Not passed around extensively
pub enum DatagramEvent {
    /// The datagram is redirected to its `Association`
    AssociationEvent(AssociationEvent),
    /// The datagram has resulted in starting a new `Association`
    NewAssociation(Association),
}

/// Errors in the parameters being used to create a new association
///
/// These arise before any I/O has been performed.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// The endpoint can no longer create new associations
    ///
    /// Indicates that a necessary component of the endpoint has been dropped or otherwise disabled.
    #[error("endpoint stopping")]
    EndpointStopping,
    /// The number of active associations on the local endpoint is at the limit
    ///
    /// Try using longer association IDs.
    #[error("too many associations")]
    TooManyAssociations,
    /// The domain name supplied was malformed
    #[error("invalid DNS name: {0}")]
    InvalidDnsName(String),
    /// The remote [`SocketAddr`] supplied was malformed
    ///
    /// Examples include attempting to connect to port 0, or using an inappropriate address family.
    #[error("invalid remote address: {0}")]
    InvalidRemoteAddress(SocketAddr),
    /// No default client configuration was set up
    ///
    /// Use `Endpoint::connect_with` to specify a client configuration.
    #[error("no default client config")]
    NoDefaultClientConfig,
}
