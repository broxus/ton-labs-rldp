use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use adnl::{common::*, node::AdnlNode};
use dashmap::DashMap;
use rand::Rng;
pub use raptorq;
use tokio::sync::mpsc;
use ton_api::ton::fec::{type_::RaptorQ as FecTypeRaptorQ, Type as FecType};
use ton_api::ton::rldp::message::Query as RldpQuery;
use ton_api::ton::rldp::messagepart::Complete as RldpComplete;
use ton_api::ton::rldp::messagepart::Confirm as RldpConfirm;
use ton_api::ton::rldp::messagepart::MessagePart as RldpMessagePart;
use ton_api::ton::rldp::Message as RldpMessageBoxed;
use ton_api::ton::rldp::MessagePart as RldpMessagePartBoxed;
use ton_api::{ton, IntoBoxed};
use ton_types::{fail, Result};

const TARGET: &str = "rldp";

type TransferId = [u8; 32];

/// RaptorQ decoder
pub struct RaptorqDecoder {
    engine: raptorq::Decoder,
    params: FecTypeRaptorQ,
    seqno: u32,
}

impl RaptorqDecoder {
    /// Construct with parameter
    pub fn with_params(params: FecTypeRaptorQ) -> Self {
        Self {
            engine: raptorq::Decoder::new(raptorq::ObjectTransmissionInformation::with_defaults(
                params.data_size as u64,
                params.symbol_size as u16,
            )),
            params,
            seqno: 0,
        }
    }

    /// Decode
    pub fn decode(&mut self, seqno: u32, data: &[u8]) -> Option<Vec<u8>> {
        let packet = raptorq::EncodingPacket::new(raptorq::PayloadId::new(0, seqno), data.to_vec());
        self.seqno = seqno;
        self.engine.decode(packet)
    }

    /// Parameters
    pub fn params(&self) -> &FecTypeRaptorQ {
        &self.params
    }
}

struct RecvTransfer {
    buf: Vec<u8>,
    complete: RldpMessagePartBoxed,
    confirm: RldpMessagePartBoxed,
    confirm_count: usize,
    data: Vec<u8>,
    decoder: Option<RaptorqDecoder>,
    part: u32,
    state: Arc<RecvTransferState>,
    total_size: Option<usize>,
}

impl RecvTransfer {
    fn new(transfer_id: TransferId) -> Self {
        Self {
            buf: Vec::new(),
            complete: RldpComplete {
                transfer_id: ton::int256(transfer_id),
                part: 0,
            }
            .into_boxed(),
            confirm: RldpConfirm {
                transfer_id: ton::int256(transfer_id),
                part: 0,
                seqno: 0,
            }
            .into_boxed(),
            confirm_count: 0,
            data: Vec::new(),
            decoder: None,
            part: 0,
            state: Arc::new(RecvTransferState {
                updates: AtomicU32::new(0),
            }),
            total_size: None,
        }
    }

    fn complete(&mut self) -> Result<&mut RldpComplete> {
        match self.complete {
            RldpMessagePartBoxed::Rldp_Complete(ref mut msg) => Ok(msg),
            _ => fail!("INTERNAL ERROR: RLDP complete message mismatch"),
        }
    }

    fn confirm(&mut self) -> Result<&mut RldpConfirm> {
        match self.confirm {
            RldpMessagePartBoxed::Rldp_Confirm(ref mut msg) => Ok(msg),
            _ => fail!("INTERNAL ERROR: RLDP confirm message mismatch"),
        }
    }

