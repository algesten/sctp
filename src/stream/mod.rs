#[cfg(test)]
mod stream_test;

use crate::association::AssociationState;
use crate::chunk::chunk_payload_data::{ChunkPayloadData, PayloadProtocolIdentifier};
use crate::error::Error;
use crate::queue::reassembly_queue::ReassemblyQueue;

use crate::queue::pending_queue::PendingQueue;
use anyhow::Result;
use bytes::Bytes;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Notify};

#[derive(Debug, Copy, Clone, PartialEq)]
#[repr(C)]
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

pub type OnBufferedAmountLowFn = Box<dyn Fn() + Send + Sync>;

// TODO: benchmark performance between multiple Atomic+Mutex vs one Mutex<StreamInternal>

/// Stream represents an SCTP stream
#[derive(Default)]
pub struct Stream {
    pub(crate) max_payload_size: u32,
    pub(crate) max_message_size: Arc<AtomicU32>, // clone from association
    pub(crate) state: Arc<AtomicU8>,             // clone from association
    pub(crate) awake_write_loop_ch: Option<Arc<mpsc::Sender<()>>>,
    pub(crate) pending_queue: Arc<PendingQueue>,

    pub(crate) stream_identifier: u16,
    pub(crate) default_payload_type: AtomicU32, //PayloadProtocolIdentifier,
    pub(crate) reassembly_queue: Mutex<ReassemblyQueue>,
    pub(crate) sequence_number: AtomicU16,
    pub(crate) read_notifier: Notify,
    pub(crate) closed: AtomicBool,
    pub(crate) unordered: AtomicBool,
    pub(crate) reliability_type: AtomicU8, //ReliabilityType,
    pub(crate) reliability_value: AtomicU32,
    pub(crate) buffered_amount: AtomicUsize,
    pub(crate) buffered_amount_low: AtomicUsize,
    pub(crate) on_buffered_amount_low: Mutex<Option<OnBufferedAmountLowFn>>,
    pub(crate) name: String,
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stream")
            .field("max_payload_size", &self.max_payload_size)
            .field("max_message_size", &self.max_message_size)
            .field("state", &self.state)
            .field("awake_write_loop_ch", &self.awake_write_loop_ch)
            .field("stream_identifier", &self.stream_identifier)
            .field("default_payload_type", &self.default_payload_type)
            .field("reassembly_queue", &self.reassembly_queue)
            .field("sequence_number", &self.sequence_number)
            .field("closed", &self.closed)
            .field("unordered", &self.unordered)
            .field("reliability_type", &self.reliability_type)
            .field("reliability_value", &self.reliability_value)
            .field("buffered_amount", &self.buffered_amount)
            .field("buffered_amount_low", &self.buffered_amount_low)
            .field("name", &self.name)
            .finish()
    }
}

impl Stream {
    pub(crate) fn new(
        name: String,
        stream_identifier: u16,
        max_payload_size: u32,
        max_message_size: Arc<AtomicU32>,
        state: Arc<AtomicU8>,
        awake_write_loop_ch: Option<Arc<mpsc::Sender<()>>>,
        pending_queue: Arc<PendingQueue>,
    ) -> Self {
        Stream {
            max_payload_size,
            max_message_size,
            state,
            awake_write_loop_ch,
            pending_queue,

            stream_identifier,
            default_payload_type: AtomicU32::new(0), //PayloadProtocolIdentifier::Unknown,
            reassembly_queue: Mutex::new(ReassemblyQueue::new(stream_identifier)),
            sequence_number: AtomicU16::new(0),
            read_notifier: Notify::new(),
            closed: AtomicBool::new(false),
            unordered: AtomicBool::new(false),
            reliability_type: AtomicU8::new(0), //ReliabilityType::Reliable,
            reliability_value: AtomicU32::new(0),
            buffered_amount: AtomicUsize::new(0),
            buffered_amount_low: AtomicUsize::new(0),
            on_buffered_amount_low: Mutex::new(None),
            name,
        }
    }

    /// stream_identifier returns the Stream identifier associated to the stream.
    pub fn stream_identifier(&self) -> u16 {
        self.stream_identifier
    }

    /// set_default_payload_type sets the default payload type used by write.
    pub fn set_default_payload_type(&self, default_payload_type: PayloadProtocolIdentifier) {
        self.default_payload_type
            .store(default_payload_type as u32, Ordering::SeqCst);
    }

    /// set_reliability_params sets reliability parameters for this stream.
    pub fn set_reliability_params(&self, unordered: bool, rel_type: ReliabilityType, rel_val: u32) {
        log::debug!(
            "[{}] reliability params: ordered={} type={} value={}",
            self.name,
            !unordered,
            rel_type,
            rel_val
        );
        self.unordered.store(unordered, Ordering::SeqCst);
        self.reliability_type
            .store(rel_type as u8, Ordering::SeqCst);
        self.reliability_value.store(rel_val, Ordering::SeqCst);
    }

