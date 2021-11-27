use std::{
    collections::{HashMap, VecDeque},
    iter,
    //fmt,
    net::{IpAddr, SocketAddr},
    ops::{Index, IndexMut},
    sync::Arc,
    time::Instant, //, SystemTime},
};

use crate::association::Association;
use crate::config::{ClientConfig, EndpointConfig, ServerConfig, TransportConfig};
use crate::shared::{
    AssociationEvent, AssociationEventInner, AssociationId, EndpointEvent, EndpointEventInner,
    IssuedAid,
};
use crate::util::{AssociationIdGenerator, RandomAssociationIdGenerator};
use crate::{EcnCodepoint, Transmit};

use bytes::{BufMut, Bytes, BytesMut};
use fxhash::FxHashMap;
use rand::{rngs::StdRng, /*Rng, RngCore,*/ SeedableRng};
use slab::Slab;
use thiserror::Error;
use tracing::{debug, trace, warn};

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

impl Endpoint {
    /// Create a new endpoint
    ///
    /// Returns `Err` if the configuration is invalid.
    pub fn new(config: Arc<EndpointConfig>, server_config: Option<Arc<ServerConfig>>) -> Self {
        Self {
            rng: StdRng::from_entropy(),
            transmits: VecDeque::new(),
            connection_ids_initial: HashMap::default(),
            connection_ids: FxHashMap::default(),
            connection_remotes: HashMap::default(),
            //TODO: connection_reset_tokens: ResetTokenTable::default(),
            connections: Slab::new(),
            local_cid_generator: (config.aid_generator_factory.as_ref())(),
            reject_new_connections: false,
            config,
            server_config,
        }
    }

    /// Get the next packet to transmit
    #[must_use]
    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    /// Replace the server configuration, affecting new incoming associations only
    pub fn set_server_config(&mut self, server_config: Option<Arc<ServerConfig>>) {
        self.server_config = server_config;
    }

    /// Process `EndpointEvent`s emitted from related `Connection`s
    ///
    /// In turn, processing this event may return a `ConnectionEvent` for the same `Connection`.
    pub fn handle_event(
        &mut self,
        ch: AssociationHandle,
        event: EndpointEvent,
    ) -> Option<AssociationEvent> {
        use EndpointEventInner::*;
        match event.0 {
            NeedIdentifiers(now, n) => {
                return Some(self.send_new_identifiers(now, ch, n));
            }
            /*TODO: ResetToken(remote, token) => {
                if let Some(old) = self.connections[ch].reset_token.replace((remote, token)) {
                    self.connection_reset_tokens.remove(old.0, old.1);
                }
                if self.connection_reset_tokens.insert(remote, token, ch) {
                    warn!("duplicate reset token");
                }
            }*/
            RetireAssociationId(now, seq, allow_more_aids) => {
                if let Some(cid) = self.connections[ch].loc_cids.remove(&seq) {
                    trace!("peer retired AID {}: {}", seq, cid);
                    self.connection_ids.remove(&cid);
                    if allow_more_aids {
                        return Some(self.send_new_identifiers(now, ch, 1));
                    }
                }
            }
            Drained => {
                let conn = self.connections.remove(ch.0);
                self.connection_ids_initial.remove(&conn.init_cid);
                for cid in conn.loc_cids.values() {
                    self.connection_ids.remove(cid);
                }
                self.connection_remotes.remove(&conn.initial_remote);
                /*TODO: if let Some((remote, token)) = conn.reset_token {
                    self.connection_reset_tokens.remove(remote, token);
                }*/
            }
        }
        None
    }

