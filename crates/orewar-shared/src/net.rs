//! UDP reliability layer.
//!
//! UDP gives us unordered, unreliable datagrams. This module builds the two
//! delivery guarantees a game actually needs on top of that, and nothing more:
//!
//! * **Unreliable** — one payload per packet, delivered at most once, never
//!   retransmitted. Snapshots and input frames go here. A dropped snapshot is
//!   irrelevant 33 ms later, so resending it would waste bandwidth and add
//!   latency to the packet that replaces it.
//! * **Reliable-ordered** — a numbered message stream that is redelivered until
//!   acknowledged and handed to the application strictly in order. Handshakes,
//!   purchases, and capture events go here.
//!
//! The mechanism is the standard sequence/ack/ack-bitfield design: every packet
//! carries its own sequence number plus the highest sequence seen from the peer
//! and a 32-bit history of the ones before it. One packet therefore acknowledges
//! up to 33 of the peer's packets, so acks survive heavy loss without any
//! dedicated ack traffic. Reliable messages ride along in every outgoing packet
//! until an ack proves they landed; there is no separate retransmit timer path.
//!
//! There is deliberately **no fragmentation**: a single reliable message must
//! fit in one packet ([`MAX_MESSAGE`]), enforced at [`Endpoint::queue_reliable`].
//! Reassembly is a meaningful amount of state to get right, and nothing in this
//! game needs a message that large.

use std::collections::HashMap;
use std::fmt;

use crate::bytes::{DecodeError, Reader, Writer};

/// Kept below the ~1500-byte Ethernet MTU with room for IP + UDP headers and any
/// tunnelling, so packets are never fragmented by the network layer either.
pub const MAX_PACKET: usize = 1200;

/// `protocol(4) + kind(1) + seq(2) + ack(2) + ack_bits(4) + ack_valid(1)
/// + unreliable_len(2) + reliable_count(1)`
pub const PAYLOAD_HEADER: usize = 17;

/// Per-message overhead inside a packet: `id(2) + len(2)`.
pub const RELIABLE_OVERHEAD: usize = 4;

/// Largest single reliable message that can be queued.
pub const MAX_MESSAGE: usize = MAX_PACKET - PAYLOAD_HEADER - RELIABLE_OVERHEAD;

/// Largest unreliable payload that still leaves room for one reliable message.
pub const MAX_UNRELIABLE: usize = MAX_PACKET - PAYLOAD_HEADER;

/// How many packets we remember for ack resolution. At 30 Hz this is ~8.5 s of
/// history, far longer than any round trip we care about.
const SENT_HISTORY: usize = 256;

/// Upper bound on reliable messages packed into one packet.
const MAX_RELIABLE_PER_PACKET: usize = 16;

/// Reliable messages buffered while waiting for an earlier one to arrive. A peer
/// claiming to be further ahead than this is malfunctioning or malicious.
const MAX_REORDER_WINDOW: u16 = 1024;

const MIN_RESEND_DELAY: f32 = 0.05;
const MAX_RESEND_DELAY: f32 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    /// A reliable message exceeded [`MAX_MESSAGE`] and cannot be sent, because
    /// this layer does not fragment.
    MessageTooLarge { len: usize, max: usize },
    /// Too many reliable messages are awaiting acknowledgement.
    SendQueueFull,
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::MessageTooLarge { len, max } => {
                write!(f, "reliable message of {len} bytes exceeds the {max} byte limit")
            }
            NetError::SendQueueFull => write!(f, "reliable send queue is full"),
        }
    }
}

impl std::error::Error for NetError {}

/// Outermost discriminant of every datagram.
///
/// The handshake kinds are handled outside [`Endpoint`], because a connection
/// has to exist before there is any sequence state to talk about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketKind {
    ConnectionRequest = 1,
    ConnectionAccepted = 2,
    ConnectionDenied = 3,
    Payload = 4,
    Disconnect = 5,
}

impl PacketKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => PacketKind::ConnectionRequest,
            2 => PacketKind::ConnectionAccepted,
            3 => PacketKind::ConnectionDenied,
            4 => PacketKind::Payload,
            5 => PacketKind::Disconnect,
            _ => return None,
        })
    }
}