    /// read reads a packet of len(p) bytes, dropping the Payload Protocol Identifier.
    /// Returns EOF when the stream is reset or an error if the stream is closed
    /// otherwise.
    pub async fn read(&self, p: &mut [u8]) -> Result<usize> {
        let (n, _) = self.read_sctp(p).await?;
        Ok(n)
    }

    /// read_sctp reads a packet of len(p) bytes and returns the associated Payload
    /// Protocol Identifier.
    /// Returns EOF when the stream is reset or an error if the stream is closed
    /// otherwise.
    pub async fn read_sctp(&self, p: &mut [u8]) -> Result<(usize, PayloadProtocolIdentifier)> {
        while !self.closed.load(Ordering::SeqCst) {
            let result = {
                let mut reassembly_queue = self.reassembly_queue.lock().await;
                reassembly_queue.read(p)
            };

            if result.is_ok() {
                return result;
            } else if let Err(err) = result {
                if Error::ErrShortBuffer.equal(&err) {
                    return Err(err);
                }
            }

            self.read_notifier.notified().await;
        }

        Err(Error::ErrStreamClosed.into())
    }

    pub(crate) async fn handle_data(&self, pd: ChunkPayloadData) {
        let readable = {
            let mut reassembly_queue = self.reassembly_queue.lock().await;
            if reassembly_queue.push(pd) {
                let readable = reassembly_queue.is_readable();
                log::debug!("[{}] reassemblyQueue readable={}", self.name, readable);
                readable
            } else {
                false
            }
        };

        if readable {
            log::debug!("[{}] readNotifier.signal()", self.name);
            self.read_notifier.notify_one();
            log::debug!("[{}] readNotifier.signal() done", self.name);
        }
    }

    pub(crate) async fn handle_forward_tsn_for_ordered(&self, ssn: u16) {
        if self.unordered.load(Ordering::SeqCst) {
            return; // unordered chunks are handled by handleForwardUnordered method
        }

        // Remove all chunks older than or equal to the new TSN from
        // the reassembly_queue.
        let readable = {
            let mut reassembly_queue = self.reassembly_queue.lock().await;
            reassembly_queue.forward_tsn_for_ordered(ssn);
            reassembly_queue.is_readable()
        };

        // Notify the reader asynchronously if there's a data chunk to read.
        if readable {
            self.read_notifier.notify_one();
        }
    }

    pub(crate) async fn handle_forward_tsn_for_unordered(&self, new_cumulative_tsn: u32) {
        if !self.unordered.load(Ordering::SeqCst) {
            return; // ordered chunks are handled by handleForwardTSNOrdered method
        }

        // Remove all chunks older than or equal to the new TSN from
        // the reassembly_queue.
        let readable = {
            let mut reassembly_queue = self.reassembly_queue.lock().await;
            reassembly_queue.forward_tsn_for_unordered(new_cumulative_tsn);
            reassembly_queue.is_readable()
        };

        // Notify the reader asynchronously if there's a data chunk to read.
        if readable {
            self.read_notifier.notify_one();
        }
    }

    /// write writes len(p) bytes from p with the default Payload Protocol Identifier
    pub async fn write(&self, p: &Bytes) -> Result<usize> {
        self.write_sctp(p, self.default_payload_type.load(Ordering::SeqCst).into())
            .await
    }

    /// write_sctp writes len(p) bytes from p to the DTLS connection
    pub async fn write_sctp(&self, p: &Bytes, ppi: PayloadProtocolIdentifier) -> Result<usize> {
        if p.len() > self.max_message_size.load(Ordering::SeqCst) as usize {
            return Err(Error::ErrOutboundPacketTooLarge.into());
        }

        let state: AssociationState = self.state.load(Ordering::SeqCst).into();
        match state {
            AssociationState::ShutdownSent
            | AssociationState::ShutdownAckSent
            | AssociationState::ShutdownPending
            | AssociationState::ShutdownReceived => return Err(Error::ErrStreamClosed.into()),
            _ => {}
        };

        let chunks = self.packetize(p, ppi);
        self.send_payload_data(chunks).await?;

        Ok(p.len())
    }

    fn packetize(&self, raw: &Bytes, ppi: PayloadProtocolIdentifier) -> Vec<ChunkPayloadData> {
        let mut i = 0;
        let mut remaining = raw.len();

        // From draft-ietf-rtcweb-data-protocol-09, section 6:
        //   All Data Channel Establishment Protocol messages MUST be sent using
        //   ordered delivery and reliable transmission.
        let unordered =
            ppi != PayloadProtocolIdentifier::Dcep && self.unordered.load(Ordering::SeqCst);

        let mut chunks = vec![];

        let head_abandoned = Arc::new(AtomicBool::new(false));
        let head_all_inflight = Arc::new(AtomicBool::new(false));
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
                stream_sequence_number: self.sequence_number.load(Ordering::SeqCst),
                abandoned: head_abandoned.clone(), // all fragmented chunks use the same abandoned
                all_inflight: head_all_inflight.clone(), // all fragmented chunks use the same all_inflight
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
            self.sequence_number.fetch_add(1, Ordering::SeqCst);
        }

