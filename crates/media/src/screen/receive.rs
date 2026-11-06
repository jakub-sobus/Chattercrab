//! Receiving side of screen share: RTP to H.264 access units to RGBA frames.
//!
//! An RTP reader task depacketizes the remote H.264 track into access units (one per
//! marker bit), drops pictures after loss until the next IDR, and asks the publisher for
//! a keyframe with PLI (at most every 500 ms). Access units go through a bounded channel
//! of 8 to a decoder thread, which converts decoded pictures to RGBA and pushes them into
//! the [`VideoFrameQueue`] (capacity 2, oldest dropped).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::packetizer::Depacketizer;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};

use crate::{MediaError, RgbaFrame, VideoFrameQueue};

/// Access units buffered between the RTP reader and the decoder.
const AU_CHANNEL_CAPACITY: usize = 8;
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

/// Decodes one access unit; returns `Ok(None)` when the decoder needs more data.
pub fn decode_to_rgba(decoder: &mut Decoder, au: &[u8]) -> Result<Option<RgbaFrame>, MediaError> {
    let Some(yuv) = decoder
        .decode(au)
        .map_err(|e| MediaError::Codec(format!("h264 decode: {e}")))?
    else {
        return Ok(None);
    };
    let (w, h) = yuv.dimensions();
    let mut pixels = vec![0u8; w * h * 4];
    yuv.write_rgba8(&mut pixels);
    Ok(Some(RgbaFrame {
        width: w as u32,
        height: h as u32,
        pixels: Arc::from(pixels),
    }))
}

/// Reads `track` until it ends, decoding into `queue`. Returns when the track ends.
pub async fn run_receiver(track: Arc<dyn TrackRemote>, queue: VideoFrameQueue) {
    let (tx, rx) = sync_channel::<Vec<u8>>(AU_CHANNEL_CAPACITY);
    let want_keyframe = Arc::new(AtomicBool::new(true));
    let decoder_flag = want_keyframe.clone();
    let decoder = std::thread::Builder::new()
        .name("cc-screen-decoder".into())
        .spawn(move || decode_loop(rx, queue, decoder_flag));
    if let Err(e) = decoder {
        tracing::warn!("spawn decoder thread: {e}");
        return;
    }
    read_loop(track, tx, want_keyframe).await;
}

async fn read_loop(
    track: Arc<dyn TrackRemote>,
    tx: SyncSender<Vec<u8>>,
    want_keyframe: Arc<AtomicBool>,
) {
    let mut depacketizer = H264Packet::default();
    let mut au: Vec<u8> = Vec::with_capacity(256 * 1024);
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
            let complete = !corrupt && !au.is_empty();
            if complete && waiting_for_idr && contains_idr(&au) {
                waiting_for_idr = false;
            }
            if complete && !waiting_for_idr {
                match tx.try_send(std::mem::replace(&mut au, Vec::with_capacity(64 * 1024))) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => waiting_for_idr = true,
                    Err(TrySendError::Disconnected(_)) => return,
                }
            }
            au.clear();
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

fn decode_loop(rx: Receiver<Vec<u8>>, queue: VideoFrameQueue, want_keyframe: Arc<AtomicBool>) {
    let mut decoder = match new_decoder() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("{e}");
            return;
        }
    };
    while let Ok(au) = rx.recv() {
        match decode_to_rgba(&mut decoder, &au) {
            Ok(Some(frame)) => queue.push(frame),
            Ok(None) => {}
            Err(e) => {
                tracing::debug!("{e}");
                want_keyframe.store(true, Ordering::Relaxed);
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
}
