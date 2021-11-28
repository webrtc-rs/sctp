use crate::association::{
    state::{AckMode, AckState, AssociationState},
    stats::AssociationStats,
};
use crate::chunk::{
    chunk_cookie_echo::ChunkCookieEcho, chunk_init::ChunkInit, chunk_reconfig::ChunkReconfig,
};
use crate::config::{
    ServerConfig, TransportConfig, COMMON_HEADER_SIZE, DATA_CHUNK_HEADER_SIZE, INITIAL_MTU,
};
use crate::error::{Error, Result};
use crate::packet::Packet;
use crate::param::{
    param_outgoing_reset_request::ParamOutgoingResetRequest, param_state_cookie::ParamStateCookie,
};
use crate::shared::AssociationId;
use crate::util::AssociationIdGenerator;
use crate::Side;

use rand::random;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;
use tracing::debug;
//{debug, trace, warn};

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
}
