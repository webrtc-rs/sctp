#[cfg(test)]
mod stream_test;

use crate::association::state::AssociationState;
use crate::chunk::chunk_payload_data::{ChunkPayloadData, PayloadProtocolIdentifier};
use crate::error::{Error, Result};
use crate::queue::pending_queue::PendingQueue;
use crate::queue::reassembly_queue::ReassemblyQueue;
use crate::Side;

use bytes::Bytes;
use std::fmt;
use tracing::{debug, error, trace};

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum ReliabilityType {
    /// ReliabilityTypeReliable is used for reliable transmission
    Reliable = 0,
    /// ReliabilityTypeRexmit is used for partial reliability by retransmission count
    Rexmit = 1,
    /// ReliabilityTypeTimed is used for partial reliability by retransmission duration
    Timed = 2,
}

impl Default for ReliabilityType {
    fn default() -> Self {
        ReliabilityType::Reliable
    }
}

impl fmt::Display for ReliabilityType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            ReliabilityType::Reliable => "Reliable",
            ReliabilityType::Rexmit => "Rexmit",
            ReliabilityType::Timed => "Timed",
        };
        write!(f, "{}", s)
    }
}

impl From<u8> for ReliabilityType {
    fn from(v: u8) -> ReliabilityType {
        match v {
            1 => ReliabilityType::Rexmit,
            2 => ReliabilityType::Timed,
            _ => ReliabilityType::Reliable,
        }
    }
}

//TODO: pub type OnBufferedAmountLowFn = Box<dyn (FnMut() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + Sync>;

/// Stream represents an SCTP stream
#[derive(Default, Debug)]
pub struct Stream {
    pub(crate) max_message_size: u32,   //TODO: clone from association
    pub(crate) state: AssociationState, //TODO: clone from association
    //TODO: pub(crate) awake_write_loop_ch: Option<Arc<mpsc::Sender<()>>>,
    pub(crate) pending_queue: PendingQueue,

    pub(crate) side: Side,
    pub(crate) max_payload_size: u32,
    pub(crate) stream_identifier: u16,
    pub(crate) default_payload_type: PayloadProtocolIdentifier,
    pub(crate) reassembly_queue: ReassemblyQueue,
    pub(crate) sequence_number: u16,
    //TODO: pub(crate) read_notifier: Notify,
    pub(crate) closed: bool,
    pub(crate) unordered: bool,
    pub(crate) reliability_type: ReliabilityType,
    pub(crate) reliability_value: u32,
    pub(crate) buffered_amount: usize,
    pub(crate) buffered_amount_low: usize,
    //TODO: pub(crate) on_buffered_amount_low: Mutex<Option<OnBufferedAmountLowFn>>,
}

impl Stream {
    pub(crate) fn new(
        side: Side,
        stream_identifier: u16,
        max_payload_size: u32,
        max_message_size: u32,
        state: AssociationState,
        //TODO: awake_write_loop_ch: Option<Arc<mpsc::Sender<()>>>,
        pending_queue: PendingQueue,
    ) -> Self {
        Stream {
            side,
            max_payload_size,
            max_message_size,
            state,
            //awake_write_loop_ch,
            pending_queue,
            stream_identifier,
            default_payload_type: PayloadProtocolIdentifier::Unknown,
            reassembly_queue: ReassemblyQueue::new(stream_identifier),
            sequence_number: 0,
            //TODO: read_notifier: Notify::new(),
            closed: false,
            unordered: false,
            reliability_type: ReliabilityType::Reliable,
            reliability_value: 0,
            buffered_amount: 0,
            buffered_amount_low: 0,
            //TODO: on_buffered_amount_low: Mutex::new(None),
        }
    }

    /// stream_identifier returns the Stream identifier associated to the stream.
    pub fn stream_identifier(&self) -> u16 {
        self.stream_identifier
    }

    /// set_default_payload_type sets the default payload type used by write.
    pub fn set_default_payload_type(&mut self, default_payload_type: PayloadProtocolIdentifier) {
        self.default_payload_type = default_payload_type;
    }

    /// set_reliability_params sets reliability parameters for this stream.
    pub fn set_reliability_params(
        &mut self,
        unordered: bool,
        rel_type: ReliabilityType,
        rel_val: u32,
    ) {
        debug!(
            "[{}] reliability params: ordered={} type={} value={}",
            self.side, !unordered, rel_type, rel_val
        );
        self.unordered = unordered;
        self.reliability_type = rel_type;
        self.reliability_value = rel_val;
    }

    /// read reads a packet of len(p) bytes, dropping the Payload Protocol Identifier.
    /// Returns EOF when the stream is reset or an error if the stream is closed
    /// otherwise.
    pub fn read(&mut self, p: &mut [u8]) -> Result<usize> {
        let (n, _) = self.read_sctp(p)?;
        Ok(n)
    }