    fn process_chunk(&mut self, message: RldpMessagePart) -> Result<Option<&[u8]>> {
        let fec_type = if let FecType::Fec_RaptorQ(fec_type) = message.fec_type {
            fec_type
        } else {
            fail!("Unsupported FEC type in RLDP packet")
        };
        let total_size = if let Some(total_size) = self.total_size {
            if total_size != message.total_size as usize {
                fail!("Incorrect total size in RLDP packet")
            }
            total_size
        } else {
            let total_size = message.total_size as usize;
            self.total_size = Some(total_size);
            self.data.reserve_exact(total_size);
            total_size
        };
        let decoder = if let Some(decoder) = &mut self.decoder {
            match self.part.cmp(&(message.part as u32)) {
                std::cmp::Ordering::Equal => {
                    if fec_type.as_ref() != &decoder.params {
                        fail!("Incorrect parameters in RLDP packet")
                    }
                    decoder
                }
                std::cmp::Ordering::Greater => {
                    self.complete()?.part = message.part;
                    serialize_inplace(&mut self.buf, &self.complete)?;
                    return Ok(Some(&self.buf[..]));
                }
                std::cmp::Ordering::Less => {
                    return Ok(None);
                }
            }
        } else {
            self.decoder
                .get_or_insert_with(|| RaptorqDecoder::with_params(*fec_type))
        };
        if let Some(mut data) = decoder.decode(message.seqno as u32, &message.data) {
            if data.len() + self.data.len() > total_size {
                fail!("Too big size for RLDP transfer")
            } else {
                self.data.append(&mut data)
            }
            if self.data.len() < total_size {
                self.decoder = None;
                self.part += 1;
                self.confirm_count = 0;
            }
            self.complete()?.part = message.part;
            serialize_inplace(&mut self.buf, &self.complete)?;
            Ok(Some(&self.buf[..]))
        } else if self.confirm_count == 9 {
            let max_seqno = decoder.seqno;
            let confirm = self.confirm()?;
            confirm.part = message.part;
            confirm.seqno = max_seqno as i32;
            self.confirm_count = 0;
            serialize_inplace(&mut self.buf, &self.confirm)?;
            Ok(Some(&self.buf[..]))
        } else {
            self.confirm_count += 1;
            Ok(None)
        }
    }
}

struct RecvTransferState {
    updates: AtomicU32,
}

impl RecvTransferState {
    fn updates(&self) -> u32 {
        self.updates.load(Ordering::Acquire)
    }

    fn set_updates(&self) {
        self.updates.fetch_add(1, Ordering::Release);
    }
}

/// RaptorQ encoder
pub struct RaptorqEncoder {
    encoder_index: usize,
    engine: raptorq::Encoder,
    params: FecTypeRaptorQ,
    source_packets: Vec<raptorq::EncodingPacket>,
}

impl RaptorqEncoder {
    /// Construct over data
    pub fn with_data(data: &[u8]) -> Self {
        let engine = raptorq::Encoder::with_defaults(data, SendTransfer::SYMBOL as u16);
        let mut source_packets = Vec::new();
        for encoder in engine.get_block_encoders() {
            // Reverse order to send efficiently
            let mut packets = encoder.source_packets();
            while let Some(packet) = packets.pop() {
                source_packets.push(packet)
            }
        }
        Self {
            encoder_index: 0,
            engine,
            params: FecTypeRaptorQ {
                data_size: data.len() as i32,
                symbol_size: SendTransfer::SYMBOL as i32,
                symbols_count: source_packets.len() as i32,
            },
            source_packets,
        }
    }

    /// Encode
    pub fn encode(&mut self, seqno: &mut u32) -> Result<Vec<u8>> {
        let encoders = self.engine.get_block_encoders();
        let packet = if let Some(packet) = self.source_packets.pop() {
            packet
        } else {
            let mut packets = encoders[self.encoder_index].repair_packets(*seqno, 1);
            let packet = if let Some(packet) = packets.pop() {
                packet
            } else {
                fail!("INTERNAL ERROR: cannot encode repair packet");
            };
            self.encoder_index += 1;
            if self.encoder_index >= encoders.len() {
                self.encoder_index = 0;
            }
            packet
        };
        *seqno = packet.payload_id().encoding_symbol_id();
        Ok(packet.data().to_vec())
    }

    /// Parameters
    pub fn params(&self) -> &FecTypeRaptorQ {
        &self.params
    }
}

struct SendTransfer<'a> {
    buf: Vec<u8>,
    data: &'a [u8],
    encoder: Option<RaptorqEncoder>,
    message: RldpMessagePartBoxed,
    state: Arc<SendTransferState>,
}

impl<'a> SendTransfer<'a> {
    const SLICE: usize = 2000000;
    const SYMBOL: usize = 768;
    const WINDOW: usize = 1000;