        let old_value = self.buffered_amount.fetch_add(raw.len(), Ordering::SeqCst);
        log::trace!("[{}] bufferedAmount = {}", self.name, old_value + raw.len());

        chunks
    }

    /// Close closes the write-direction of the stream.
    /// Future calls to write are not permitted after calling Close.
    pub async fn close(&self) -> Result<()> {
        if !self.closed.load(Ordering::SeqCst) {
            // Reset the outgoing stream
            // https://tools.ietf.org/html/rfc6525
            self.send_reset_request(self.stream_identifier).await?;
        }
        self.closed.store(true, Ordering::SeqCst);
        self.read_notifier.notify_waiters(); // broadcast regardless

        Ok(())
    }

    /// buffered_amount returns the number of bytes of data currently queued to be sent over this stream.
    pub fn buffered_amount(&self) -> usize {
        self.buffered_amount.load(Ordering::SeqCst)
    }

    /// buffered_amount_low_threshold returns the number of bytes of buffered outgoing data that is
    /// considered "low." Defaults to 0.
    pub fn buffered_amount_low_threshold(&self) -> usize {
        self.buffered_amount_low.load(Ordering::SeqCst)
    }

    /// set_buffered_amount_low_threshold is used to update the threshold.
    /// See buffered_amount_low_threshold().
    pub fn set_buffered_amount_low_threshold(&self, th: usize) {
        self.buffered_amount_low.store(th, Ordering::SeqCst);
    }

    /// on_buffered_amount_low sets the callback handler which would be called when the number of
    /// bytes of outgoing data buffered is lower than the threshold.
    pub async fn on_buffered_amount_low(&self, f: OnBufferedAmountLowFn) {
        let mut on_buffered_amount_low = self.on_buffered_amount_low.lock().await;
        *on_buffered_amount_low = Some(f);
    }

    /// This method is called by association's read_loop (go-)routine to notify this stream
    /// of the specified amount of outgoing data has been delivered to the peer.
    pub(crate) async fn on_buffer_released(&self, n_bytes_released: i64) {
        if n_bytes_released <= 0 {
            return;
        }

        let from_amount = self.buffered_amount.load(Ordering::SeqCst);
        let new_amount = if from_amount < n_bytes_released as usize {
            self.buffered_amount.store(0, Ordering::SeqCst);
            log::error!(
                "[{}] released buffer size {} should be <= {}",
                self.name,
                n_bytes_released,
                0,
            );
            0
        } else {
            self.buffered_amount
                .fetch_sub(n_bytes_released as usize, Ordering::SeqCst);

            from_amount - n_bytes_released as usize
        };

        log::trace!("[{}] bufferedAmount = {}", self.name, new_amount);

        let on_buffered_amount_low = self.on_buffered_amount_low.lock().await;
        if let Some(f) = &*on_buffered_amount_low {
            let buffered_amount_low = self.buffered_amount_low.load(Ordering::SeqCst);
            if from_amount > buffered_amount_low && new_amount <= buffered_amount_low {
                f();
            }
        }
    }

    pub(crate) async fn get_num_bytes_in_reassembly_queue(&self) -> usize {
        // No lock is required as it reads the size with atomic load function.
        let reassembly_queue = self.reassembly_queue.lock().await;
        reassembly_queue.get_num_bytes()
    }

    /// get_state atomically returns the state of the Association.
    fn get_state(&self) -> AssociationState {
        self.state.load(Ordering::SeqCst).into()
    }

    fn awake_write_loop(&self) {
        //log::debug!("[{}] awake_write_loop_ch.notify_one", self.name);
        if let Some(awake_write_loop_ch) = &self.awake_write_loop_ch {
            let _ = awake_write_loop_ch.try_send(());
        }
    }

    async fn send_payload_data(&self, chunks: Vec<ChunkPayloadData>) -> Result<()> {
        let state = self.get_state();
        if state != AssociationState::Established {
            return Err(Error::ErrPayloadDataStateNotExist.into());
        }

        // Push the chunks into the pending queue first.
        for c in chunks {
            self.pending_queue.push(c).await;
        }

        self.awake_write_loop();
        Ok(())
    }

    async fn send_reset_request(&self, stream_identifier: u16) -> Result<()> {
        let state = self.get_state();
        if state != AssociationState::Established {
            return Err(Error::ErrResetPacketInStateNotExist.into());
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

        self.pending_queue.push(c).await;

        self.awake_write_loop();
        Ok(())
    }
}