    /// read_sctp reads a packet of len(p) bytes and returns the associated Payload
    /// Protocol Identifier.
    /// Returns EOF when the stream is reset or an error if the stream is closed
    /// otherwise.
    pub fn read_sctp(&mut self, p: &mut [u8]) -> Result<(usize, PayloadProtocolIdentifier)> {
        while !self.closed {
            let result = self.reassembly_queue.read(p);

            if result.is_ok() {
                return result;
            } else if let Err(err) = result {
                if Error::ErrShortBuffer == err {
                    return Err(err);
                }
            }

            //TODO:self.read_notifier.notified().await;
        }

        Err(Error::ErrStreamClosed)
    }

    pub(crate) fn handle_data(&mut self, pd: ChunkPayloadData) {
        let readable = {
            if self.reassembly_queue.push(pd) {
                let readable = self.reassembly_queue.is_readable();
                debug!("[{}] reassemblyQueue readable={}", self.side, readable);
                readable
            } else {
                false
            }
        };

        if readable {
            debug!("[{}] readNotifier.signal()", self.side);
            //TODO: self.read_notifier.notify_one();
            debug!("[{}] readNotifier.signal() done", self.side);
        }
    }

    pub(crate) fn handle_forward_tsn_for_ordered(&mut self, ssn: u16) {
        if self.unordered {
            return; // unordered chunks are handled by handleForwardUnordered method
        }

        // Remove all chunks older than or equal to the new TSN from
        // the reassembly_queue.
        let _readable = {
            self.reassembly_queue.forward_tsn_for_ordered(ssn);
            self.reassembly_queue.is_readable()
        };

        // Notify the reader asynchronously if there's a data chunk to read.
        /*TODO:if readable {
            self.read_notifier.notify_one();
        }*/
    }

    pub(crate) fn handle_forward_tsn_for_unordered(&mut self, new_cumulative_tsn: u32) {
        if !self.unordered {
            return; // ordered chunks are handled by handleForwardTSNOrdered method
        }

        // Remove all chunks older than or equal to the new TSN from
        // the reassembly_queue.
        let _readable = {
            self.reassembly_queue
                .forward_tsn_for_unordered(new_cumulative_tsn);
            self.reassembly_queue.is_readable()
        };

        // Notify the reader asynchronously if there's a data chunk to read.
        /*TODO:if readable {
            self.read_notifier.notify_one();
        }*/
    }

    /// write writes len(p) bytes from p with the default Payload Protocol Identifier
    pub fn write(&mut self, p: &Bytes) -> Result<usize> {
        self.write_sctp(p, self.default_payload_type)
    }

    /// write_sctp writes len(p) bytes from p to the DTLS connection
    pub fn write_sctp(&mut self, p: &Bytes, ppi: PayloadProtocolIdentifier) -> Result<usize> {
        if p.len() > self.max_message_size as usize {
            return Err(Error::ErrOutboundPacketTooLarge);
        }

        let state: AssociationState = self.state;
        match state {
            AssociationState::ShutdownSent
            | AssociationState::ShutdownAckSent
            | AssociationState::ShutdownPending
            | AssociationState::ShutdownReceived => return Err(Error::ErrStreamClosed),
            _ => {}
        };

        let chunks = self.packetize(p, ppi);
        self.send_payload_data(chunks)?;

        Ok(p.len())
    }

    fn packetize(&mut self, raw: &Bytes, ppi: PayloadProtocolIdentifier) -> Vec<ChunkPayloadData> {
        let mut i = 0;
        let mut remaining = raw.len();

        // From draft-ietf-rtcweb-data-protocol-09, section 6:
        //   All Data Channel Establishment Protocol messages MUST be sent using
        //   ordered delivery and reliable transmission.
        let unordered = ppi != PayloadProtocolIdentifier::Dcep && self.unordered;

        let mut chunks = vec![];

        let head_abandoned = false;
        let head_all_inflight = false;
        while remaining != 0 {
            let fragment_size = std::cmp::min(self.max_payload_size as usize, remaining); //self.association.max_payload_size

            // Copy the userdata since we'll have to store it until acked
            // and the caller may re-use the buffer in the mean time
            let user_data = raw.slice(i..i + fragment_size);

            let chunk = ChunkPayloadData {
                stream_identifier: self.stream_identifier,
                user_data,
                unordered,
                beginning_fragment: i == 0,
                ending_fragment: remaining - fragment_size == 0,
                immediate_sack: false,
                payload_type: ppi,
                stream_sequence_number: self.sequence_number,
                abandoned: head_abandoned, // all fragmented chunks use the same abandoned
                all_inflight: head_all_inflight, // all fragmented chunks use the same all_inflight
                ..Default::default()
            };

            chunks.push(chunk);

            remaining -= fragment_size;
            i += fragment_size;
        }

        // RFC 4960 Sec 6.6
        // Note: When transmitting ordered and unordered data, an endpoint does
        // not increment its Stream Sequence Number when transmitting a DATA
        // chunk with U flag set to 1.
        if !unordered {
            self.sequence_number += 1;
        }

        let old_value = self.buffered_amount;
        self.buffered_amount += raw.len();
        trace!("[{}] bufferedAmount = {}", self.side, old_value + raw.len());

        chunks
    }