    fn new(data: &'a [u8], transfer_id: Option<TransferId>) -> Self {
        let transfer_id = transfer_id.unwrap_or_else(|| rand::thread_rng().gen());
        let message = RldpMessagePart {
            transfer_id: ton::int256(transfer_id),
            fec_type: FecTypeRaptorQ {
                data_size: 0,
                symbol_size: Self::SYMBOL as i32,
                symbols_count: 0,
            }
            .into_boxed(),
            part: 0,
            total_size: 0,
            seqno: 0,
            data: ton::bytes(Vec::new()),
        }
        .into_boxed();
        Self {
            buf: Vec::new(),
            data,
            encoder: None,
            message,
            state: Arc::new(SendTransferState {
                part: AtomicU32::new(0),
                reply: AtomicBool::new(false),
                seqno_sent: AtomicU32::new(0),
                seqno_recv: AtomicU32::new(0),
            }),
        }
    }

    fn is_finished(&self) -> bool {
        self.state.has_reply()
            && ((self.state.part() as usize + 1) * Self::SLICE >= self.data.len())
    }

    fn is_finished_or_next_part(&self, part: u32) -> Result<bool> {
        if self.is_finished() {
            Ok(true)
        } else {
            match self.state.part() {
                x if x == part => Ok(false),
                x if x == part + 1 => Ok(true),
                _ => fail!("INTERNAL ERROR: part # mismatch"),
            }
        }
    }

    fn message(&mut self) -> Result<&mut RldpMessagePart> {
        match self.message {
            RldpMessagePartBoxed::Rldp_MessagePart(ref mut msg) => Ok(msg),
            _ => fail!("INTERNAL ERROR: RLDP message mismatch"),
        }
    }

    fn prepare_chunk(&mut self) -> Result<&[u8]> {
        if let Some(encoder) = &mut self.encoder {
            let mut seqno_sent = self.state.seqno_sent();
            let seqno_sent_original = seqno_sent;
            let chunk = encoder.encode(&mut seqno_sent)?;
            let message = self.message()?;
            message.seqno = seqno_sent as i32;
            message.data = ton::bytes(chunk);
            let seqno_recv = self.state.seqno_recv();
            if seqno_sent - seqno_recv <= Self::WINDOW as u32 {
                if seqno_sent_original == seqno_sent {
                    seqno_sent += 1;
                }
                self.state.set_seqno_sent(seqno_sent);
            }
            serialize_inplace(&mut self.buf, &self.message)?;
            Ok(&self.buf[..])
        } else {
            fail!("Encoder is not ready");
        }
    }

    fn start_next_part(&mut self) -> Result<u32> {
        if self.is_finished() {
            return Ok(0);
        }
        let part = self.state.part() as usize;
        let processed = part * Self::SLICE;
        let total = self.data.len();
        if processed >= total {
            return Ok(0);
        }
        let chunk_size = std::cmp::min(total - processed, Self::SLICE);
        let encoder = RaptorqEncoder::with_data(&self.data[processed..processed + chunk_size]);
        let message = self.message()?;
        message.part = part as i32;
        message.total_size = total as i64;
        let ret = encoder.params.symbols_count;
        match message.fec_type {
            FecType::Fec_RaptorQ(ref mut fec_type) => {
                fec_type.data_size = encoder.params.data_size;
                fec_type.symbols_count = ret;
            }
            _ => fail!("INTERNAL ERROR: unsupported FEC type"),
        }
        self.encoder = Some(encoder);
        Ok(ret as u32)
    }
}

struct SendTransferState {
    part: AtomicU32,
    reply: AtomicBool,
    seqno_sent: AtomicU32,
    seqno_recv: AtomicU32,
}

impl SendTransferState {
    fn has_reply(&self) -> bool {
        self.reply.load(Ordering::Acquire)
    }

    fn part(&self) -> u32 {
        self.part.load(Ordering::Acquire)
    }

    fn seqno_recv(&self) -> u32 {
        self.seqno_recv.load(Ordering::Acquire)
    }

    fn seqno_sent(&self) -> u32 {
        self.seqno_sent.load(Ordering::Acquire)
    }

