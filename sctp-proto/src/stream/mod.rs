#[cfg(test)]
mod stream_test;

use crate::association::state::AssociationState;
use crate::association::Association;
use crate::chunk::chunk_payload_data::{ChunkPayloadData, PayloadProtocolIdentifier};
use crate::error::{Error, Result};
use crate::queue::reassembly_queue::{Chunks, ReassemblyQueue};
use crate::Side;

use bytes::Bytes;
use std::fmt;
use tracing::{debug, error, trace};

/// Application events about streams
#[derive(Debug, PartialEq, Eq)]
pub enum StreamEvent {
    /// One or more new streams has been opened
    Opened,
    /// A currently open stream has data or errors waiting to be read
    Readable {
        /// Which stream is now readable
        id: u16,
    },
    /// A formerly write-blocked stream might be ready for a write or have been stopped
    ///
    /// Only generated for streams that are currently open.
    Writable {
        /// Which stream is now writable
        id: u16,
    },
    /// A finished stream has been fully acknowledged or stopped
    Finished {
        /// Which stream has been finished
        id: u16,
    },
    /// The peer asked us to stop sending on an outgoing stream
    Stopped {
        /// Which stream has been stopped
        id: u16,
        /// Error code supplied by the peer
        error_code: u16,
    },
    /// At least one new stream of a certain directionality may be opened
    Available,
}

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
pub struct Stream<'a> {
    //TODO: pub(crate) on_buffered_amount_low: Mutex<Option<OnBufferedAmountLowFn>>,
    pub(crate) stream_identifier: u16,
    pub(crate) association: &'a mut Association,
}

impl<'a> Stream<'a> {
    /// read reads a packet of len(p) bytes, dropping the Payload Protocol Identifier.
    /// Returns EOF when the stream is reset or an error if the stream is closed
    /// otherwise.
    pub fn read(&mut self) -> Result<Option<Chunks>> {
        self.read_sctp()
    }

    /// read_sctp reads a packet of len(p) bytes and returns the associated Payload
    /// Protocol Identifier.
    /// Returns EOF when the stream is reset or an error if the stream is closed
    /// otherwise.
    pub fn read_sctp(&mut self) -> Result<Option<Chunks>> {
        if let Some(s) = self.association.streams.get_mut(&self.stream_identifier) {
            if !s.closed {
                return Ok(s.reassembly_queue.read());
            }
        }

        Err(Error::ErrStreamClosed)
    }

    /// write writes len(p) bytes from p with the default Payload Protocol Identifier
    pub fn write(&mut self, p: &Bytes) -> Result<usize> {
        let default_payload_type =
            if let Some(s) = self.association.streams.get(&self.stream_identifier) {
                s.default_payload_type
            } else {
                return Err(Error::ErrStreamClosed);
            };
        self.write_sctp(p, default_payload_type)
    }

    /// write_sctp writes len(p) bytes from p to the DTLS connection
    pub fn write_sctp(&mut self, p: &Bytes, ppi: PayloadProtocolIdentifier) -> Result<usize> {
        if p.len() > self.association.max_message_size() as usize {
            return Err(Error::ErrOutboundPacketTooLarge);
        }

        let state: AssociationState = self.association.state();
        match state {
            AssociationState::ShutdownSent
            | AssociationState::ShutdownAckSent
            | AssociationState::ShutdownPending
            | AssociationState::ShutdownReceived => return Err(Error::ErrStreamClosed),
            _ => {}
        };

        if let Some(s) = self.association.streams.get_mut(&self.stream_identifier) {
            let chunks = s.packetize(p, ppi);
            self.association.send_payload_data(chunks)?;

            Ok(p.len())
        } else {
            Err(Error::ErrStreamClosed)
        }
    }

    /// Close closes the write-direction of the stream.
    /// Future calls to write are not permitted after calling Close.
    pub fn close(&mut self) -> Result<()> {
        let mut reset = false;
        if let Some(s) = self.association.streams.get_mut(&self.stream_identifier) {
            if !s.closed {
                reset = true;
            }
            s.closed = true;
        }

        if reset {
            // Reset the outgoing stream
            // https://tools.ietf.org/html/rfc6525
            self.association
                .send_reset_request(self.stream_identifier)?;
        }

        Ok(())
    }
}

impl<'a> std::ops::Deref for Stream<'a> {
    type Target = StreamState;
    fn deref(&self) -> &StreamState {
        self.association
            .streams
            .get(&self.stream_identifier)
            .unwrap()
    }
}

impl<'a> std::ops::DerefMut for Stream<'a> {
    fn deref_mut(&mut self) -> &mut StreamState {
        self.association
            .streams
            .get_mut(&self.stream_identifier)
            .unwrap()
    }
}

/// Stream represents an SCTP stream
#[derive(Default, Debug)]
pub struct StreamState {
    pub(crate) side: Side,
    pub(crate) max_payload_size: u32,
    pub(crate) stream_identifier: u16,
    pub(crate) default_payload_type: PayloadProtocolIdentifier,
    pub(crate) reassembly_queue: ReassemblyQueue,
    pub(crate) sequence_number: u16,
    pub(crate) closed: bool,
    pub(crate) unordered: bool,
    pub(crate) reliability_type: ReliabilityType,
    pub(crate) reliability_value: u32,
    pub(crate) buffered_amount: usize,
    pub(crate) buffered_amount_low: usize,
}
impl StreamState {
    pub(crate) fn new(
        side: Side,
        stream_identifier: u16,
        max_payload_size: u32,
        default_payload_type: PayloadProtocolIdentifier,
    ) -> Self {
        StreamState {
            side,
            stream_identifier,
            max_payload_size,
            default_payload_type,
            reassembly_queue: ReassemblyQueue::new(stream_identifier),
            sequence_number: 0,
            closed: false,
            unordered: false,
            reliability_type: ReliabilityType::Reliable,
            reliability_value: 0,
            buffered_amount: 0,
            buffered_amount_low: 0,
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

    pub(crate) fn handle_data(&mut self, pd: &ChunkPayloadData) {
        self.reassembly_queue.push(pd.clone());
    }

    pub(crate) fn handle_forward_tsn_for_ordered(&mut self, ssn: u16) {
        if self.unordered {
            return; // unordered chunks are handled by handleForwardUnordered method
        }

        // Remove all chunks older than or equal to the new TSN from
        // the reassembly_queue.
        self.reassembly_queue.forward_tsn_for_ordered(ssn);
    }

    pub(crate) fn handle_forward_tsn_for_unordered(&mut self, new_cumulative_tsn: u32) {
        if !self.unordered {
            return; // ordered chunks are handled by handleForwardTSNOrdered method
        }

        // Remove all chunks older than or equal to the new TSN from
        // the reassembly_queue.
        self.reassembly_queue
            .forward_tsn_for_unordered(new_cumulative_tsn);
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

    /*TODO:/// on_buffered_amount_low sets the callback handler which would be called when the number of
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
}