    /// Process an incoming UDP datagram
    pub fn handle(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        local_ip: Option<IpAddr>,
        ecn: Option<EcnCodepoint>,
        data: BytesMut,
    ) -> Option<(AssociationHandle, DatagramEvent)> {
        /*let datagram_len = data.len();
        let (first_decode, remaining) = match PartialDecode::new(
            data,
            self.local_cid_generator.cid_len(),
            &self.config.supported_versions,
        ) {
            Ok(x) => x,
            Err(PacketDecodeError::UnsupportedVersion {
                src_cid,
                dst_cid,
                version,
            }) => {
                if self.server_config.is_none() {
                    debug!("dropping packet with unsupported version");
                    return None;
                }
                trace!("sending version negotiation");
                // Negotiate versions
                let mut buf = Vec::<u8>::new();
                Header::VersionNegotiate {
                    random: self.rng.gen::<u8>() | 0x40,
                    src_cid: dst_cid,
                    dst_cid: src_cid,
                }
                .encode(&mut buf);
                // Grease with a reserved version
                if version != 0x0a1a_2a3a {
                    buf.write::<u32>(0x0a1a_2a3a);
                } else {
                    buf.write::<u32>(0x0a1a_2a4a);
                }
                for &version in &self.config.supported_versions {
                    buf.write(version);
                }
                self.transmits.push_back(Transmit {
                    destination: remote,
                    ecn: None,
                    contents: buf,
                    segment_size: None,
                    src_ip: local_ip,
                });
                return None;
            }
            Err(e) => {
                trace!("malformed header: {}", e);
                return None;
            }
        };

        //
        // Handle packet on existing connection, if any
        //

        let dst_cid = first_decode.dst_cid();
        let known_ch = {
            let ch = if self.local_cid_generator.cid_len() > 0 {
                self.connection_ids.get(&dst_cid)
            } else {
                None
            };
            ch.or_else(|| {
                if first_decode.is_initial() || first_decode.is_0rtt() {
                    self.connection_ids_initial.get(&dst_cid)
                } else {
                    None
                }
            })
            .or_else(|| {
                if self.local_cid_generator.cid_len() == 0 {
                    self.connection_remotes.get(&remote)
                } else {
                    None
                }
            })
            .or_else(|| {
                let data = first_decode.data();
                if data.len() < RESET_TOKEN_SIZE {
                    return None;
                }
                self.connection_reset_tokens
                    .get(remote, &data[data.len() - RESET_TOKEN_SIZE..])
            })
            .cloned()
        };
        if let Some(ch) = known_ch {
            return Some((
                ch,
                DatagramEvent::ConnectionEvent(ConnectionEvent(ConnectionEventInner::Datagram {
                    now,
                    remote,
                    ecn,
                    first_decode,
                    remaining,
                })),
            ));
        }

        //
        // Potentially create a new connection
        //

        let server_config = match &self.server_config {
            Some(config) => config,
            None => {
                debug!("packet for unrecognized connection {}", dst_cid);
                self.stateless_reset(datagram_len, remote, local_ip, &dst_cid);
                return None;
            }
        };

        if let Some(version) = first_decode.initial_version() {
            if datagram_len < MIN_INITIAL_SIZE as usize {
                debug!("ignoring short initial for connection {}", dst_cid);
                return None;
            }

            let crypto = match server_config
                .crypto
                .initial_keys(version, &dst_cid, Side::Server)
            {
                Ok(keys) => keys,
                Err(UnsupportedVersion) => {
                    // This probably indicates that the user set supported_versions incorrectly in
                    // `EndpointConfig`.
                    debug!(
                        "ignoring initial packet version {:#x} unsupported by cryptographic layer",
                        version
                    );
                    return None;
                }
            };
            return match first_decode.finish(Some(&*crypto.header.remote)) {
                Ok(packet) => self
                    .handle_first_packet(now, remote, local_ip, ecn, packet, remaining, &crypto)
                    .map(|(ch, conn)| (ch, DatagramEvent::NewConnection(conn))),
                Err(e) => {
                    trace!("unable to decode initial packet: {}", e);
                    None
                }
            };
        } else if first_decode.has_long_header() {
            debug!(
                "ignoring non-initial packet for unknown connection {}",
                dst_cid
            );
            return None;
        }

        //
        // If we got this far, we're a server receiving a seemingly valid packet for an unknown
        // connection. Send a stateless reset.
        //

        if !dst_cid.is_empty() {
            self.stateless_reset(datagram_len, remote, local_ip, &dst_cid);
        } else {
            trace!("dropping unrecognized short packet without ID");
        }*/
        None
    }