/// Writes `protocol_id` and `kind`, returning a writer positioned for the body.
pub fn begin_packet(protocol_id: u32, kind: PacketKind) -> Writer {
    let mut w = Writer::with_capacity(MAX_PACKET);
    w.u32(protocol_id).u8(kind as u8);
    w
}

/// Validates the outer header and returns the kind plus a reader over the body.
pub fn parse_packet(protocol_id: u32, packet: &[u8]) -> Result<(PacketKind, Reader<'_>), DecodeError> {
    let mut r = Reader::new(packet);
    if r.u32()? != protocol_id {
        return Err(DecodeError::BadTag("protocol id", 0));
    }
    let raw = r.u8()?;
    let kind = PacketKind::from_u8(raw).ok_or(DecodeError::BadTag("PacketKind", raw))?;
    Ok((kind, r))
}

/// True when `a` is strictly newer than `b` in wrapping 16-bit sequence space.
#[inline]
pub fn seq_greater(a: u16, b: u16) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000
}

/// Everything a single received packet delivered to the application.
#[derive(Debug, Default)]
pub struct Incoming {
    /// Reliable messages, already deduplicated and restored to send order.
    pub reliable: Vec<Vec<u8>>,
    /// The unreliable payload, if this packet carried one.
    pub unreliable: Option<Vec<u8>>,
    /// False when the packet was a duplicate and contributed nothing.
    pub fresh: bool,
}

#[derive(Clone)]
struct SentRecord {
    seq: u16,
    time: f64,
    reliable_ids: Vec<u16>,
    acked: bool,
    valid: bool,
}

impl Default for SentRecord {
    fn default() -> Self {
        Self { seq: 0, time: 0.0, reliable_ids: Vec::new(), acked: false, valid: false }
    }
}

#[derive(Clone)]
struct Pending {
    id: u16,
    data: Vec<u8>,
    /// `NEG_INFINITY` until first transmission, so new messages go out at once.
    last_sent: f64,
    sends: u32,
}

/// One end of a reliable connection. Transport-agnostic: it produces and
/// consumes byte buffers, and never touches a socket itself. The server owns one
/// per connected client; the client owns exactly one.
pub struct Endpoint {
    protocol_id: u32,

    // Outgoing sequence state.
    local_seq: u16,
    sent: Vec<SentRecord>,

    // Incoming sequence state. `ack_bits` bit `i` means "received `remote_seq - 1 - i`".
    remote_seq: u16,
    remote_seq_valid: bool,
    ack_bits: u32,

    // Reliable send stream.
    next_reliable_id: u16,
    pending: Vec<Pending>,

    // Reliable receive stream.
    next_deliver: u16,
    reorder: HashMap<u16, Vec<u8>>,

    // Diagnostics.
    rtt: f32,
    last_recv_time: f64,
    last_send_time: f64,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_lost: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

impl Endpoint {
    pub fn new(protocol_id: u32, now: f64) -> Self {
        Self {
            protocol_id,
            local_seq: 0,
            sent: vec![SentRecord::default(); SENT_HISTORY],
            remote_seq: 0,
            remote_seq_valid: false,
            ack_bits: 0,
            next_reliable_id: 0,
            pending: Vec::new(),
            next_deliver: 0,
            reorder: HashMap::new(),
            rtt: 0.1,
            last_recv_time: now,
            last_send_time: now,
            packets_sent: 0,
            packets_received: 0,
            packets_lost: 0,
            bytes_sent: 0,
            bytes_received: 0,
        }
    }

    /// Smoothed round-trip time in seconds.
    pub fn rtt(&self) -> f32 {
        self.rtt
    }

    pub fn time_since_recv(&self, now: f64) -> f64 {
        now - self.last_recv_time
    }

    pub fn time_since_send(&self, now: f64) -> f64 {
        now - self.last_send_time
    }

    /// Reliable messages still awaiting acknowledgement.
    pub fn unacked_count(&self) -> usize {
        self.pending.len()
    }

    /// Fraction of sent packets never acknowledged, over the retained history.
    pub fn packet_loss(&self) -> f32 {
        let total = self.packets_sent;
        if total == 0 {
            return 0.0;
        }
        self.packets_lost as f32 / total as f32
    }