    /// Close closes the write-direction of the stream.
    /// Future calls to write are not permitted after calling Close.
    pub fn close(&mut self) -> Result<()> {
        if !self.closed {
            // Reset the outgoing stream
            // https://tools.ietf.org/html/rfc6525
            self.send_reset_request(self.stream_identifier)?;
        }
        self.closed = true;
        //TODO: self.read_notifier.notify_waiters(); // broadcast regardless

        Ok(())
    }

    /// buffered_amount returns the number of bytes of data currently queued to be sent over this stream.
    pub fn buffered_amount(&self) -> usize {
        self.buffered_amount
    }

    /// buffered_amount_low_threshold returns the number of bytes of buffered outgoing data that is
    /// considered "low." Defaults to 0.
    pub fn buffered_amount_low_threshold(&self) -> usize {
        self.buffered_amount_low
    }

    /// set_buffered_amount_low_threshold is used to update the threshold.
    /// See buffered_amount_low_threshold().
    pub fn set_buffered_amount_low_threshold(&mut self, th: usize) {
        self.buffered_amount_low = th;
    }

    /*TODO:
    /// on_buffered_amount_low sets the callback handler which would be called when the number of
    /// bytes of outgoing data buffered is lower than the threshold.
    pub fn on_buffered_amount_low(&self, f: OnBufferedAmountLowFn) {
        let mut on_buffered_amount_low = self.on_buffered_amount_low.lock().await;
        *on_buffered_amount_low = Some(f);
    }*/

    /// This method is called by association's read_loop (go-)routine to notify this stream
    /// of the specified amount of outgoing data has been delivered to the peer.
    pub(crate) fn on_buffer_released(&mut self, n_bytes_released: i64) {
        if n_bytes_released <= 0 {
            return;
        }

        let from_amount = self.buffered_amount;
        let new_amount = if from_amount < n_bytes_released as usize {
            self.buffered_amount = 0;
            error!(
                "[{}] released buffer size {} should be <= {}",
                self.side, n_bytes_released, 0,
            );
            0
        } else {
            self.buffered_amount -= n_bytes_released as usize;

            from_amount - n_bytes_released as usize
        };

        let buffered_amount_low = self.buffered_amount_low;

        trace!(
            "[{}] bufferedAmount = {}, from_amount = {}, buffered_amount_low = {}",
            self.side,
            new_amount,
            from_amount,
            buffered_amount_low,
        );

        if from_amount > buffered_amount_low && new_amount <= buffered_amount_low {
            /*TODO:let mut handler = self.on_buffered_amount_low.lock().await;
            if let Some(f) = &mut *handler {
                f().await;
            }*/
        }
    }

    pub(crate) fn get_num_bytes_in_reassembly_queue(&self) -> usize {
        // No lock is required as it reads the size with atomic load function.
        self.reassembly_queue.get_num_bytes()
    }

    /// get_state atomically returns the state of the Association.
    fn get_state(&self) -> AssociationState {
        self.state
    }

    fn awake_write_loop(&self) {
        //TODO: debug!("[{}] awake_write_loop_ch.notify_one", self.name);
        //if let Some(awake_write_loop_ch) = &self.awake_write_loop_ch {
        //    let _ = awake_write_loop_ch.try_send(());
        //}
    }

    fn send_payload_data(&mut self, chunks: Vec<ChunkPayloadData>) -> Result<()> {
        let state = self.get_state();
        if state != AssociationState::Established {
            return Err(Error::ErrPayloadDataStateNotExist);
        }

        // Push the chunks into the pending queue first.
        for c in chunks {
            self.pending_queue.push(c);
        }

        self.awake_write_loop();
        Ok(())
    }

    fn send_reset_request(&mut self, stream_identifier: u16) -> Result<()> {
        let state = self.get_state();
        if state != AssociationState::Established {
            return Err(Error::ErrResetPacketInStateNotExist);
        }

        // Create DATA chunk which only contains valid stream identifier with
        // nil userData and use it as a EOS from the stream.
        let c = ChunkPayloadData {
            stream_identifier,
            beginning_fragment: true,
            ending_fragment: true,
            user_data: Bytes::new(),
            ..Default::default()
        };

        self.pending_queue.push(c);

        self.awake_write_loop();
        Ok(())
    }
}
