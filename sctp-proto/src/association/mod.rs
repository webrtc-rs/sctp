use crate::association::{
    state::{AckMode, AckState, AssociationState},
    stats::AssociationStats,
};
use crate::chunk::{
    chunk_abort::ChunkAbort, chunk_cookie_ack::ChunkCookieAck, chunk_cookie_echo::ChunkCookieEcho,
    chunk_error::ChunkError, chunk_forward_tsn::ChunkForwardTsn, chunk_heartbeat::ChunkHeartbeat,
    chunk_init::ChunkInit, chunk_payload_data::ChunkPayloadData, chunk_reconfig::ChunkReconfig,
    chunk_selective_ack::ChunkSelectiveAck, chunk_shutdown::ChunkShutdown,
    chunk_shutdown_ack::ChunkShutdownAck, chunk_shutdown_complete::ChunkShutdownComplete,
    chunk_type::CT_FORWARD_TSN, Chunk,
};
use crate::config::{
    ServerConfig, TransportConfig, COMMON_HEADER_SIZE, DATA_CHUNK_HEADER_SIZE, INITIAL_MTU,
};
use crate::error::{Error, Result};
use crate::packet::Packet;
use crate::param::param_supported_extensions::ParamSupportedExtensions;
use crate::param::{
    param_outgoing_reset_request::ParamOutgoingResetRequest, param_state_cookie::ParamStateCookie,
};
use crate::shared::AssociationId;
use crate::util::{sna32lt, AssociationIdGenerator};
use crate::Side;

use crate::chunk::chunk_heartbeat_ack::ChunkHeartbeatAck;
use crate::param::param_heartbeat_info::ParamHeartbeatInfo;
use bytes::Bytes;
use rand::random;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, trace, warn};

mod state;
mod stats;
mod streams;
mod timer;

///Association represents an SCTP association
///13.2.  Parameters Necessary per Association (i.e., the TCB)
///Peer : Tag value to be sent in every packet and is received
///Verification: in the INIT or INIT ACK chunk.
///Tag :
///
///My : Tag expected in every inbound packet and sent in the
///Verification: INIT or INIT ACK chunk.
///
///Tag :
///State : A state variable indicating what state the association
/// : is in, i.e., COOKIE-WAIT, COOKIE-ECHOED, ESTABLISHED,
/// : SHUTDOWN-PENDING, SHUTDOWN-SENT, SHUTDOWN-RECEIVED,
/// : SHUTDOWN-ACK-SENT.
///
/// No Closed state is illustrated since if a
/// association is Closed its TCB SHOULD be removed.
#[derive(Default, Debug)]
pub struct Association {
    side: Side,
    state: AssociationState,
    max_message_size: u32,
    inflight_queue_length: usize,
    will_send_shutdown: bool,
    bytes_received: usize,
    bytes_sent: usize,

    control_queue: VecDeque<Packet>,

    peer_verification_tag: u32,
    my_verification_tag: u32,
    my_next_tsn: u32,
    peer_last_tsn: u32,
    // for RTT measurement
    min_tsn2measure_rtt: u32,
    will_send_forward_tsn: bool,
    will_retransmit_fast: bool,
    will_retransmit_reconfig: bool,

    will_send_shutdown_ack: bool,
    will_send_shutdown_complete: bool,

    // Reconfig
    my_next_rsn: u32,
    reconfigs: HashMap<u32, ChunkReconfig>,
    reconfig_requests: HashMap<u32, ParamOutgoingResetRequest>,

    // Non-RFC internal data
    source_port: u16,
    destination_port: u16,
    my_max_num_inbound_streams: u16,
    my_max_num_outbound_streams: u16,
    my_cookie: Option<ParamStateCookie>,

    //payload_queue: PayloadQueue,
    //inflight_queue: PayloadQueue,
    //pending_queue: Arc<PendingQueue>,
    //control_queue: ControlQueue,
    mtu: u32,
    // max DATA chunk payload size
    max_payload_size: u32,
    cumulative_tsn_ack_point: u32,
    advanced_peer_tsn_ack_point: u32,
    use_forward_tsn: bool,

