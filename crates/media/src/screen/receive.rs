//! Receiving side of screen share: RTP to H.264 access units to NV12 frames.
//!
//! An RTP reader task depacketizes the remote H.264 track into access units (one per
//! marker bit), drops pictures after loss until the next IDR, and asks the publisher for
//! a keyframe with PLI (at most every 500 ms). Access units wait in a playout buffer of
//! up to one second. The decoder thread shows them on the RTP clock, so a burst of
//! packets becomes a steady picture instead of a stutter. A viewer that falls further
//! than that second behind skips pictures (still decoding them, so the stream stays
//! valid) until it is inside the window again.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::packetizer::Depacketizer;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};

use super::playout::{AuBuffer, MAX_PLAYOUT_DELAY, PushError, SharedClock, Step, TimedAu, Wait};
use crate::{MediaError, Nv12Frame, VideoFrameQueue};

/// Minimum interval between PLIs.
const PLI_INTERVAL: Duration = Duration::from_millis(500);
/// Upper bound for one access unit; larger ones are dropped.
const MAX_AU_BYTES: usize = 8 * 1024 * 1024;

/// Whether an Annex-B buffer contains an IDR slice (NAL type 5).
pub fn contains_idr(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if au[i + 3] & 0x1F == 5 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// Creates an OpenH264 decoder.
pub fn new_decoder() -> Result<Decoder, MediaError> {
    Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
        .map_err(|e| MediaError::Codec(format!("openh264 decoder: {e}")))
}

/// Decodes one access unit into NV12. Returns `Ok(None)` when the decoder needs more data.
pub fn decode_frame(decoder: &mut Decoder, au: &[u8]) -> Result<Option<Nv12Frame>, MediaError> {
    let Some(yuv) = decoder
        .decode(au)
        .map_err(|e| MediaError::Codec(format!("h264 decode: {e}")))?
    else {
        return Ok(None);
    };
    Ok(Some(nv12_from_yuv(&yuv)))
}

fn nv12_from_yuv(yuv: &impl YUVSource) -> Nv12Frame {
    let (width, height) = yuv.dimensions();
    let (y_stride, u_stride, v_stride) = yuv.strides();
    let y_plane = yuv.y();
    let u_plane = yuv.u();
    let v_plane = yuv.v();
    let mut y = Vec::with_capacity(width * height);
    for row in 0..height {
        let start = row * y_stride;
        y.extend_from_slice(&y_plane[start..start + width]);
    }
    let chroma_w = width / 2;
    let chroma_h = height / 2;
    let mut uv = Vec::with_capacity(chroma_w * chroma_h * 2);
    for row in 0..chroma_h {
        for col in 0..chroma_w {
            uv.push(u_plane[row * u_stride + col]);
            uv.push(v_plane[row * v_stride + col]);
        }
    }
    Nv12Frame {
        width: width as u32,
        height: height as u32,
        y_stride: width as u32,
        uv_stride: width as u32,
        y: Arc::from(y),
        uv: Arc::from(uv),
        seq: Nv12Frame::next_seq(),
    }
}

struct PictureDecoder {
    #[cfg(windows)]
    hardware: Option<super::hw_decode::HwDecoder>,
    software: Option<Decoder>,
}

impl PictureDecoder {
    fn open() -> Result<Self, MediaError> {
        #[cfg(windows)]
        match super::hw_decode::HwDecoder::open() {
            Ok(decoder) => {
                return Ok(Self {
                    hardware: Some(decoder),
                    software: None,
                });
            }
            Err(error) => {
                tracing::warn!(%error, "hardware h264 decoder unavailable, using openh264");
            }
        }
        Ok(Self {
            #[cfg(windows)]
            hardware: None,
            software: Some(new_decoder()?),
        })
    }

    fn is_hardware(&self) -> bool {
        #[cfg(windows)]
        {
            self.hardware.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    fn decode(&mut self, au: &[u8], present: bool) -> Result<Option<Nv12Frame>, MediaError> {
        #[cfg(windows)]
        let hardware_failed = if let Some(hw) = self.hardware.as_mut() {
            match hw.decode(au, present) {
                Ok(frame) => return Ok(frame),
                Err(error) => {
                    tracing::warn!(%error, "hardware h264 decode failed, using openh264");
                    true
                }
            }
        } else {
            false
        };
        #[cfg(windows)]
        if hardware_failed {
            self.hardware = None;
            self.software = Some(new_decoder()?);
        }
        let Some(decoder) = self.software.as_mut() else {
            return Err(MediaError::Codec("no h264 decoder".into()));
        };
        if !present {
            decode_discard(decoder, au)?;
            return Ok(None);
        }
        let started = Instant::now();
        let frame = decode_frame(decoder, au)?;
        let took = started.elapsed();
        if took > Duration::from_millis(12) {
            slow_software_decode(took);
        }
        Ok(frame)
    }
}

fn slow_software_decode(took: Duration) {
    use std::cell::Cell;
    thread_local! {
        static LAST: Cell<Option<Instant>> = const { Cell::new(None) };
    }
    let overdue = LAST.with(|last| {
        last.get()
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(1))
    });
    if overdue {
        LAST.with(|last| last.set(Some(Instant::now())));
        tracing::warn!(?took, "software h264 decode missed the frame budget");
    }
}

fn decode_discard(decoder: &mut Decoder, au: &[u8]) -> Result<(), MediaError> {
    decoder
        .decode(au)
        .map(|_| ())
        .map_err(|e| MediaError::Codec(format!("h264 decode: {e}")))
}

/// Reads `track` until it ends, decoding into `queue`. Returns when the track ends.
pub async fn run_receiver(track: Arc<dyn TrackRemote>, queue: VideoFrameQueue) {
    queue.clear();
    let buffer = AuBuffer::new();
    let clock = SharedClock::new();
    let want_keyframe = Arc::new(AtomicBool::new(true));
    let decoder_buffer = Arc::clone(&buffer);
    let decoder_clock = clock.clone();
    let decoder_flag = Arc::clone(&want_keyframe);
    let decoder = std::thread::Builder::new()
        .name("cc-screen-decoder".into())
        .spawn(move || decode_loop(decoder_buffer, decoder_clock, queue, decoder_flag));
    if let Err(e) = decoder {
        tracing::warn!("spawn decoder thread: {e}");
        return;
    }
    read_loop(track, &buffer, &clock, &want_keyframe).await;
    buffer.close();
}

async fn read_loop(
    track: Arc<dyn TrackRemote>,
    buffer: &AuBuffer,
    clock: &SharedClock,
    want_keyframe: &AtomicBool,
) {
    let mut depacketizer = H264Packet::default();
    let mut au: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut au_ts: Option<u32> = None;
    let mut last_seq: Option<u16> = None;
    let mut last_pli: Option<Instant> = None;
    let mut waiting_for_idr = true;
    let mut corrupt = false;

    while let Some(event) = track.poll().await {
        let pkt = match event {
            TrackRemoteEvent::OnRtpPacket(pkt) => pkt,
            TrackRemoteEvent::OnEnded => break,
            _ => continue,
        };
        let seq = pkt.header.sequence_number;
        if let Some(prev) = last_seq
            && seq != prev.wrapping_add(1)
        {
            // Loss: the current picture is unusable and references will be broken.
            corrupt = true;
            waiting_for_idr = true;
        }
        last_seq = Some(seq);
        if au_ts.is_none() {
            au_ts = Some(pkt.header.timestamp);
        }

        match depacketizer.depacketize(&pkt.payload) {
            Ok(bytes) => {
                if au.len() + bytes.len() <= MAX_AU_BYTES {
                    au.extend_from_slice(&bytes);
                } else {
                    corrupt = true;
                }
            }
            Err(_) => corrupt = true,
        }

        if pkt.header.marker {
            let timestamp = au_ts.take().unwrap_or(pkt.header.timestamp);
            let complete = !corrupt && !au.is_empty();
            let keyframe = complete && contains_idr(&au);
            if complete && waiting_for_idr && keyframe {
                waiting_for_idr = false;
            }
            if complete && !waiting_for_idr {
                let arrival = Instant::now();
                clock.observe(timestamp, arrival);
                let timed = TimedAu {
                    data: std::mem::replace(&mut au, Vec::with_capacity(64 * 1024)),
                    rtp_ts: timestamp,
                    keyframe,
                };
                match buffer.push(timed) {
                    Ok(()) => {}
                    Err(PushError::Overflow) => {
                        tracing::debug!("screen playout buffer overflowed; waiting for a keyframe");
                        waiting_for_idr = true;
                    }
                    Err(PushError::Closed) => return,
                }
            }
            au.clear();
            au_ts = None;
            corrupt = false;
        }

        if (waiting_for_idr || want_keyframe.load(Ordering::Relaxed))
            && last_pli.is_none_or(|t| t.elapsed() >= PLI_INTERVAL)
        {
            last_pli = Some(Instant::now());
            want_keyframe.store(false, Ordering::Relaxed);
            let pli = PictureLossIndication {
                sender_ssrc: 0,
                media_ssrc: pkt.header.ssrc,
            };
            let _ = track.write_rtcp(vec![Box::new(pli)]).await;
        }
    }
}

fn decode_loop(
    buffer: Arc<AuBuffer>,
    clock: SharedClock,
    queue: VideoFrameQueue,
    want_keyframe: Arc<AtomicBool>,
) {
    let mut decoder = match PictureDecoder::open() {
        Ok(decoder) => decoder,
        Err(e) => {
            tracing::warn!("{e}");
            return;
        }
    };
    let mut generation = 0u64;
    let mut need_idr = false;
    let mut logged_start = false;
    let mut empty_since: Option<Instant> = None;
    let mut pace = DecodePace::new(decoder.is_hardware());
    loop {
        pace.tick(decoder.is_hardware());
        if buffer.take_overflow() {
            pace.underruns = pace.underruns.saturating_add(1);
            need_idr = true;
            want_keyframe.store(true, Ordering::Relaxed);
            clock.with_mut(super::playout::PlayoutClock::underrun);
            decoder = match PictureDecoder::open() {
                Ok(decoder) => decoder,
                Err(e) => {
                    tracing::warn!("{e}");
                    return;
                }
            };
        }

        let timeout = clock.with_mut(|clock| clock.frame_duration());
        let peeked = match buffer.wait_generation(generation, timeout) {
            Wait::Closed => return,
            Wait::Overflow => continue,
            Wait::Timeout { queued: None } => {
                let (started, cushion) =
                    clock.with_mut(|clock| (clock.is_started(), clock.delay()));
                if started {
                    let since = empty_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= cushion {
                        empty_since = None;
                        pace.underruns = pace.underruns.saturating_add(1);
                        clock.with_mut(super::playout::PlayoutClock::underrun);
                        tracing::debug!("screen playout underrun; buffering again");
                    }
                }
                continue;
            }
            Wait::Timeout { queued: Some(peek) } | Wait::Ready(peek) => {
                empty_since = None;
                peek
            }
        };
        generation = peeked.generation;

        if need_idr && !peeked.keyframe {
            let _ = buffer.pop();
            continue;
        }
        if need_idr {
            need_idr = false;
            clock.with_mut(super::playout::PlayoutClock::reset);
            decoder = match PictureDecoder::open() {
                Ok(decoder) => decoder,
                Err(e) => {
                    tracing::warn!("{e}");
                    return;
                }
            };
        }

        let (decision, skipped_before) = clock.with_mut(|clock| {
            let before = clock.skipped();
            (
                clock.step(peeked.oldest, peeked.newest, Instant::now()),
                before,
            )
        });
        match decision {
            Step::Buffer => continue,
            Step::DecodeOnly => {
                pace.skipped = pace.skipped.saturating_add(1);
                let (skipped, delay) = clock.with_mut(|clock| (clock.skipped(), clock.delay()));
                if skipped != skipped_before {
                    if skipped == 1 || skipped.is_multiple_of(60) {
                        tracing::debug!(
                            skipped,
                            "screen playout skipped a frame to stay within 1s"
                        );
                    }
                } else {
                    tracing::trace!(
                        delay_ms = delay.as_millis(),
                        "screen playout skipped a frame to lower its delay"
                    );
                }
                if let Some(au) = buffer.pop()
                    && decoder.decode(&au.data, false).is_err()
                {
                    need_idr = true;
                    want_keyframe.store(true, Ordering::Relaxed);
                }
            }
            Step::Present { at } => {
                if !logged_start {
                    logged_start = true;
                    let delay = clock.with_mut(|clock| clock.delay());
                    tracing::debug!(delay_ms = delay.as_millis(), "screen playout started");
                }
                let Some(au) = buffer.pop() else {
                    continue;
                };
                if !present_frame(
                    &buffer,
                    &clock,
                    &queue,
                    &mut decoder,
                    &mut pace,
                    &want_keyframe,
                    au,
                    at,
                ) {
                    if buffer.is_closed() {
                        return;
                    }
                    need_idr = true;
                }
            }
        }
    }
}

/// Decodes `au` and hands it to the UI at `at`.
/// Returns false when the picture could not be decoded or the receiver stopped.
#[allow(clippy::too_many_arguments)]
fn present_frame(
    buffer: &AuBuffer,
    clock: &SharedClock,
    queue: &VideoFrameQueue,
    decoder: &mut PictureDecoder,
    pace: &mut DecodePace,
    want_keyframe: &AtomicBool,
    au: TimedAu,
    at: Instant,
) -> bool {
    let frame_len = clock.with_mut(|clock| clock.frame_duration());
    let lead = decode_lead(frame_len);
    let decode_at = at.checked_sub(lead).unwrap_or(at);
    if !buffer.sleep_until(decode_at) {
        return false;
    }
    let started = Instant::now();
    let decoded = decoder.decode(&au.data, true);
    pace.last_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let frame = match decoded {
        Ok(Some(frame)) => {
            pace.shown = pace.shown.saturating_add(1);
            frame
        }
        Ok(None) => {
            pace.empty = pace.empty.saturating_add(1);
            return true;
        }
        Err(e) => {
            pace.failed = pace.failed.saturating_add(1);
            tracing::debug!("{e}");
            want_keyframe.store(true, Ordering::Relaxed);
            return false;
        }
    };
    if !buffer.sleep_until(at) {
        return false;
    }
    match enqueue_frame(queue, frame, buffer, at) {
        Enqueue::Queued(shown) => {
            // A frame that is only a little late stays on the RTP grid. Stamping
            // the actual time every frame would add that lateness again and
            // the delay would grow until we started skipping.
            let late = shown.saturating_duration_since(at);
            let stamp = if late > frame_len / 2 { shown } else { at };
            clock.with_mut(|clock| clock.did_show(stamp));
            true
        }
        Enqueue::Skipped => {
            // The UI is still busy and we are already a second behind. Keep the
            // picture it has; the next step skips until the buffer fits again.
            pace.held = pace.held.saturating_add(1);
            clock.with_mut(|clock| clock.did_show(Instant::now()));
            true
        }
        Enqueue::Closed => false,
    }
}

struct DecodePace {
    at: Instant,
    shown: u32,
    empty: u32,
    skipped: u32,
    held: u32,
    failed: u32,
    underruns: u32,
    last_ms: u64,
    hardware: bool,
}

impl DecodePace {
    fn new(hardware: bool) -> Self {
        Self {
            at: Instant::now(),
            shown: 0,
            empty: 0,
            skipped: 0,
            held: 0,
            failed: 0,
            underruns: 0,
            last_ms: 0,
            hardware,
        }
    }

    fn tick(&mut self, hardware: bool) {
        self.hardware = hardware;
        if self.at.elapsed() < Duration::from_secs(2) {
            return;
        }
        let secs = self.at.elapsed().as_secs_f32().max(0.001);
        tracing::info!(
            fps = self.shown as f32 / secs,
            empty = self.empty,
            skipped = self.skipped,
            held = self.held,
            failed = self.failed,
            underruns = self.underruns,
            decode_ms = self.last_ms,
            hardware,
            "screen decode pace"
        );
        *self = Self::new(hardware);
    }
}

fn decode_lead(frame: Duration) -> Duration {
    (frame * 2).clamp(Duration::from_millis(8), Duration::from_millis(50))
}

enum Enqueue {
    Queued(Instant),
    Skipped,
    Closed,
}

fn enqueue_frame(
    queue: &VideoFrameQueue,
    frame: Nv12Frame,
    buffer: &AuBuffer,
    planned: Instant,
) -> Enqueue {
    let mut frame = Some(frame);
    let deadline = planned + MAX_PLAYOUT_DELAY;
    loop {
        if buffer.is_closed() {
            return Enqueue::Closed;
        }
        let Some(pending) = frame.take() else {
            return Enqueue::Skipped;
        };
        match queue.try_push(pending) {
            Ok(()) => return Enqueue::Queued(Instant::now()),
            Err(returned) => {
                frame = Some(returned);
                if buffer.span() > MAX_PLAYOUT_DELAY || Instant::now() >= deadline {
                    return Enqueue::Skipped;
                }
                let pause = Instant::now() + Duration::from_millis(4);
                if !buffer.sleep_until(pause) {
                    return Enqueue::Closed;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idr_detection() {
        assert!(contains_idr(&[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 9]));
        assert!(!contains_idr(&[0, 0, 0, 1, 0x41, 1, 2, 3]));
        assert!(!contains_idr(&[]));
    }

    #[test]
    fn decode_lead_stays_inside_one_frame_budget() {
        assert_eq!(
            decode_lead(Duration::from_millis(16)),
            Duration::from_millis(32)
        );
        assert_eq!(
            decode_lead(Duration::from_millis(8)),
            Duration::from_millis(16)
        );
        assert_eq!(
            decode_lead(Duration::from_millis(100)),
            Duration::from_millis(50)
        );
    }
}