    /// Queues a message for reliable, ordered delivery.
    ///
    /// Fails rather than truncating if the message cannot fit a single packet;
    /// silently dropping a reliable message would defeat the entire point.
    pub fn queue_reliable(&mut self, data: Vec<u8>) -> Result<(), NetError> {
        if data.len() > MAX_MESSAGE {
            return Err(NetError::MessageTooLarge { len: data.len(), max: MAX_MESSAGE });
        }
        if self.pending.len() >= MAX_REORDER_WINDOW as usize {
            return Err(NetError::SendQueueFull);
        }
        let id = self.next_reliable_id;
        self.next_reliable_id = self.next_reliable_id.wrapping_add(1);
        self.pending.push(Pending { id, data, last_sent: f64::NEG_INFINITY, sends: 0 });
        Ok(())
    }

    /// Builds the next outgoing packet.
    ///
    /// The unreliable payload is written first and is never displaced: a
    /// backlog of reliable messages must not be able to starve the stream of
    /// snapshots or inputs that keeps the game moving. Reliable messages then
    /// fill whatever space is left.
    pub fn build_packet(&mut self, now: f64, unreliable: Option<&[u8]>) -> Vec<u8> {
        let seq = self.local_seq;
        self.local_seq = self.local_seq.wrapping_add(1);

        let mut w = begin_packet(self.protocol_id, PacketKind::Payload);
        w.u16(seq);
        w.u16(self.remote_seq);
        w.u32(self.ack_bits);
        // Before we have heard anything, "ack 0" would be indistinguishable from
        // a genuine acknowledgement of packet 0 -- which would let the peer
        // retire a reliable message that never arrived, stalling its stream
        // permanently. Say explicitly whether the ack field means anything.
        w.bool(self.remote_seq_valid);

        let payload = unreliable.unwrap_or(&[]);
        debug_assert!(payload.len() <= MAX_UNRELIABLE, "unreliable payload exceeds packet budget");
        let payload = &payload[..payload.len().min(MAX_UNRELIABLE)];
        w.u16(payload.len() as u16);
        w.raw(payload);

        // Pick resend candidates. A message is retried once we would expect an
        // ack to have arrived, scaled to the measured round trip.
        let resend_delay = (self.rtt * 1.5).clamp(MIN_RESEND_DELAY, MAX_RESEND_DELAY) as f64;
        let mut chosen: Vec<usize> = Vec::new();
        let mut budget = MAX_PACKET.saturating_sub(w.len() + 1);
        for (i, p) in self.pending.iter().enumerate() {
            if chosen.len() >= MAX_RELIABLE_PER_PACKET {
                break;
            }
            if now - p.last_sent < resend_delay {
                continue;
            }
            let cost = RELIABLE_OVERHEAD + p.data.len();
            if cost > budget {
                break;
            }
            budget -= cost;
            chosen.push(i);
        }

        w.u8(chosen.len() as u8);
        let mut ids = Vec::with_capacity(chosen.len());
        for &i in &chosen {
            let p = &mut self.pending[i];
            p.last_sent = now;
            p.sends += 1;
            ids.push(p.id);
            w.u16(p.id);
            w.u16(p.data.len() as u16);
            let data = std::mem::take(&mut p.data);
            w.raw(&data);
            p.data = data;
        }

        let slot = seq as usize % SENT_HISTORY;
        // Overwriting a record that was never acked means that packet is lost
        // beyond recovery; count it before it disappears.
        if self.sent[slot].valid && !self.sent[slot].acked {
            self.packets_lost += 1;
        }
        self.sent[slot] = SentRecord { seq, time: now, reliable_ids: ids, acked: false, valid: true };

        self.last_send_time = now;
        self.packets_sent += 1;
        let out = w.into_inner();
        self.bytes_sent += out.len() as u64;
        debug_assert!(out.len() <= MAX_PACKET, "built an oversized packet: {} bytes", out.len());
        out
    }