    // Congestion control parameters
    max_receive_buffer_size: u32,
    // my congestion window size
    cwnd: u32,
    // calculated peer's receiver windows size
    rwnd: u32,
    // slow start threshold
    ssthresh: u32,
    partial_bytes_acked: u32,
    in_fast_recovery: bool,
    fast_recover_exit_point: u32,

    // Chunks stored for retransmission
    stored_init: Option<ChunkInit>,
    stored_cookie_echo: Option<ChunkCookieEcho>,

    //TODO: streams: HashMap<u16, Arc<Stream>>,

    // local error
    //TODO: silent_error: Option<Error>,

    // per inbound packet context
    delayed_ack_triggered: bool,
    immediate_ack_triggered: bool,

    stats: AssociationStats,
    ack_state: AckState,

    // for testing
    ack_mode: AckMode,
}

impl Association {
    pub(crate) fn new(
        server_config: Option<Arc<ServerConfig>>,
        config: Arc<TransportConfig>,
        init_cid: AssociationId,
        loc_cid: AssociationId,
        rem_cid: AssociationId,
        remote: SocketAddr,
        local_ip: Option<IpAddr>,
        //crypto: Box<dyn crypto::Session>,
        cid_gen: &dyn AssociationIdGenerator,
        now: Instant,
        //version: u32,
    ) -> Self {
        let side = if server_config.is_some() {
            Side::Server
        } else {
            Side::Client
        };

        let mtu = INITIAL_MTU;
        // RFC 4690 Sec 7.2.1
        //  o  The initial cwnd before DATA transmission or after a sufficiently
        //     long idle period MUST be set to min(4*MTU, max (2*MTU, 4380
        //     bytes)).
        let cwnd = std::cmp::min(4 * mtu, std::cmp::max(2 * mtu, 4380));
        let mut tsn = random::<u32>();
        if tsn == 0 {
            tsn += 1;
        }

        let mut this = Association {
            side,
            max_receive_buffer_size: config.max_receive_buffer_size,
            max_message_size: config.max_message_size,
            my_max_num_outbound_streams: config.max_num_outbound_streams,
            my_max_num_inbound_streams: config.max_num_inbound_streams,
            max_payload_size: INITIAL_MTU - (COMMON_HEADER_SIZE + DATA_CHUNK_HEADER_SIZE),

            mtu,
            cwnd,
            my_verification_tag: random::<u32>(),
            my_next_tsn: tsn,
            my_next_rsn: tsn,
            min_tsn2measure_rtt: tsn,
            cumulative_tsn_ack_point: tsn - 1,
            advanced_peer_tsn_ack_point: tsn - 1,
            //silent_error: Some(Error::ErrSilentlyDiscard),
            ..Default::default()
        };

        if side.is_client() {
            let mut init = ChunkInit {
                initial_tsn: this.my_next_tsn,
                num_outbound_streams: this.my_max_num_outbound_streams,
                num_inbound_streams: this.my_max_num_inbound_streams,
                initiate_tag: this.my_verification_tag,
                advertised_receiver_window_credit: this.max_receive_buffer_size,
                ..Default::default()
            };
            init.set_supported_extensions();

            this.set_state(AssociationState::CookieWait);
            this.stored_init = Some(init);
            let _ = this.send_init();
            /*TODO: let rto = ai.rto_mgr.get_rto();
            if let Some(t1init) = &ai.t1init {
                t1init.start(rto).await;
            }*/
        }

        this
    }

    /// bytes_sent returns the number of bytes sent
    pub(crate) fn bytes_sent(&self) -> usize {
        self.bytes_sent
    }

    /// bytes_received returns the number of bytes received
    pub(crate) fn bytes_received(&self) -> usize {
        self.bytes_received
    }

    /// max_message_size returns the maximum message size you can send.
    pub(crate) fn max_message_size(&self) -> u32 {
        self.max_message_size
    }

    /// set_max_message_size sets the maximum message size you can send.
    pub(crate) fn set_max_message_size(&mut self, max_message_size: u32) {
        self.max_message_size = max_message_size;
    }

