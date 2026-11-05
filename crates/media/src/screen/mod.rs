//! Screen sharing: capture and H.264 encode (publisher) and H.264 decode (receiver).
//!
//! Publisher: a dedicated thread captures the selected monitor or window with `xcap`,
//! downscales to fit 1280x720 with `fast_image_resize`, converts to YUV 4:2:0 and encodes
//! with OpenH264 (constrained baseline, about 1.5 Mbps, IDR every 2 s and whenever a peer
//! sends PLI/FIR). Encoded access units (Annex-B) leave through a bounded channel.
//!
//! Receiver: see [`receive`].

pub mod receive;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, Profile, RateControlMode,
    UsageType,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer};
use tokio::sync::mpsc;

use crate::{MediaError, ScreenSource, ScreenSourceKind};

/// Maximum encoded width.
pub const MAX_WIDTH: u32 = 1280;
/// Maximum encoded height.
pub const MAX_HEIGHT: u32 = 720;
/// Target bitrate.
pub const BITRATE_BPS: u32 = 1_500_000;
/// Default frame rate.
pub const DEFAULT_FPS: u32 = 15;
/// Allowed frame-rate range.
pub const FPS_RANGE: std::ops::RangeInclusive<u32> = 5..=30;
/// Encoded frames buffered towards the network.
const VIDEO_CHANNEL_CAPACITY: usize = 4;
/// Consecutive capture failures after which publishing stops.
const MAX_CAPTURE_FAILURES: u32 = 30;

/// One encoded access unit.
#[derive(Debug, Clone)]
pub struct EncodedVideo {
    /// Annex-B NAL units of one picture.
    pub data: Bytes,
    /// Presentation duration (1 / fps).
    pub duration: Duration,
    /// Whether this is an IDR picture.
    pub keyframe: bool,
}

/// Lists capturable monitors and windows.
pub fn list_sources() -> Result<Vec<ScreenSource>, MediaError> {
    let mut out = Vec::new();
    let monitors = xcap::Monitor::all().map_err(|e| MediaError::Screen(e.to_string()))?;
    for m in monitors {
        let Ok(id) = m.id() else { continue };
        let name = m
            .friendly_name()
            .or_else(|_| m.name())
            .unwrap_or_else(|_| format!("Monitor {id}"));
        let dims = match (m.width(), m.height()) {
            (Ok(w), Ok(h)) => format!(" ({w}x{h})"),
            _ => String::new(),
        };
        out.push(ScreenSource {
            id,
            kind: ScreenSourceKind::Monitor,
            name: format!("Screen: {name}{dims}"),
        });
    }
    // Window enumeration is optional (it may be unsupported, e.g. on some Wayland setups).
    if let Ok(windows) = xcap::Window::all() {
        for w in windows {
            if w.is_minimized().unwrap_or(true) {
                continue;
            }
            let (Ok(id), Ok(title)) = (w.id(), w.title()) else {
                continue;
            };
            if title.trim().is_empty() {
                continue;
            }
            let app = w.app_name().unwrap_or_default();
            let name = if app.is_empty() || title.contains(&app) {
                title
            } else {
                format!("{app}: {title}")
            };
            out.push(ScreenSource {
                id,
                kind: ScreenSourceKind::Window,
                name: format!("Window: {name}"),
            });
        }
    }
    Ok(out)
}

/// Largest even size that fits within `max_w` x `max_h`, preserving aspect ratio.
pub fn fit_dimensions(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (2, 2);
    }
    let scale = (f64::from(max_w) / f64::from(w))
        .min(f64::from(max_h) / f64::from(h))
        .min(1.0);
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(f64::from(w) * scale), even(f64::from(h) * scale))
}

enum Target {
    Monitor(xcap::Monitor),
    Window(xcap::Window),
}

impl Target {
    fn open(source: &ScreenSource) -> Result<Self, MediaError> {
        let not_found = || MediaError::Screen(format!("source not found: {}", source.name));
        match source.kind {
            ScreenSourceKind::Monitor => xcap::Monitor::all()
                .map_err(|e| MediaError::Screen(e.to_string()))?
                .into_iter()
                .find(|m| m.id().ok() == Some(source.id))
                .map(Target::Monitor)
                .ok_or_else(not_found),
            ScreenSourceKind::Window => xcap::Window::all()
                .map_err(|e| MediaError::Screen(e.to_string()))?
                .into_iter()
                .find(|w| w.id().ok() == Some(source.id))
                .map(Target::Window)
                .ok_or_else(not_found),
        }
    }