    fn set_part(&self, part: u32) {
        let _ = self
            .part
            .compare_exchange(part - 1, part, Ordering::Release, Ordering::Relaxed);
    }

    fn set_reply(&self) {
        self.reply.store(true, Ordering::Release)
    }

    fn set_seqno_recv(&self, seqno: u32) {
        if self.seqno_sent() >= seqno {
            let seqno_recv = self.seqno_recv();
            if seqno_recv < seqno {
                let _ = self.seqno_recv.compare_exchange(
                    seqno_recv,
                    seqno,
                    Ordering::Release,
                    Ordering::Relaxed,
                );
            }
        }
    }

    fn set_seqno_sent(&self, seqno: u32) {
        let seqno_sent = self.seqno_sent();
        if seqno_sent < seqno {
            let _ = self.seqno_sent.compare_exchange(
                seqno_sent,
                seqno,
                Ordering::Release,
                Ordering::Relaxed,
            );
        }
    }
}

enum RldpTransfer {
    Recv(mpsc::UnboundedSender<Box<RldpMessagePart>>),
    Send(Arc<SendTransferState>),
    Done,
}

struct RldpRecvContext {
    adnl: Arc<AdnlNode>,
    peers: AdnlPeers,
    queue_reader: mpsc::UnboundedReceiver<Box<RldpMessagePart>>,
    recv_transfer: RecvTransfer,
    transfer_id: TransferId,
}

struct RldpSendContext<'a> {
    adnl: Arc<AdnlNode>,
    peers: AdnlPeers,
    send_transfer: SendTransfer<'a>,
    transfer_id: TransferId,
}

struct RldpPeer {
    queries: AtomicU32,
    queue: lockfree::queue::Queue<Arc<tokio::sync::Barrier>>,
}

/// Rldp Node
pub struct RldpNode {
    adnl: Arc<AdnlNode>,
    peers: DashMap<Arc<KeyId>, Arc<RldpPeer>>,
    subscribers: Arc<Vec<Arc<dyn Subscriber>>>,
    transfers: Arc<DashMap<TransferId, RldpTransfer>>,
}

impl RldpNode {
    const MAX_QUERIES: u32 = 3;
    const SIZE_TRANSFER_WAVE: u32 = 10;
    const SPINNER: u64 = 10; // Milliseconds
    const TIMEOUT_MAX: u64 = 10000; // Milliseconds
    const TIMEOUT_MIN: u64 = 500; // Milliseconds

    /// Constructor
    pub fn with_adnl_node(adnl: Arc<AdnlNode>, subscribers: Vec<Arc<dyn Subscriber>>) -> Arc<Self> {
        Arc::new(Self {
            adnl,
            peers: DashMap::new(),
            subscribers: Arc::new(subscribers),
            transfers: Arc::new(DashMap::new()),
        })
    }

    /// Send query
    pub async fn query(
        &self,
        data: &[u8],
        max_answer_size: Option<i64>,
        peers: &AdnlPeers,
        roundtrip: Option<u64>,
    ) -> Result<(Option<Vec<u8>>, u64)> {
        self.query_transfer(data, max_answer_size, peers, roundtrip)
            .await
    }