    /// set_state atomically sets the state of the Association.
    fn set_state(&mut self, new_state: AssociationState) {
        if new_state != self.state {
            debug!(
                "[{}] state change: '{}' => '{}'",
                self.side, self.state, new_state,
            );
        }
        self.state = new_state;
    }

    /// get_state atomically returns the state of the Association.
    fn get_state(&self) -> AssociationState {
        self.state
    }

    /// caller must hold self.lock
    fn send_init(&mut self) -> Result<()> {
        if let Some(stored_init) = self.stored_init.take() {
            debug!("[{}] sending INIT", self.side);

            self.source_port = 5000; // Spec??
            self.destination_port = 5000; // Spec??

            let outbound = Packet {
                source_port: self.source_port,
                destination_port: self.destination_port,
                verification_tag: self.peer_verification_tag,
                chunks: vec![Box::new(stored_init)],
            };

            self.control_queue.push_back(outbound);
            //TODO:self.awake_write_loop();

            Ok(())
        } else {
            Err(Error::ErrInitNotStoredToSend)
        }
    }

    /// caller must hold self.lock
    fn send_cookie_echo(&mut self) -> Result<()> {
        if let Some(stored_cookie_echo) = &self.stored_cookie_echo {
            debug!("[{}] sending COOKIE-ECHO", self.side);

            let outbound = Packet {
                source_port: self.source_port,
                destination_port: self.destination_port,
                verification_tag: self.peer_verification_tag,
                chunks: vec![Box::new(stored_cookie_echo.clone())],
            };

            self.control_queue.push_back(outbound);
            //TODO:self.awake_write_loop();
            Ok(())
        } else {
            Err(Error::ErrCookieEchoNotStoredToSend)
        }
    }

    /// handle_inbound parses incoming raw packets
    fn handle_inbound(&mut self, raw: &Bytes) -> Result<()> {
        let p = match Packet::unmarshal(raw) {
            Ok(p) => p,
            Err(err) => {
                warn!("[{}] unable to parse SCTP packet {}", self.side, err);
                return Ok(());
            }
        };

        if let Err(err) = p.check_packet() {
            warn!("[{}] failed validating packet {}", self.side, err);
            return Ok(());
        }

        self.handle_chunk_start();

        for c in &p.chunks {
            self.handle_chunk(&p, c)?;
        }

        self.handle_chunk_end();

        Ok(())
    }

    fn handle_chunk_start(&mut self) {
        self.delayed_ack_triggered = false;
        self.immediate_ack_triggered = false;
    }

    fn handle_chunk_end(&mut self) {
        if self.immediate_ack_triggered {
            self.ack_state = AckState::Immediate;
            //TODO:if let Some(ack_timer) = &mut self.ack_timer {
            //    ack_timer.stop();
            //}
            //TODO:self.awake_write_loop();
        } else if self.delayed_ack_triggered {
            // Will send delayed ack in the next ack timeout
            self.ack_state = AckState::Delay;
            //TODO: if let Some(ack_timer) = &mut self.ack_timer {
            //    ack_timer.start();
            //}
        }
    }