    fn capture(&self) -> Result<xcap::image::RgbaImage, MediaError> {
        match self {
            Target::Monitor(m) => m.capture_image(),
            Target::Window(w) => w.capture_image(),
        }
        .map_err(|e| MediaError::Screen(e.to_string()))
    }
}

/// A running screen publisher. Dropping it stops the capture thread.
pub struct ScreenPublisher {
    /// Encoded access units, bounded.
    pub frames: mpsc::Receiver<EncodedVideo>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for ScreenPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenPublisher").finish_non_exhaustive()
    }
}

impl ScreenPublisher {
    /// Opens `source` and starts capturing at `fps` (clamped to 5..=30). `keyframe` is
    /// polled every frame; when set, the next frame is an IDR. Blocks until the source
    /// is opened.
    pub fn start(
        source: &ScreenSource,
        fps: u32,
        keyframe: Arc<AtomicBool>,
    ) -> Result<Self, MediaError> {
        let fps = fps.clamp(*FPS_RANGE.start(), *FPS_RANGE.end());
        let (tx, rx) = mpsc::channel(VIDEO_CHANNEL_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<(), MediaError>>(1);
        let source_owned = source.clone();
        let thread_stop = stop.clone();
        // Capture handles are not `Send` on every platform, so the source is opened on the
        // capture thread itself and the result reported back.
        let thread = std::thread::Builder::new()
            .name("cc-screen-capture".into())
            .spawn(move || {
                let opened = Target::open(&source_owned).and_then(|target| {
                    let first = target.capture()?;
                    let encoder = new_encoder(fps)?;
                    Ok((target, first, encoder))
                });
                match opened {
                    Ok((target, first, encoder)) => {
                        let _ = ready_tx.send(Ok(()));
                        CaptureWorker {
                            target,
                            fps,
                            encoder,
                            keyframe,
                            tx,
                            stop: thread_stop,
                        }
                        .run(first);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| MediaError::Screen(format!("spawn capture thread: {e}")))?;
        let publisher = Self {
            frames: rx,
            stop,
            thread: Some(thread),
        };
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {
                tracing::info!(source = %source.name, fps, "screen publisher started");
                Ok(publisher)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                publisher.stop.store(true, Ordering::Release);
                Err(MediaError::Screen(
                    "screen source did not open in time".into(),
                ))
            }
        }
    }
}

impl Drop for ScreenPublisher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn new_encoder(fps: u32) -> Result<Encoder, MediaError> {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(BITRATE_BPS))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .profile(Profile::Baseline)
        .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 2))
        .skip_frames(true);
    Encoder::with_api_config(OpenH264API::from_source(), config)
        .map_err(|e| MediaError::Codec(format!("openh264 encoder: {e}")))
}

struct CaptureWorker {
    target: Target,
    fps: u32,
    encoder: Encoder,
    keyframe: Arc<AtomicBool>,
    tx: mpsc::Sender<EncodedVideo>,
    stop: Arc<AtomicBool>,
}