    fn answer_transfer(
        &self,
        transfer_id: &TransferId,
        peers: &AdnlPeers,
    ) -> Result<Option<mpsc::UnboundedSender<Box<RldpMessagePart>>>> {
        use dashmap::mapref::entry::Entry;

        let (queue_sender, queue_reader) = mpsc::unbounded_channel();

        match self.transfers.entry(*transfer_id) {
            Entry::Vacant(entry) => entry.insert(RldpTransfer::Recv(queue_sender.clone())),
            Entry::Occupied(_) => return Ok(None),
        };

        let mut context = RldpRecvContext {
            adnl: self.adnl.clone(),
            peers: peers.clone(),
            queue_reader,
            recv_transfer: RecvTransfer::new(*transfer_id),
            transfer_id: *transfer_id,
        };

        tokio::spawn({
            let subscribers = self.subscribers.clone();
            let transfers = self.transfers.clone();

            async move {
                Self::receive_loop(&mut context, None).await;
                transfers.insert(context.transfer_id, RldpTransfer::Done);

                let send_transfer_id =
                    Self::answer_transfer_loop(&mut context, subscribers, transfers.clone())
                        .await
                        .unwrap_or_else(|e| {
                            log::warn!(
                                target: TARGET,
                                "ERROR: {}, transfer {}",
                                e,
                                base64::encode(&context.transfer_id)
                            );
                            None
                        });
                tokio::time::sleep(Duration::from_millis(Self::TIMEOUT_MAX * 2)).await;
                if let Some(send_transfer_id) = send_transfer_id {
                    transfers.remove(&send_transfer_id);
                }
                transfers.remove(&context.transfer_id);
            }
        });

        let transfers = self.transfers.clone();
        let transfer_id = *transfer_id;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(Self::TIMEOUT_MAX)).await;
            transfers.insert(transfer_id, RldpTransfer::Done);
        });

        Ok(Some(queue_sender))
    }

    async fn answer_transfer_loop(
        context: &mut RldpRecvContext,
        subscribers: Arc<Vec<Arc<dyn Subscriber>>>,
        transfers: Arc<DashMap<TransferId, RldpTransfer>>,
    ) -> Result<Option<TransferId>> {
        let query =
            match deserialize(&context.recv_transfer.data[..])?.downcast::<RldpMessageBoxed>() {
                Ok(RldpMessageBoxed::Rldp_Query(query)) => query,
                Ok(message) => fail!("Unexpected RLDP message: {:?}", message),
                Err(object) => fail!("Unexpected RLDP message: {:?}", object),
            };

        let answer = match Query::process_rldp(&subscribers, &query, &context.peers).await? {
            (true, Some(answer)) => answer,
            (true, None) => return Ok(None),
            _ => fail!("No subscribers for query {:?}", query),
        };

        let (len, max) = (answer.data.len(), query.max_answer_size as usize);
        if len > max {
            fail!("Exceeded max RLDP answer size: {} vs {}", len, max)
        }

        let data = serialize(&answer.into_boxed())?;
        let mut send_transfer_id = context.transfer_id;
        for byte in &mut send_transfer_id {
            *byte ^= 0xFF
        }
        log::trace!(
            target: TARGET,
            "RLDP answer to be sent in transfer {}/{} to {}",
            base64::encode(&context.transfer_id),
            base64::encode(&send_transfer_id),
            context.peers.other()
        );

        let send_transfer = SendTransfer::new(data.as_slice(), Some(send_transfer_id));
        transfers.insert(
            send_transfer_id,
            RldpTransfer::Send(send_transfer.state.clone()),
        );
        let context_send = RldpSendContext {
            adnl: context.adnl.clone(),
            peers: context.peers.clone(),
            send_transfer,
            transfer_id: context.transfer_id,
        };

        match Self::send_loop(context_send, None).await? {
            (true, _) => log::trace!(
                target: TARGET,
                "RLDP answer sent in transfer {} to {}",
                base64::encode(&context.transfer_id),
                context.peers.other()
            ),
            _ => log::warn!(
                target: TARGET,
                "Timeout on answer in RLDP transfer {} to {}",
                base64::encode(&context.transfer_id),
                context.peers.other()
            ),
        }

        Ok(Some(send_transfer_id))
    }

    fn calc_timeout(roundtrip: Option<u64>) -> u64 {
        std::cmp::max(roundtrip.unwrap_or(Self::TIMEOUT_MAX), Self::TIMEOUT_MIN)
    }

    fn is_timed_out(timeout: u64, updates: u32, start: &Instant) -> bool {
        start.elapsed().as_millis() as u64 > timeout + timeout * updates as u64 / 100
    }

    async fn query_transfer(
        &self,
        data: &[u8],
        max_answer_size: Option<i64>,
        peers: &AdnlPeers,
        roundtrip: Option<u64>,
    ) -> Result<(Option<Vec<u8>>, u64)> {
        use dashmap::mapref::entry::Entry;

        let query_id: QueryId = rand::thread_rng().gen();
        let data = serialize(
            &RldpQuery {
                query_id: ton::int256(query_id),
                max_answer_size: max_answer_size.unwrap_or(128 * 1024),
                timeout: now() + Self::TIMEOUT_MAX as i32 / 1000,
                data: ton::bytes(data.to_vec()),
            }
            .into_boxed(),
        )?;

        let peer = match self.peers.entry(peers.other().clone()) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => entry
                .insert(Arc::new(RldpPeer {
                    queries: AtomicU32::new(0),
                    queue: lockfree::queue::Queue::new(),
                }))
                .value()
                .clone(),
        };

        let queries = peer.queries.fetch_add(1, Ordering::Acquire);
        if queries >= Self::MAX_QUERIES {
            let ping = Arc::new(tokio::sync::Barrier::new(2));
            peer.queue.push(ping.clone());
            ping.wait().await;
        }

        let send_transfer = SendTransfer::new(data.as_slice(), None);
        let send_transfer_id = send_transfer.message.transfer_id().0;
        self.transfers.insert(
            send_transfer_id,
            RldpTransfer::Send(send_transfer.state.clone()),
        );
        let mut recv_transfer_id = send_transfer_id;
        for byte in &mut recv_transfer_id {
            *byte ^= 0xFF
        }
        let (queue_sender, queue_reader) = mpsc::unbounded_channel();
        let recv_transfer = RecvTransfer::new(recv_transfer_id);
        self.transfers
            .insert(recv_transfer_id, RldpTransfer::Recv(queue_sender));
        let send_context = RldpSendContext {
            adnl: self.adnl.clone(),
            peers: peers.clone(),
            send_transfer,
            transfer_id: send_transfer_id,
        };
        let recv_context = RldpRecvContext {
            adnl: self.adnl.clone(),
            peers: peers.clone(),
            queue_reader,
            recv_transfer,
            transfer_id: send_transfer_id,
        };
        log::trace!(
            target: TARGET,
            "transfer id {}/{}, total to send {}",
            base64::encode(&send_transfer_id),
            base64::encode(&recv_transfer_id),
            data.len()
        );
        let res = self
            .query_transfer_loop(send_context, recv_context, roundtrip)
            .await;
        if res.is_err() {
            self.transfers.insert(send_transfer_id, RldpTransfer::Done);
        }
        self.transfers.insert(recv_transfer_id, RldpTransfer::Done);
        let transfers = self.transfers.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(Self::TIMEOUT_MAX * 2)).await;
            transfers.remove(&send_transfer_id);
            transfers.remove(&recv_transfer_id);
        });

        let queries = peer.queries.fetch_sub(1, Ordering::Acquire);
        if queries > Self::MAX_QUERIES {
            loop {
                if let Some(pong) = peer.queue.pop() {
                    pong.wait().await;
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
        let (answer, roundtrip) = res?;
        if let Some(answer) = answer {
            match deserialize(&answer[..])?.downcast::<RldpMessageBoxed>() {
                Ok(RldpMessageBoxed::Rldp_Answer(answer)) => {
                    if answer.query_id.0 != query_id {
                        fail!("Unknown query ID in RLDP answer")
                    } else {
                        log::trace!(
                            target: TARGET,
                            "RLDP answer {:02x}{:02x}{:02x}{:02x}...",
                            answer.data[0],
                            answer.data[1],
                            answer.data[2],
                            answer.data[3]
                        );
                        Ok((Some(answer.data.to_vec()), roundtrip))
                    }
                }
                Ok(answer) => fail!("Unexpected answer to RLDP query: {:?}", answer),
                Err(answer) => fail!("Unexpected answer to RLDP query: {:?}", answer),
            }
        } else {
            Ok((None, roundtrip))
        }
    }

    async fn query_transfer_loop(
        &self,
        send_context: RldpSendContext<'_>,
        mut recv_context: RldpRecvContext,
        roundtrip: Option<u64>,
    ) -> Result<(Option<Vec<u8>>, u64)> {
        let ping = Arc::new(lockfree::queue::Queue::new());
        let pong = ping.clone();
        let peers = send_context.peers.clone();
        let recv_state = recv_context.recv_transfer.state.clone();
        let send_state = send_context.send_transfer.state.clone();
        let transfer_id = send_context.transfer_id;
        tokio::spawn(async move {
            Self::receive_loop(&mut recv_context, Some(send_state)).await;
            pong.push(recv_context.recv_transfer)
        });
        let (ok, mut roundtrip) = Self::send_loop(send_context, roundtrip).await?;
        let mut timeout = Self::calc_timeout(Some(roundtrip));
        self.transfers.insert(transfer_id, RldpTransfer::Done);
        if ok {
            log::trace!(
                target: TARGET,
                "RLDP query sent in transfer {} to {}, waiting for answer",
                base64::encode(&transfer_id),
                peers.other()
            )
        } else {
            log::warn!(
                target: TARGET,
                "Timeout ({} ms) on query in RLDP transfer {} to {}",
                timeout,
                base64::encode(&transfer_id),
                peers.other()
            );
            return Ok((None, roundtrip));
        }
        let mut start_part = Instant::now();
        let mut updates = recv_state.updates();
        loop {
            tokio::time::sleep(Duration::from_millis(Self::SPINNER)).await;
            let new_updates = recv_state.updates();
            if new_updates > updates {
                log::trace!(
                    target: TARGET,
                    "Recv updates {} -> {} in transfer {}",
                    updates,
                    new_updates,
                    base64::encode(&transfer_id)
                );
                timeout = Self::update_roundtrip(&mut roundtrip, &start_part);
                updates = new_updates;
                start_part = Instant::now();
            } else if Self::is_timed_out(timeout, updates, &start_part) {
                log::warn!(
                    target: TARGET,
                    "No activity for transfer {} to {} in {} ms, aborting",
                    base64::encode(&transfer_id),
                    peers.other(),
                    timeout
                );
                break;
            }
            if let Some(reply) = ping.pop() {
                log::trace!(
                    target: TARGET,
                    "Got reply for transfer {} from {}",
                    base64::encode(&transfer_id),
                    peers.other()
                );
                Self::update_roundtrip(&mut roundtrip, &start_part);
                return Ok((Some(reply.data), roundtrip));
            }
        }
        Ok((None, roundtrip))
    }

    async fn receive_loop(
        context: &mut RldpRecvContext,
        mut send_state: Option<Arc<SendTransferState>>,
    ) {
        while let Some(job) = context.queue_reader.recv().await {
            let begin = context.recv_transfer.data.is_empty();
            match context.recv_transfer.process_chunk(*job) {
                Err(e) => log::warn!("RLDP error: {}", e),
                Ok(Some(reply)) => {
                    if let Err(e) = context.adnl.send_custom(reply, &context.peers).await {
                        log::warn!("RLDP error: {}", e)
                    }
                }
                _ => (),
            }
            context.recv_transfer.state.set_updates();
            if let Some(send_state) = send_state.take() {
                send_state.set_reply();
            }
            if begin && !context.recv_transfer.data.is_empty() {
                log::trace!(
                    target: TARGET,
                    "transfer id {}, received first {}, total to receive {:?}",
                    base64::encode(&context.transfer_id),
                    context.recv_transfer.data.len(),
                    context.recv_transfer.total_size
                );
            }
            if let Some(total_size) = context.recv_transfer.total_size {
                if total_size == context.recv_transfer.data.len() {
                    log::trace!(
                        target: TARGET,
                        "transfer id {}, receive completed ({})",
                        base64::encode(&context.transfer_id),
                        total_size,
                    );
                    break;
                }
            } else {
                log::warn!("INTERNAL ERROR: RLDP total size mismatch")
            }
        }
        // Graceful close
        context.queue_reader.close();
        while context.queue_reader.recv().await.is_some() {}
    }

    async fn send_loop(
        mut context: RldpSendContext<'_>,
        roundtrip: Option<u64>,
    ) -> Result<(bool, u64)> {
        let mut timeout = Self::calc_timeout(roundtrip);
        let mut roundtrip = roundtrip.unwrap_or(0);
        loop {
            let mut transfer_wave = context.send_transfer.start_next_part()?;
            if transfer_wave == 0 {
                break;
            }
            transfer_wave = std::cmp::min(transfer_wave, Self::SIZE_TRANSFER_WAVE);
            let part = context.send_transfer.state.part();
            let mut start_part = Instant::now();
            let mut recv_seqno = 0;
            'part: loop {
                for _ in 0..transfer_wave {
                    context
                        .adnl
                        .send_custom(context.send_transfer.prepare_chunk()?, &context.peers)
                        .await?;
                    if context.send_transfer.is_finished_or_next_part(part)? {
                        break 'part;
                    }
                }
                tokio::time::sleep(Duration::from_millis(Self::SPINNER)).await;
                if context.send_transfer.is_finished_or_next_part(part)? {
                    break;
                }
                let new_recv_seqno = context.send_transfer.state.seqno_recv();
                if new_recv_seqno > recv_seqno {
                    log::trace!(
                        target: TARGET,
                        "Send updates {} -> {} in transfer {}",
                        recv_seqno,
                        new_recv_seqno,
                        base64::encode(&context.transfer_id)
                    );
                    timeout = Self::update_roundtrip(&mut roundtrip, &start_part);
                    recv_seqno = new_recv_seqno;
                    start_part = Instant::now();
                } else if Self::is_timed_out(timeout, recv_seqno, &start_part) {
                    return Ok((false, std::cmp::min(roundtrip * 2, Self::TIMEOUT_MAX)));
                }
            }
            timeout = Self::update_roundtrip(&mut roundtrip, &start_part);
        }
        Ok((true, roundtrip))
    }

    fn update_roundtrip(roundtrip: &mut u64, start: &Instant) -> u64 {
        *roundtrip = if *roundtrip == 0 {
            start.elapsed().as_millis() as u64
        } else {
            (*roundtrip + start.elapsed().as_millis() as u64) / 2
        };
        Self::calc_timeout(Some(*roundtrip))
    }
}

