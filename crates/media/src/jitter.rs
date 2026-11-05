//! Adaptive jitter buffer for one remote Opus stream.
//!
//! Packets arrive with RTP sequence numbers and timestamps, possibly late, duplicated or
//! out of order. The mixer pulls exactly one 20 ms frame per tick with [`JitterBuffer::pop`]:
//!
//! * the packet for the next sequence number, if present;
//! * otherwise the following packet for Opus in-band FEC recovery, if present;
//! * otherwise packet loss concealment (PLC).
//!
//! The playout delay adapts between [`MIN_TARGET_FRAMES`] (40 ms) and [`MAX_FRAMES`]
//! (120 ms) from an RFC 3550 interarrival jitter estimate and observed underruns.
//! All storage is a fixed array of slots; nothing allocates after construction.

use std::time::Instant;

/// Duration of one frame in milliseconds.
pub const FRAME_MS: u32 = 20;
/// RTP timestamp increment per 20 ms frame at 48 kHz.
pub const FRAME_TS: u32 = 960;
/// Largest Opus packet we store (RFC 6716 maximum frame size is 1275 bytes).
pub const MAX_PACKET: usize = 1276;
/// Minimum (and initial) playout target: 2 frames = 40 ms.
pub const MIN_TARGET_FRAMES: u16 = 2;
/// Maximum buffered span: 6 frames = 120 ms.
pub const MAX_FRAMES: u16 = 6;
/// Number of slots (power of two, larger than `MAX_FRAMES` plus reorder headroom).
const SLOTS: usize = 16;
/// Consecutive concealed frames after which the buffer stops and re-buffers.
const MAX_CONSECUTIVE_PLC: u16 = 5;
/// Good pops (about 5 s) after which an underrun-raised target decays by one frame.
const TARGET_DECAY_POPS: u32 = 250;

/// What the mixer should decode for the current tick.
#[derive(Debug, PartialEq, Eq)]
pub enum Playout<'a> {
    /// Decode this packet normally.
    Packet(&'a [u8]),
    /// The expected packet is missing; decode this *next* packet with `fec = true`.
    Fec(&'a [u8]),
    /// The expected packet is missing and no FEC is available; run PLC.
    Plc,
    /// Not playing (buffering or stream idle); output silence.
    Silence,
}

/// Counters for diagnostics and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JitterStats {
    /// Packets accepted into the buffer.
    pub received: u64,
    /// Packets that arrived after their playout time.
    pub late: u64,
    /// Duplicate packets.
    pub duplicate: u64,
    /// Frames recovered with FEC.
    pub fec: u64,
    /// Frames concealed with PLC.
    pub plc: u64,
    /// Frames dropped to reduce latency or on overflow.
    pub dropped: u64,
    /// Times the buffer had to resynchronise because the stream jumped ahead.
    pub resyncs: u64,
}

#[derive(Clone, Copy)]
struct Slot {
    occupied: bool,
    seq: u16,
    len: u16,
    data: [u8; MAX_PACKET],
}

const EMPTY_SLOT: Slot = Slot {
    occupied: false,
    seq: 0,
    len: 0,
    data: [0; MAX_PACKET],
};

/// Signed distance `a - b` in sequence-number space.
fn seq_diff(a: u16, b: u16) -> i32 {
    i32::from(a.wrapping_sub(b) as i16)
}