    /// Consumes a `Payload` packet body (positioned after the outer header).
    pub fn receive(&mut self, now: f64, r: &mut Reader<'_>) -> Result<Incoming, DecodeError> {
        let seq = r.u16()?;
        let ack = r.u16()?;
        let ack_bits = r.u32()?;
        let ack_valid = r.bool()?;

        self.last_recv_time = now;
        self.packets_received += 1;
        self.bytes_received += r.remaining() as u64 + 9;

        if self.already_received(seq) {
            return Ok(Incoming { fresh: false, ..Default::default() });
        }
        self.record_received(seq);
        if ack_valid {
            self.process_acks(now, ack, ack_bits);
        }

        let unreliable_len = r.u16()? as usize;
        let unreliable =
            if unreliable_len > 0 { Some(r.raw(unreliable_len)?.to_vec()) } else { None };

        let count = r.u8()? as usize;
        for _ in 0..count {
            let id = r.u16()?;
            let len = r.u16()? as usize;
            if len > MAX_MESSAGE {
                return Err(DecodeError::TooLong);
            }
            let data = r.raw(len)?.to_vec();
            self.accept_reliable(id, data);
        }

        Ok(Incoming { reliable: self.drain_reliable(), unreliable, fresh: true })
    }

    fn already_received(&self, seq: u16) -> bool {
        if !self.remote_seq_valid {
            return false;
        }
        if seq == self.remote_seq {
            return true;
        }
        if seq_greater(seq, self.remote_seq) {
            return false;
        }
        let diff = self.remote_seq.wrapping_sub(seq) as u32;
        // Anything older than the bitfield is treated as a duplicate: we cannot
        // prove otherwise, and reliable delivery does not depend on this.
        if diff > 32 { true } else { self.ack_bits & (1 << (diff - 1)) != 0 }
    }

    fn record_received(&mut self, seq: u16) {
        if !self.remote_seq_valid {
            self.remote_seq = seq;
            self.remote_seq_valid = true;
            self.ack_bits = 0;
            return;
        }
        if seq_greater(seq, self.remote_seq) {
            let shift = seq.wrapping_sub(self.remote_seq) as u32;
            // Shifting a u32 by >= 32 is undefined, and semantically the whole
            // history has scrolled out of the window anyway.
            self.ack_bits = if shift >= 32 { 0 } else { self.ack_bits << shift };
            if shift <= 32 {
                self.ack_bits |= 1 << (shift - 1);
            }
            self.remote_seq = seq;
        } else {
            let diff = self.remote_seq.wrapping_sub(seq) as u32;
            if (1..=32).contains(&diff) {
                self.ack_bits |= 1 << (diff - 1);
            }
        }
    }

    fn process_acks(&mut self, now: f64, ack: u16, ack_bits: u32) {
        let mut newest_sample: Option<f64> = None;
        for i in 0..=32u32 {
            if i > 0 && ack_bits & (1 << (i - 1)) == 0 {
                continue;
            }
            let seq = ack.wrapping_sub(i as u16);
            let slot = seq as usize % SENT_HISTORY;
            let rec = &mut self.sent[slot];
            if !rec.valid || rec.seq != seq || rec.acked {
                continue;
            }
            rec.acked = true;
            if i == 0 {
                newest_sample = Some(now - rec.time);
            }
            let ids = std::mem::take(&mut rec.reliable_ids);
            if !ids.is_empty() {
                self.pending.retain(|p| !ids.contains(&p.id));
            }
        }

        if let Some(sample) = newest_sample {
            let sample = sample.clamp(0.0, 2.0) as f32;
            // Exponential moving average; a single delayed ack should nudge the
            // resend delay, not redefine it.
            self.rtt += (sample - self.rtt) * 0.1;
        }
    }

    fn accept_reliable(&mut self, id: u16, data: Vec<u8>) {
        // Older than the message we are waiting on: already delivered, or a
        // straggling retransmission of one we have.
        if id != self.next_deliver && !seq_greater(id, self.next_deliver) {
            return;
        }
        // Implausibly far ahead: a malfunctioning or hostile peer. Buffering it
        // would let a single packet pin arbitrary memory.
        if id.wrapping_sub(self.next_deliver) > MAX_REORDER_WINDOW {
            return;
        }
        self.reorder.entry(id).or_insert(data);
    }