impl CaptureWorker {
    fn run(mut self, first: xcap::image::RgbaImage) {
        let period = Duration::from_secs_f64(1.0 / f64::from(self.fps));
        let started = Instant::now();
        let mut resizer = Resizer::new();
        let options = ResizeOptions::new()
            .resize_alg(ResizeAlg::Convolution(FilterType::Bilinear))
            .use_alpha(false);
        let mut scaled: Option<Image<'static>> = None;
        let mut yuv: Option<YUVBuffer> = None;
        let mut bitstream = Vec::with_capacity(256 * 1024);
        let mut failures = 0u32;
        let mut pending = Some(first);
        let mut next = Instant::now();

        while !self.stop.load(Ordering::Acquire) {
            let image = match pending.take().map_or_else(|| self.target.capture(), Ok) {
                Ok(img) => {
                    failures = 0;
                    img
                }
                Err(e) => {
                    failures += 1;
                    if failures >= MAX_CAPTURE_FAILURES {
                        tracing::warn!("screen capture keeps failing, stopping: {e}");
                        return;
                    }
                    std::thread::sleep(period);
                    continue;
                }
            };
            let (sw, sh) = image.dimensions();
            let (tw, th) = fit_dimensions(sw, sh, MAX_WIDTH, MAX_HEIGHT);
            let rgba: &[u8] = if (tw, th) == (sw, sh) {
                image.as_raw()
            } else {
                let dst = match scaled.as_mut() {
                    Some(img) if img.width() == tw && img.height() == th => img,
                    _ => scaled.insert(Image::new(tw, th, PixelType::U8x4)),
                };
                let src = match ImageRef::new(sw, sh, image.as_raw(), PixelType::U8x4) {
                    Ok(src) => src,
                    Err(e) => {
                        tracing::warn!("screen frame layout: {e}");
                        continue;
                    }
                };
                if let Err(e) = resizer.resize(&src, dst, &options) {
                    tracing::warn!("screen resize: {e}");
                    continue;
                }
                dst.buffer()
            };
            let (w, h) = (tw as usize, th as usize);
            let buf = match yuv.as_mut() {
                Some(b) if openh264::formats::YUVSource::dimensions(b) == (w, h) => b,
                _ => yuv.insert(YUVBuffer::new(w, h)),
            };
            buf.read_rgba8(RgbaSliceU8::new(rgba, (w, h)));

            if self.keyframe.swap(false, Ordering::Relaxed) {
                self.encoder.force_intra_frame();
            }
            let ts = openh264::Timestamp::from_millis(started.elapsed().as_millis() as u64);
            match self.encoder.encode_at(&*buf, ts) {
                Ok(bs) => {
                    let keyframe = matches!(bs.frame_type(), openh264::encoder::FrameType::IDR);
                    bitstream.clear();
                    bs.write_vec(&mut bitstream);
                    if !bitstream.is_empty() {
                        let frame = EncodedVideo {
                            data: Bytes::copy_from_slice(&bitstream),
                            duration: period,
                            keyframe,
                        };
                        match self.tx.try_send(frame) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                // The network side is behind; the next IDR will resync.
                                self.keyframe.store(true, Ordering::Relaxed);
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => return,
                        }
                    }
                }
                Err(e) => tracing::warn!("h264 encode: {e}"),
            }

            next += period;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_and_even_sizes() {
        assert_eq!(fit_dimensions(1920, 1080, 1280, 720), (1280, 720));
        assert_eq!(fit_dimensions(3840, 2160, 1280, 720), (1280, 720));
        assert_eq!(fit_dimensions(1280, 1024, 1280, 720), (900, 720));
        assert_eq!(fit_dimensions(800, 600, 1280, 720), (800, 600));
        assert_eq!(fit_dimensions(801, 601, 1280, 720), (800, 600));
        assert_eq!(fit_dimensions(0, 10, 1280, 720), (2, 2));
        let (w, h) = fit_dimensions(5120, 1440, 1280, 720);
        assert!(w <= 1280 && h <= 720 && w % 2 == 0 && h % 2 == 0);
    }

    #[test]
    fn encoder_produces_idr_then_decodes() {
        let mut enc = new_encoder(15).unwrap();
        let (w, h) = (320usize, 240usize);
        let mut rgba = vec![0u8; w * h * 4];
        for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *px = [(i % 255) as u8, ((i / w) % 255) as u8, 128, 255];
        }
        let mut yuv = YUVBuffer::new(w, h);
        yuv.read_rgba8(RgbaSliceU8::new(&rgba, (w, h)));
        let bs = enc.encode(&yuv).unwrap();
        assert!(matches!(bs.frame_type(), openh264::encoder::FrameType::IDR));
        let au = bs.to_vec();
        assert!(receive::contains_idr(&au));

        let mut dec = receive::new_decoder().unwrap();
        let frame = receive::decode_to_rgba(&mut dec, &au).unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (320, 240));
        assert_eq!(frame.pixels.len(), 320 * 240 * 4);
    }
}