#[async_trait::async_trait]
impl Subscriber for RldpNode {
    async fn try_consume_custom(&self, data: &[u8], peers: &AdnlPeers) -> Result<bool> {
        let msg = match deserialize(data) {
            Ok(msg) => msg,
            Err(_) => return Ok(false),
        };

        let msg = match msg.downcast::<RldpMessagePartBoxed>() {
            Ok(msg) => msg,
            Err(_) => return Ok(false),
        };

        match msg {
            RldpMessagePartBoxed::Rldp_Complete(msg) => {
                if let Some(transfer) = self.transfers.get(&msg.transfer_id.0) {
                    if let RldpTransfer::Send(transfer) = transfer.value() {
                        transfer.set_part(msg.part as u32 + 1);
                    }
                }
            }
            RldpMessagePartBoxed::Rldp_Confirm(msg) => {
                if let Some(transfer) = self.transfers.get(&msg.transfer_id.0) {
                    if let RldpTransfer::Send(transfer) = transfer.value() {
                        if transfer.part() == msg.part as u32 {
                            transfer.set_seqno_recv(msg.seqno as u32);
                        }
                    }
                }
            }
            RldpMessagePartBoxed::Rldp_MessagePart(msg) => {
                let transfer_id = get256(&msg.transfer_id);
                loop {
                    if let Some(transfer) = self.transfers.get(transfer_id) {
                        if let RldpTransfer::Recv(queue_sender) = transfer.value() {
                            let _ = queue_sender.send(msg);
                        } else {
                            let reply = RldpConfirm {
                                transfer_id: msg.transfer_id,
                                part: msg.part,
                                seqno: msg.seqno,
                            }
                            .into_boxed();
                            let reply = serialize(&reply)?;
                            self.adnl.send_custom(&reply[..], peers).await?;
                            let reply = RldpComplete {
                                transfer_id: msg.transfer_id,
                                part: msg.part,
                            }
                            .into_boxed();
                            let reply = serialize(&reply)?;
                            self.adnl.send_custom(&reply[..], peers).await?;
                            log::info!(
                                target: TARGET,
                                "Receive update on closed RLDP transfer {}, part {}, seqno {}",
                                base64::encode(transfer_id),
                                msg.part,
                                msg.seqno
                            );
                            break;
                        }
                    } else if let Some(queue_sender) = self.answer_transfer(transfer_id, peers)? {
                        let _ = queue_sender.send(msg);
                    } else {
                        continue;
                    }

                    break;
                }
            }
        }
        Ok(true)
    }
}