    fn drain_reliable(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(data) = self.reorder.remove(&self.next_deliver) {
            out.push(data);
            self.next_deliver = self.next_deliver.wrapping_add(1);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    const PROTO: u32 = 0xAABB_CCDD;

    /// Feeds a packet built by one endpoint into the other.
    fn deliver(to: &mut Endpoint, now: f64, packet: &[u8]) -> Incoming {
        let (kind, mut r) = parse_packet(PROTO, packet).expect("valid packet");
        assert_eq!(kind, PacketKind::Payload);
        to.receive(now, &mut r).expect("decodable payload")
    }

    /// Regression: a peer that has not yet received anything must not be able
    /// to retire our reliable messages.
    ///
    /// An endpoint with no history has nothing meaningful to put in the ack
    /// field. If that empty ack is taken at face value it reads as "packet 0
    /// acknowledged", the sender drops reliable message 0 without it ever having
    /// arrived, and the receiver's ordered stream blocks on the gap forever --
    /// every later message arrives and none can ever be delivered.
    #[test]
    fn an_empty_ack_does_not_retire_an_unreceived_message() {
        let mut sender = Endpoint::new(PROTO, 0.0);
        let mut receiver = Endpoint::new(PROTO, 0.0);

        sender.queue_reliable(b"first".to_vec()).unwrap();
        // Sent, then lost in transit.
        let _lost = sender.build_packet(0.0, None);
        assert_eq!(sender.unacked_count(), 1);

        // The receiver, still having heard nothing, sends us a packet anyway.
        let empty_ack = receiver.build_packet(0.0, None);
        deliver(&mut sender, 0.0, &empty_ack);

        assert_eq!(sender.unacked_count(), 1, "message must survive an ack from a peer with no history");

        // And it does eventually get through once resends kick in.
        let retry = sender.build_packet(1.0, None);
        let got = deliver(&mut receiver, 1.0, &retry);
        assert_eq!(got.reliable, vec![b"first".to_vec()]);
    }

    #[test]
    fn seq_comparison_wraps() {
        assert!(seq_greater(1, 0));
        assert!(!seq_greater(0, 1));
        assert!(seq_greater(0, 65535));
        assert!(!seq_greater(65535, 0));
        assert!(!seq_greater(5, 5));
    }

    #[test]
    fn unreliable_payload_round_trips() {
        let mut a = Endpoint::new(PROTO, 0.0);
        let mut b = Endpoint::new(PROTO, 0.0);
        let packet = a.build_packet(0.0, Some(b"snapshot"));
        let got = deliver(&mut b, 0.0, &packet);
        assert_eq!(got.unreliable.as_deref(), Some(&b"snapshot"[..]));
        assert!(got.reliable.is_empty());
    }

    #[test]
    fn oversized_reliable_message_is_rejected_not_truncated() {
        let mut a = Endpoint::new(PROTO, 0.0);
        let err = a.queue_reliable(vec![0u8; MAX_MESSAGE + 1]).unwrap_err();
        assert!(matches!(err, NetError::MessageTooLarge { .. }));
        assert!(a.queue_reliable(vec![0u8; MAX_MESSAGE]).is_ok());
    }

    #[test]
    fn duplicate_packets_are_ignored() {
        let mut a = Endpoint::new(PROTO, 0.0);
        let mut b = Endpoint::new(PROTO, 0.0);
        a.queue_reliable(b"once".to_vec()).unwrap();
        let packet = a.build_packet(0.0, None);
        let first = deliver(&mut b, 0.0, &packet);
        assert_eq!(first.reliable.len(), 1);
        let second = deliver(&mut b, 0.0, &packet);
        assert!(!second.fresh);
        assert!(second.reliable.is_empty());
    }

    #[test]
    fn a_big_unreliable_payload_never_starves_reliable_messages() {
        let mut a = Endpoint::new(PROTO, 0.0);
        let mut b = Endpoint::new(PROTO, 0.0);
        for i in 0..8u8 {
            a.queue_reliable(vec![i; 32]).unwrap();
        }
        // Fill the packet almost entirely with unreliable data.
        let fat = vec![9u8; MAX_UNRELIABLE - 64];
        let packet = a.build_packet(0.0, Some(&fat));
        assert!(packet.len() <= MAX_PACKET);
        let got = deliver(&mut b, 0.0, &packet);
        assert_eq!(got.unreliable.map(|v| v.len()), Some(MAX_UNRELIABLE - 64));
        // Some reliables fit, the rest stay queued for the next packet.
        assert!(!got.reliable.is_empty(), "at least one reliable should fit");
        assert!(a.unacked_count() > 0, "unsent reliables must remain queued");
    }

    /// The load-bearing test: reliable messages must arrive exactly once each,
    /// in order, across a link that drops and reorders aggressively.
    #[test]
    fn reliable_stream_survives_a_lossy_reordering_link() {
        const MESSAGES: usize = 500;
        const DROP_RATE: f32 = 0.3;

        let mut sender = Endpoint::new(PROTO, 0.0);
        let mut receiver = Endpoint::new(PROTO, 0.0);
        let mut rng = Rng::new(0xDEAD_BEEF);

        let mut queued = 0usize;
        let mut delivered: Vec<u32> = Vec::new();
        // Packets held back a tick or two, to force out-of-order arrival.
        let mut in_flight: Vec<(f64, Vec<u8>, bool)> = Vec::new();

        let mut now = 0.0f64;
        for tick in 0..4000 {
            now += 1.0 / 30.0;

            if queued < MESSAGES && tick % 2 == 0 {
                sender.queue_reliable((queued as u32).to_le_bytes().to_vec()).unwrap();
                queued += 1;
            }

            // Sender -> receiver, and the ack packet back the other way.
            let fwd = sender.build_packet(now, None);
            let back = receiver.build_packet(now, None);
            for (packet, forward) in [(fwd, true), (back, false)] {
                if rng.chance(DROP_RATE) {
                    continue;
                }
                let jitter = rng.range_f32(0.0, 0.09) as f64;
                in_flight.push((now + jitter, packet, forward));
            }

            // Deliver whatever has come due, in arrival order rather than send order.
            in_flight.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            let mut still = Vec::new();
            for (due, packet, forward) in in_flight.drain(..) {
                if due > now {
                    still.push((due, packet, forward));
                    continue;
                }
                if forward {
                    let got = deliver(&mut receiver, now, &packet);
                    for msg in got.reliable {
                        delivered.push(u32::from_le_bytes(msg.try_into().unwrap()));
                    }
                } else {
                    deliver(&mut sender, now, &packet);
                }
            }
            in_flight = still;

            if delivered.len() == MESSAGES {
                break;
            }
        }

        assert_eq!(delivered.len(), MESSAGES, "every message must arrive exactly once");
        let expected: Vec<u32> = (0..MESSAGES as u32).collect();
        assert_eq!(delivered, expected, "messages must arrive in send order");

        // Delivery of the last message necessarily precedes its ack getting
        // home, so let the link settle on a clean channel before asserting the
        // send queue drained.
        for _ in 0..50 {
            now += 1.0 / 30.0;
            let fwd = sender.build_packet(now, None);
            deliver(&mut receiver, now, &fwd);
            let back = receiver.build_packet(now, None);
            deliver(&mut sender, now, &back);
        }
        assert_eq!(sender.unacked_count(), 0, "every message should end up acknowledged");
        assert!(sender.rtt() > 0.0 && sender.rtt() < 1.0, "rtt estimate: {}", sender.rtt());
        assert!(sender.packets_lost > 0, "the lossy link should have registered losses");
    }

    /// Sequence numbers are 16 bits; the connection must survive them wrapping.
    #[test]
    fn reliable_delivery_survives_sequence_wraparound() {
        let mut a = Endpoint::new(PROTO, 0.0);
        let mut b = Endpoint::new(PROTO, 0.0);
        let mut now = 0.0f64;
        let mut delivered = 0u32;

        // Well past 65536 packets, so both packet and message ids wrap.
        for i in 0..70_000u32 {
            now += 1.0 / 30.0;
            if i % 3 == 0 {
                a.queue_reliable(vec![(i % 251) as u8]).unwrap();
            }
            let fwd = a.build_packet(now, None);
            let got = deliver(&mut b, now, &fwd);
            for msg in &got.reliable {
                assert_eq!(msg[0], ((delivered * 3) % 251) as u8, "ordering broke at {delivered}");
                delivered += 1;
            }
            let back = b.build_packet(now, None);
            deliver(&mut a, now, &back);
        }

        assert!(delivered > 23_000, "expected ~23334 deliveries, got {delivered}");
        assert_eq!(a.unacked_count(), 0);
    }
}