/// Adaptive jitter buffer. See the module docs.
pub struct JitterBuffer {
    slots: Box<[Slot; SLOTS]>,
    /// Next sequence number to play; `None` while (re)buffering.
    next_seq: Option<u16>,
    /// Target number of buffered frames before playout starts / to keep.
    target: u16,
    /// Floor raised by underruns, decays over time.
    underrun_floor: u16,
    good_pops: u32,
    consecutive_plc: u16,
    /// RFC 3550 jitter estimate in RTP timestamp units (48 kHz), scaled by 16.
    jitter_q4: u32,
    last_transit: Option<i64>,
    epoch: Option<Instant>,
    stats: JitterStats,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterBuffer {
    /// Creates an empty buffer.
    pub fn new() -> Self {
        Self {
            slots: Box::new([EMPTY_SLOT; SLOTS]),
            next_seq: None,
            target: MIN_TARGET_FRAMES,
            underrun_floor: MIN_TARGET_FRAMES,
            good_pops: 0,
            consecutive_plc: 0,
            jitter_q4: 0,
            last_transit: None,
            epoch: None,
            stats: JitterStats::default(),
        }
    }

    /// Current playout target in frames.
    pub fn target_frames(&self) -> u16 {
        self.computed_target()
    }

    /// Diagnostic counters.
    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    /// Current interarrival jitter estimate in milliseconds.
    pub fn jitter_ms(&self) -> f32 {
        (self.jitter_q4 as f32 / 16.0) / 48.0
    }

    /// Whether playout is running (as opposed to buffering).
    pub fn is_playing(&self) -> bool {
        self.next_seq.is_some()
    }

    /// Number of packets currently stored.
    pub fn buffered(&self) -> usize {
        self.slots.iter().filter(|s| s.occupied).count()
    }

    fn slot_index(seq: u16) -> usize {
        usize::from(seq) % SLOTS
    }

    fn get(&self, seq: u16) -> Option<&Slot> {
        let slot = &self.slots[Self::slot_index(seq)];
        (slot.occupied && slot.seq == seq).then_some(slot)
    }

    fn clear(&mut self) {
        for slot in self.slots.iter_mut() {
            slot.occupied = false;
        }
    }

    /// Inserts a packet that arrived at `arrival`.
    pub fn push(&mut self, seq: u16, timestamp: u32, payload: &[u8], arrival: Instant) {
        if payload.is_empty() || payload.len() > MAX_PACKET {
            return;
        }
        self.update_jitter(timestamp, arrival);

        if let Some(next) = self.next_seq {
            let ahead = seq_diff(seq, next);
            if ahead < 0 {
                self.stats.late += 1;
                return;
            }
            if ahead >= i32::from(MAX_FRAMES) {
                // The stream is further ahead than we may buffer: drop what we have,
                // move the playout point so this packet sits at the newest end.
                self.stats.resyncs += 1;
                let keep_from = seq.wrapping_sub(MAX_FRAMES - 1);
                for slot in self.slots.iter_mut() {
                    if slot.occupied && seq_diff(slot.seq, keep_from) < 0 {
                        slot.occupied = false;
                        self.stats.dropped += 1;
                    }
                }
                self.next_seq = Some(keep_from);
            }
        } else if let Some(oldest) = self.oldest_seq() {
            // Buffering: a packet far away from what we hold means the stream restarted.
            if seq_diff(seq, oldest).unsigned_abs() >= SLOTS as u32 {
                self.clear();
            }
        }

        let idx = Self::slot_index(seq);
        let slot = &mut self.slots[idx];
        if slot.occupied && slot.seq == seq {
            self.stats.duplicate += 1;
            return;
        }
        if slot.occupied {
            self.stats.dropped += 1;
        }
        slot.occupied = true;
        slot.seq = seq;
        slot.len = payload.len() as u16;
        slot.data[..payload.len()].copy_from_slice(payload);
        self.stats.received += 1;
    }

    fn update_jitter(&mut self, timestamp: u32, arrival: Instant) {
        let epoch = *self.epoch.get_or_insert(arrival);
        // Arrival time in 48 kHz units.
        let arrival_ts = (arrival.saturating_duration_since(epoch).as_micros() * 48 / 1000) as i64;
        let transit = arrival_ts - i64::from(timestamp);
        if let Some(last) = self.last_transit {
            let d = (transit - last).unsigned_abs().min(48_000) as u32;
            // J += (|D| - J) / 16, kept in Q4.
            let j = self.jitter_q4 as i64;
            let delta = i64::from(d) * 16 - j;
            self.jitter_q4 = (j + delta / 16).max(0) as u32;
        }
        self.last_transit = Some(transit);
    }

    fn oldest_seq(&self) -> Option<u16> {
        let mut oldest: Option<u16> = None;
        for slot in self.slots.iter().filter(|s| s.occupied) {
            oldest = Some(match oldest {
                Some(o) if seq_diff(slot.seq, o) >= 0 => o,
                _ => slot.seq,
            });
        }
        oldest
    }

    fn newest_seq(&self) -> Option<u16> {
        let mut newest: Option<u16> = None;
        for slot in self.slots.iter().filter(|s| s.occupied) {
            newest = Some(match newest {
                Some(n) if seq_diff(slot.seq, n) <= 0 => n,
                _ => slot.seq,
            });
        }
        newest
    }

    fn computed_target(&self) -> u16 {
        // Cover roughly three standard deviations of arrival jitter on top of one frame.
        let jitter_ms = self.jitter_ms();
        let from_jitter = 1 + ((jitter_ms * 3.0) / FRAME_MS as f32).ceil() as u16;
        from_jitter
            .max(self.underrun_floor)
            .clamp(MIN_TARGET_FRAMES, MAX_FRAMES)
    }

    fn recompute_target(&mut self) {
        self.target = self.computed_target();
    }

    /// Returns what to play for the next 20 ms frame.
    pub fn pop(&mut self) -> Playout<'_> {
        self.recompute_target();
        let next = match self.next_seq {
            Some(next) => next,
            None => {
                let (Some(oldest), Some(newest)) = (self.oldest_seq(), self.newest_seq()) else {
                    return Playout::Silence;
                };
                let span = seq_diff(newest, oldest) + 1;
                if span < i32::from(self.target) {
                    return Playout::Silence;
                }
                self.consecutive_plc = 0;
                oldest
            }
        };

        // Latency control: if 60 ms more than the target is buffered, skip one frame.
        let mut next = next;
        if let Some(newest) = self.newest_seq() {
            let span = seq_diff(newest, next) + 1;
            if span > i32::from(self.target) + 3 {
                let idx = Self::slot_index(next);
                if self.slots[idx].occupied && self.slots[idx].seq == next {
                    self.slots[idx].occupied = false;
                }
                self.stats.dropped += 1;
                next = next.wrapping_add(1);
            }
        }

        let idx = Self::slot_index(next);
        let following = next.wrapping_add(1);
        self.next_seq = Some(following);

        if self.slots[idx].occupied && self.slots[idx].seq == next {
            self.slots[idx].occupied = false;
            self.consecutive_plc = 0;
            self.good_pops += 1;
            if self.good_pops >= TARGET_DECAY_POPS {
                self.good_pops = 0;
                self.underrun_floor = self.underrun_floor.saturating_sub(1).max(MIN_TARGET_FRAMES);
            }
            let len = usize::from(self.slots[idx].len);
            return Playout::Packet(&self.slots[idx].data[..len]);
        }

        // Missing frame.
        self.good_pops = 0;
        if self.buffered() == 0 {
            self.consecutive_plc += 1;
            if self.consecutive_plc > MAX_CONSECUTIVE_PLC {
                // The stream went quiet: stop and re-buffer when packets return.
                self.next_seq = None;
                self.consecutive_plc = 0;
                return Playout::Silence;
            }
            // Underrun: we ran dry, so raise the playout target.
            self.underrun_floor = (self.underrun_floor + 1).min(MAX_FRAMES);
            self.stats.plc += 1;
            return Playout::Plc;
        }

        self.consecutive_plc += 1;
        if self.get(following).is_some() {
            self.stats.fec += 1;
            let fidx = Self::slot_index(following);
            let len = usize::from(self.slots[fidx].len);
            return Playout::Fec(&self.slots[fidx].data[..len]);
        }
        self.stats.plc += 1;
        Playout::Plc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn pkt(seq: u16) -> [u8; 3] {
        let b = seq.to_be_bytes();
        [b[0], b[1], 0xAA]
    }

    /// Feeds packets `seqs` arriving perfectly on time starting at `t0`.
    fn feed(jb: &mut JitterBuffer, t0: Instant, seqs: impl IntoIterator<Item = u16>) {
        for seq in seqs {
            let at = t0 + Duration::from_millis(u64::from(seq) * 20);
            jb.push(seq, u32::from(seq) * FRAME_TS, &pkt(seq), at);
        }
    }

    fn seq_of(p: &Playout<'_>) -> Option<u16> {
        match p {
            Playout::Packet(d) | Playout::Fec(d) => Some(u16::from_be_bytes([d[0], d[1]])),
            _ => None,
        }
    }

    #[test]
    fn buffers_until_target_then_plays_in_order() {
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        feed(&mut jb, t0, [0]);
        assert_eq!(jb.pop(), Playout::Silence);
        feed(&mut jb, t0, [1, 2, 3]);
        for expected in 0..4 {
            let p = jb.pop();
            assert!(matches!(p, Playout::Packet(_)));
            assert_eq!(seq_of(&p), Some(expected));
        }
    }

    #[test]
    fn reorders_out_of_order_arrivals() {
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        feed(&mut jb, t0, [1, 0, 3, 2]);
        let order: Vec<_> = (0..4).map(|_| seq_of(&jb.pop())).collect();
        assert_eq!(order, [Some(0), Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn loss_uses_fec_when_next_packet_present() {
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        feed(&mut jb, t0, [0, 1, 3, 4]);
        assert_eq!(seq_of(&jb.pop()), Some(0));
        assert_eq!(seq_of(&jb.pop()), Some(1));
        // 2 is missing, 3 is present: FEC from packet 3.
        let p = jb.pop();
        assert!(matches!(p, Playout::Fec(_)));
        assert_eq!(seq_of(&p), Some(3));
        // 3 is then played normally.
        let p = jb.pop();
        assert!(matches!(p, Playout::Packet(_)));
        assert_eq!(seq_of(&p), Some(3));
        assert_eq!(jb.stats().fec, 1);
    }

    #[test]
    fn loss_uses_plc_without_next_packet() {
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        feed(&mut jb, t0, [0, 1, 4]);
        assert_eq!(seq_of(&jb.pop()), Some(0));
        assert_eq!(seq_of(&jb.pop()), Some(1));
        assert_eq!(jb.pop(), Playout::Plc); // 2 missing, 3 missing
        assert!(matches!(jb.pop(), Playout::Fec(_))); // 3 missing, 4 present
        assert_eq!(seq_of(&jb.pop()), Some(4));
        assert_eq!(jb.stats().plc, 1);
    }

    #[test]
    fn late_packets_are_dropped() {
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        feed(&mut jb, t0, [0, 1, 2]);
        jb.pop();
        jb.pop();
        // Packet 0 again (late, already played).
        jb.push(0, 0, &pkt(0), t0 + Duration::from_millis(100));
        assert_eq!(jb.stats().late, 1);
        assert_eq!(seq_of(&jb.pop()), Some(2));
    }

    #[test]
    fn duplicates_are_ignored() {
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        feed(&mut jb, t0, [0, 1, 1, 2]);
        assert_eq!(jb.stats().duplicate, 1);
        assert_eq!(jb.buffered(), 3);
    }

    #[test]
    fn overflow_resyncs_to_newest() {
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        feed(&mut jb, t0, [0, 1]);
        assert_eq!(seq_of(&jb.pop()), Some(0));
        // The sender jumps far ahead (e.g. after a network stall).
        feed(&mut jb, t0, [40]);
        assert_eq!(jb.stats().resyncs, 1);
        // Old packet 1 was dropped; playout continues towards 40 with at most MAX_FRAMES delay.
        let mut played = None;
        for _ in 0..usize::from(MAX_FRAMES) {
            if let Some(s) = seq_of(&jb.pop()) {
                played = Some(s);
            }
        }
        assert_eq!(played, Some(40));
    }

    #[test]
    fn handles_sequence_wraparound() {
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        let seqs = [65534u16, 65535, 0, 1];
        for (i, seq) in seqs.iter().enumerate() {
            let at = t0 + Duration::from_millis(i as u64 * 20);
            jb.push(*seq, i as u32 * FRAME_TS, &pkt(*seq), at);
        }
        let order: Vec<_> = (0..4).map(|_| seq_of(&jb.pop())).collect();
        assert_eq!(order, [Some(65534), Some(65535), Some(0), Some(1)]);
    }

    #[test]
    fn target_grows_with_jitter_and_stays_bounded() {
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        // Packets every 20 ms of media but arriving with +-30 ms jitter.
        for seq in 0..200u16 {
            let wobble = if seq % 2 == 0 { 0 } else { 60 };
            let at = t0 + Duration::from_millis(u64::from(seq) * 20 + wobble);
            jb.push(seq, u32::from(seq) * FRAME_TS, &pkt(seq), at);
            jb.pop();
        }
        assert!(jb.jitter_ms() > 10.0, "jitter {}", jb.jitter_ms());
        assert!(jb.target_frames() > MIN_TARGET_FRAMES);
        assert!(jb.target_frames() <= MAX_FRAMES);
    }

    #[test]
    fn underrun_raises_target_and_idle_stream_stops() {
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        feed(&mut jb, t0, [0, 1]);
        jb.pop();
        jb.pop();
        assert_eq!(jb.pop(), Playout::Plc);
        assert!(jb.target_frames() > MIN_TARGET_FRAMES);
        // Keep popping with nothing arriving: the buffer stops and returns silence.
        let mut saw_silence = false;
        for _ in 0..10 {
            if jb.pop() == Playout::Silence {
                saw_silence = true;
            }
        }
        assert!(saw_silence);
        assert!(!jb.is_playing());
    }

    #[test]
    fn steady_stream_keeps_minimum_latency() {
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        let mut played = 0;
        for seq in 0..500u16 {
            let at = t0 + Duration::from_millis(u64::from(seq) * 20);
            jb.push(seq, u32::from(seq) * FRAME_TS, &pkt(seq), at);
            if matches!(jb.pop(), Playout::Packet(_)) {
                played += 1;
            }
        }
        assert_eq!(jb.target_frames(), MIN_TARGET_FRAMES);
        assert!(played >= 495, "played {played}");
        assert!(jb.buffered() <= usize::from(MIN_TARGET_FRAMES));
    }

    #[test]
    fn rejects_oversized_and_empty_payloads() {
        let mut jb = JitterBuffer::new();
        let now = Instant::now();
        jb.push(0, 0, &[], now);
        jb.push(1, 960, &[0u8; MAX_PACKET + 1], now);
        assert_eq!(jb.buffered(), 0);
    }
}