    #[allow(clippy::borrowed_box)]
    fn handle_chunk(&mut self, p: &Packet, chunk: &Box<dyn Chunk + Send + Sync>) -> Result<()> {
        chunk.check()?;
        let chunk_any = chunk.as_any();
        let packets = if let Some(c) = chunk_any.downcast_ref::<ChunkInit>() {
            if c.is_ack {
                self.handle_init_ack(p, c)?
            } else {
                self.handle_init(p, c)?
            }
        } else if chunk_any.downcast_ref::<ChunkAbort>().is_some()
            || chunk_any.downcast_ref::<ChunkError>().is_some()
        {
            return Err(Error::ErrChunk);
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkHeartbeat>() {
            self.handle_heartbeat(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkCookieEcho>() {
            self.handle_cookie_echo(c)?
        } else if chunk_any.downcast_ref::<ChunkCookieAck>().is_some() {
            self.handle_cookie_ack()?
        }
        /*TODO: else if let Some(c) = chunk_any.downcast_ref::<ChunkPayloadData>() {
            self.handle_data(c)?
        }*/
        /* else if let Some(c) = chunk_any.downcast_ref::<ChunkSelectiveAck>() {
            self.handle_sack(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkReconfig>() {
            self.handle_reconfig(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkForwardTsn>() {
            self.handle_forward_tsn(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkShutdown>() {
            self.handle_shutdown(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkShutdownAck>() {
            self.handle_shutdown_ack(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkShutdownComplete>() {
            self.handle_shutdown_complete(c)?
        } */
        else {
            return Err(Error::ErrChunkTypeUnhandled);
        };

        if !packets.is_empty() {
            let mut buf: VecDeque<_> = packets.into_iter().collect();
            self.control_queue.append(&mut buf);
            //TODO: self.awake_write_loop();
        }

        Ok(())
    }

    fn handle_init(&mut self, p: &Packet, i: &ChunkInit) -> Result<Vec<Packet>> {
        let state = self.get_state();
        debug!("[{}] chunkInit received in state '{}'", self.side, state);

        // https://tools.ietf.org/html/rfc4960#section-5.2.1
        // Upon receipt of an INIT in the COOKIE-WAIT state, an endpoint MUST
        // respond with an INIT ACK using the same parameters it sent in its
        // original INIT chunk (including its Initiate Tag, unchanged).  When
        // responding, the endpoint MUST send the INIT ACK back to the same
        // address that the original INIT (sent by this endpoint) was sent.

        if state != AssociationState::Closed
            && state != AssociationState::CookieWait
            && state != AssociationState::CookieEchoed
        {
            // 5.2.2.  Unexpected INIT in States Other than CLOSED, COOKIE-ECHOED,
            //        COOKIE-WAIT, and SHUTDOWN-ACK-SENT
            return Err(Error::ErrHandleInitState);
        }

        // Should we be setting any of these permanently until we've ACKed further?
        self.my_max_num_inbound_streams =
            std::cmp::min(i.num_inbound_streams, self.my_max_num_inbound_streams);
        self.my_max_num_outbound_streams =
            std::cmp::min(i.num_outbound_streams, self.my_max_num_outbound_streams);
        self.peer_verification_tag = i.initiate_tag;
        self.source_port = p.destination_port;
        self.destination_port = p.source_port;

        // 13.2 This is the last TSN received in sequence.  This value
        // is set initially by taking the peer's initial TSN,
        // received in the INIT or INIT ACK chunk, and
        // subtracting one from it.
        self.peer_last_tsn = if i.initial_tsn == 0 {
            u32::MAX
        } else {
            i.initial_tsn - 1
        };

        for param in &i.params {
            if let Some(v) = param.as_any().downcast_ref::<ParamSupportedExtensions>() {
                for t in &v.chunk_types {
                    if *t == CT_FORWARD_TSN {
                        debug!("[{}] use ForwardTSN (on init)", self.side);
                        self.use_forward_tsn = true;
                    }
                }
            }
        }
        if !self.use_forward_tsn {
            warn!("[{}] not using ForwardTSN (on init)", self.side);
        }

        let mut outbound = Packet {
            verification_tag: self.peer_verification_tag,
            source_port: self.source_port,
            destination_port: self.destination_port,
            ..Default::default()
        };

        let mut init_ack = ChunkInit {
            is_ack: true,
            initial_tsn: self.my_next_tsn,
            num_outbound_streams: self.my_max_num_outbound_streams,
            num_inbound_streams: self.my_max_num_inbound_streams,
            initiate_tag: self.my_verification_tag,
            advertised_receiver_window_credit: self.max_receive_buffer_size,
            ..Default::default()
        };

        if self.my_cookie.is_none() {
            self.my_cookie = Some(ParamStateCookie::new());
        }

        if let Some(my_cookie) = &self.my_cookie {
            init_ack.params = vec![Box::new(my_cookie.clone())];
        }

        init_ack.set_supported_extensions();

        outbound.chunks = vec![Box::new(init_ack)];

        Ok(vec![outbound])
    }

    fn handle_init_ack(&mut self, p: &Packet, i: &ChunkInit) -> Result<Vec<Packet>> {
        let state = self.get_state();
        debug!("[{}] chunkInitAck received in state '{}'", self.side, state);
        if state != AssociationState::CookieWait {
            // RFC 4960
            // 5.2.3.  Unexpected INIT ACK
            //   If an INIT ACK is received by an endpoint in any state other than the
            //   COOKIE-WAIT state, the endpoint should discard the INIT ACK chunk.
            //   An unexpected INIT ACK usually indicates the processing of an old or
            //   duplicated INIT chunk.
            return Ok(vec![]);
        }

        self.my_max_num_inbound_streams =
            std::cmp::min(i.num_inbound_streams, self.my_max_num_inbound_streams);
        self.my_max_num_outbound_streams =
            std::cmp::min(i.num_outbound_streams, self.my_max_num_outbound_streams);
        self.peer_verification_tag = i.initiate_tag;
        self.peer_last_tsn = if i.initial_tsn == 0 {
            u32::MAX
        } else {
            i.initial_tsn - 1
        };
        if self.source_port != p.destination_port || self.destination_port != p.source_port {
            warn!("[{}] handle_init_ack: port mismatch", self.side);
            return Ok(vec![]);
        }

        self.rwnd = i.advertised_receiver_window_credit;
        debug!("[{}] initial rwnd={}", self.side, self.rwnd);

        // RFC 4690 Sec 7.2.1
        //  o  The initial value of ssthresh MAY be arbitrarily high (for
        //     example, implementations MAY use the size of the receiver
        //     advertised window).
        self.ssthresh = self.rwnd;
        /*TODO:trace!(
            "[{}] updated cwnd={} ssthresh={} inflight={} (INI)",
            self.side,
            self.cwnd,
            self.ssthresh,
            self.inflight_queue.get_num_bytes()
        );*/

        /*TODO:if let Some(t1init) = &self.t1init {
            t1init.stop().await;
        }*/
        self.stored_init = None;

        let mut cookie_param = None;
        for param in &i.params {
            if let Some(v) = param.as_any().downcast_ref::<ParamStateCookie>() {
                cookie_param = Some(v);
            } else if let Some(v) = param.as_any().downcast_ref::<ParamSupportedExtensions>() {
                for t in &v.chunk_types {
                    if *t == CT_FORWARD_TSN {
                        debug!("[{}] use ForwardTSN (on initAck)", self.side);
                        self.use_forward_tsn = true;
                    }
                }
            }
        }
        if !self.use_forward_tsn {
            warn!("[{}] not using ForwardTSN (on initAck)", self.side);
        }

        if let Some(v) = cookie_param {
            self.stored_cookie_echo = Some(ChunkCookieEcho {
                cookie: v.cookie.clone(),
            });

            self.send_cookie_echo()?;

            /*TODO: if let Some(t1cookie) = &self.t1cookie {
                t1cookie.start(self.rto_mgr.get_rto()).await;
            }*/

            self.set_state(AssociationState::CookieEchoed);

            Ok(vec![])
        } else {
            Err(Error::ErrInitAckNoCookie)
        }
    }

    fn handle_heartbeat(&self, c: &ChunkHeartbeat) -> Result<Vec<Packet>> {
        trace!("[{}] chunkHeartbeat", self.side);
        if let Some(p) = c.params.first() {
            if let Some(hbi) = p.as_any().downcast_ref::<ParamHeartbeatInfo>() {
                return Ok(vec![Packet {
                    verification_tag: self.peer_verification_tag,
                    source_port: self.source_port,
                    destination_port: self.destination_port,
                    chunks: vec![Box::new(ChunkHeartbeatAck {
                        params: vec![Box::new(ParamHeartbeatInfo {
                            heartbeat_information: hbi.heartbeat_information.clone(),
                        })],
                    })],
                }]);
            } else {
                warn!(
                    "[{}] failed to handle Heartbeat, no ParamHeartbeatInfo",
                    self.side,
                );
            }
        }

        Ok(vec![])
    }

    fn handle_cookie_echo(&mut self, c: &ChunkCookieEcho) -> Result<Vec<Packet>> {
        let state = self.get_state();
        debug!("[{}] COOKIE-ECHO received in state '{}'", self.side, state);

        if let Some(my_cookie) = &self.my_cookie {
            match state {
                AssociationState::Established => {
                    if my_cookie.cookie != c.cookie {
                        return Ok(vec![]);
                    }
                }
                AssociationState::Closed
                | AssociationState::CookieWait
                | AssociationState::CookieEchoed => {
                    if my_cookie.cookie != c.cookie {
                        return Ok(vec![]);
                    }

                    /*TODO: if let Some(t1init) = &self.t1init {
                        t1init.stop().await;
                    }*/
                    self.stored_init = None;

                    /*TODO: if let Some(t1cookie) = &self.t1cookie {
                        t1cookie.stop().await;
                    }*/
                    self.stored_cookie_echo = None;

                    self.set_state(AssociationState::Established);
                    /*TODO: if let Some(handshake_completed_ch) = &self.handshake_completed_ch_tx {
                        let _ = handshake_completed_ch.send(None).await;
                    }*/
                }
                _ => return Ok(vec![]),
            };
        } else {
            debug!("[{}] COOKIE-ECHO received before initialization", self.side);
            return Ok(vec![]);
        }

        Ok(vec![Packet {
            verification_tag: self.peer_verification_tag,
            source_port: self.source_port,
            destination_port: self.destination_port,
            chunks: vec![Box::new(ChunkCookieAck {})],
        }])
    }

    fn handle_cookie_ack(&mut self) -> Result<Vec<Packet>> {
        let state = self.get_state();
        debug!("[{}] COOKIE-ACK received in state '{}'", self.side, state);
        if state != AssociationState::CookieEchoed {
            // RFC 4960
            // 5.2.5.  Handle Duplicate COOKIE-ACK.
            //   At any state other than COOKIE-ECHOED, an endpoint should silently
            //   discard a received COOKIE ACK chunk.
            return Ok(vec![]);
        }

        /*TODO: if let Some(t1cookie) = &self.t1cookie {
            t1cookie.stop().await;
        }*/
        self.stored_cookie_echo = None;

        self.set_state(AssociationState::Established);
        /*TODO: if let Some(handshake_completed_ch) = &self.handshake_completed_ch_tx {
            let _ = handshake_completed_ch.send(None).await;
        }*/

        Ok(vec![])
    }

    /*TODO: fn handle_data(&mut self, d: &ChunkPayloadData) -> Result<Vec<Packet>> {
        trace!(
            "[{}] DATA: tsn={} immediateSack={} len={}",
            self.side,
            d.tsn,
            d.immediate_sack,
            d.user_data.len()
        );
        self.stats.inc_datas();

        let can_push = self.payload_queue.can_push(d, self.peer_last_tsn);
        let mut stream_handle_data = false;
        if can_push {
            if let Some(_s) = self.get_or_create_stream(d.stream_identifier) {
                if self.get_my_receiver_window_credit() > 0 {
                    // Pass the new chunk to stream level as soon as it arrives
                    self.payload_queue.push(d.clone(), self.peer_last_tsn);
                    stream_handle_data = true;
                } else {
                    // Receive buffer is full
                    if let Some(last_tsn) = self.payload_queue.get_last_tsn_received() {
                        if sna32lt(d.tsn, *last_tsn) {
                            debug!("[{}] receive buffer full, but accepted as this is a missing chunk with tsn={} ssn={}", self.side, d.tsn, d.stream_sequence_number);
                            self.payload_queue.push(d.clone(), self.peer_last_tsn);
                            stream_handle_data = true; //s.handle_data(d.clone());
                        }
                    } else {
                        debug!(
                            "[{}] receive buffer full. dropping DATA with tsn={} ssn={}",
                            self.side, d.tsn, d.stream_sequence_number
                        );
                    }
                }
            } else {
                // silently discard the data. (sender will retry on T3-rtx timeout)
                // see pion/sctp#30
                debug!("[{}] discard {}", self.side, d.stream_sequence_number);
                return Ok(vec![]);
            }
        }

        let immediate_sack = d.immediate_sack;

        if stream_handle_data {
            if let Some(s) = self.streams.get_mut(&d.stream_identifier) {
                s.handle_data(d.clone());
            }
        }

        self.handle_peer_last_tsn_and_acknowledgement(immediate_sack)
    }

    /// A common routine for handle_data and handle_forward_tsn routines
    fn handle_peer_last_tsn_and_acknowledgement(
        &mut self,
        sack_immediately: bool,
    ) -> Result<Vec<Packet>> {
        let mut reply = vec![];

        // Try to advance peer_last_tsn

        // From RFC 3758 Sec 3.6:
        //   .. and then MUST further advance its cumulative TSN point locally
        //   if possible
        // Meaning, if peer_last_tsn+1 points to a chunk that is received,
        // advance peer_last_tsn until peer_last_tsn+1 points to unreceived chunk.
        debug!("[{}] peer_last_tsn = {}", self.side, self.peer_last_tsn);
        while self.payload_queue.pop(self.peer_last_tsn + 1).is_some() {
            self.peer_last_tsn += 1;
            debug!("[{}] peer_last_tsn = {}", self.side, self.peer_last_tsn);

            let rst_reqs: Vec<ParamOutgoingResetRequest> =
                self.reconfig_requests.values().cloned().collect();
            for rst_req in rst_reqs {
                let resp = self.reset_streams_if_any(&rst_req);
                debug!("[{}] RESET RESPONSE: {}", self.side, resp);
                reply.push(resp);
            }
        }

        let has_packet_loss = !self.payload_queue.is_empty();
        if has_packet_loss {
            trace!(
                "[{}] packetloss: {}",
                self.side,
                self.payload_queue
                    .get_gap_ack_blocks_string(self.peer_last_tsn)
            );
        }

        if (self.ack_state != AckState::Immediate
            && !sack_immediately
            && !has_packet_loss
            && self.ack_mode == AckMode::Normal)
            || self.ack_mode == AckMode::AlwaysDelay
        {
            if self.ack_state == AckState::Idle {
                self.delayed_ack_triggered = true;
            } else {
                self.immediate_ack_triggered = true;
            }
        } else {
            self.immediate_ack_triggered = true;
        }

        Ok(reply)
    }*/

    fn handle_shutdown(&mut self, _: &ChunkShutdown) -> Result<Vec<Packet>> {
        let state = self.get_state();

        if state == AssociationState::Established {
            if !self.inflight_queue.is_empty() {
                self.set_state(AssociationState::ShutdownReceived);
            } else {
                // No more outstanding, send shutdown ack.
                self.will_send_shutdown_ack = true;
                self.set_state(AssociationState::ShutdownAckSent);

                //TODO: self.awake_write_loop();
            }
        } else if state == AssociationState::ShutdownSent {
            // self.cumulative_tsn_ack_point = c.cumulative_tsn_ack

            self.will_send_shutdown_ack = true;
            self.set_state(AssociationState::ShutdownAckSent);

            //TODO: self.awake_write_loop();
        }

        Ok(vec![])
    }

    fn handle_shutdown_ack(&mut self, _: &ChunkShutdownAck) -> Result<Vec<Packet>> {
        let state = self.get_state();
        if state == AssociationState::ShutdownSent || state == AssociationState::ShutdownAckSent {
            /*TODO: if let Some(t2shutdown) = &self.t2shutdown {
                t2shutdown.stop().await;
            }*/
            self.will_send_shutdown_complete = true;

            //TODO: self.awake_write_loop();
        }

        Ok(vec![])
    }

    fn handle_shutdown_complete(&mut self, _: &ChunkShutdownComplete) -> Result<Vec<Packet>> {
        let state = self.get_state();
        if state == AssociationState::ShutdownAckSent {
            /*TODO: if let Some(t2shutdown) = &self.t2shutdown {
                t2shutdown.stop().await;
            }*/
            //TODO: self.close().await?;
        }

        Ok(vec![])
    }
}