    /// Initiate a Association
    pub fn connect(
        &mut self,
        config: ClientConfig,
        remote: SocketAddr,
        server_name: &str,
    ) -> Result<(AssociationHandle, Association), ConnectError> {
        if self.is_full() {
            return Err(ConnectError::TooManyAssociations);
        }
        if remote.port() == 0 {
            return Err(ConnectError::InvalidRemoteAddress(remote));
        }

        let remote_id = RandomAssociationIdGenerator::new().generate_aid();
        trace!(initial_dcid = %remote_id);

        let loc_cid = self.new_aid();
        /*TODO: let params = TransportParameters::new(
            &config.transport,
            &self.config,
            self.local_cid_generator.as_ref(),
            loc_cid,
            None,
        );
        let tls = config
            .crypto
            .start_session(config.version, server_name, &params)?;*/

        let (ch, conn) = self.add_connection(
            //config.version,
            remote_id,
            loc_cid,
            remote_id,
            remote,
            None,
            Instant::now(),
            //tls,
            None,
            config.transport,
        );
        Ok((ch, conn))
    }

    fn send_new_identifiers(
        &mut self,
        now: Instant,
        ch: AssociationHandle,
        num: u64,
    ) -> AssociationEvent {
        let mut ids = vec![];
        for _ in 0..num {
            let id = self.new_aid();
            self.connection_ids.insert(id, ch);
            let meta = &mut self.connections[ch];
            meta.cids_issued += 1;
            let sequence = meta.cids_issued;
            meta.loc_cids.insert(sequence, id);
            ids.push(IssuedAid {
                sequence,
                id,
                //TODO: reset_token: ResetToken::new(&*self.config.reset_key, &id),
            });
        }
        AssociationEvent(AssociationEventInner::NewIdentifiers(ids, now))
    }

    fn new_aid(&mut self) -> AssociationId {
        loop {
            let aid = self.local_cid_generator.generate_aid();
            if !self.connection_ids.contains_key(&aid) {
                break aid;
            }
        }
    }

    fn add_connection(
        &mut self,
        //version: u32,
        init_cid: AssociationId,
        loc_cid: AssociationId,
        rem_cid: AssociationId,
        remote: SocketAddr,
        local_ip: Option<IpAddr>,
        now: Instant,
        //tls: Box<dyn crypto::Session>,
        server_config: Option<Arc<ServerConfig>>,
        transport_config: Arc<TransportConfig>,
    ) -> (AssociationHandle, Association) {
        let conn = Association::new(
            server_config,
            transport_config,
            init_cid,
            loc_cid,
            rem_cid,
            remote,
            local_ip,
            //tls,
            self.local_cid_generator.as_ref(),
            now,
            //version,
        );

        let id = self.connections.insert(AssociationMeta {
            init_cid,
            cids_issued: 0,
            loc_cids: iter::once((0, loc_cid)).collect(),
            initial_remote: remote,
            //reset_token: None,
        });

        let ch = AssociationHandle(id);
        //TODO: match self.local_cid_generator.cid_len() {
        //    0 => self.connection_remotes.insert(remote, ch),
        //    _ => ,
        //};
        self.connection_ids.insert(loc_cid, ch);

        (ch, conn)
    }

    /// Unconditionally reject future incoming connections
    pub fn reject_new_connections(&mut self) {
        self.reject_new_connections = true;
    }

    /// Access the configuration used by this endpoint
    pub fn config(&self) -> &EndpointConfig {
        &self.config
    }

    /// Whether we've used up 3/4 of the available AID space
    fn is_full(&self) -> bool {
        (((u32::MAX >> 1) + (u32::MAX >> 2)) as usize) < self.connection_ids.len()
    }
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
