//! Audio playback: cpal output stream fed from an SPSC ring.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use super::resample::MonoResampler;
use super::{FRAME_SAMPLES, SAMPLE_RATE, device_name, find_output_device};
use crate::MediaError;

/// Output ring capacity in milliseconds.
const RING_MS: u32 = 200;

/// A running output stream. Dropping it stops playback.
pub struct AudioOutput {
    /// Name of the opened device.
    pub device_name: String,
    /// Device sample rate.
    pub device_rate: u32,
    /// Samples the callback had to fill with silence because the ring was empty.
    pub underruns: Arc<AtomicU64>,
    _stream: cpal::Stream,
}

impl std::fmt::Debug for AudioOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioOutput")
            .field("device_name", &self.device_name)
            .field("device_rate", &self.device_rate)
            .finish_non_exhaustive()
    }
}

/// Producer side of the playback ring, owned by the mixer thread. Accepts 20 ms frames
/// at 48 kHz and converts them to the device rate.
pub struct OutputSink {
    producer: HeapProd<f32>,
    device_rate: u32,
    resampler: Option<MonoResampler>,
    scratch: Box<[f32; FRAME_SAMPLES]>,
    /// Largest number of frames the device callback has taken at once.
    callback_frames: Arc<AtomicU32>,
}

impl OutputSink {
    /// Creates a sink writing into `producer` at `device_rate`.
    pub fn new(producer: HeapProd<f32>, device_rate: u32) -> Result<Self, MediaError> {
        let resampler = if device_rate == SAMPLE_RATE {
            None
        } else {
            Some(MonoResampler::new(SAMPLE_RATE, device_rate, FRAME_SAMPLES)?)
        };
        Ok(Self {
            producer,
            device_rate,
            resampler,
            scratch: Box::new([0.0; FRAME_SAMPLES]),
            callback_frames: Arc::default(),
        })
    }

    /// Longest device callback seen so far, in milliseconds (0 before the first one).
    pub fn callback_ms(&self) -> u32 {
        let frames = u64::from(self.callback_frames.load(Ordering::Relaxed));
        (frames * 1000).div_ceil(u64::from(self.device_rate)) as u32
    }

    /// Audio currently queued for the device, in milliseconds.
    pub fn buffered_ms(&self) -> u32 {
        (self.producer.occupied_len() as u64 * 1000 / u64::from(self.device_rate)) as u32
    }

    /// Writes one 20 ms frame of 48 kHz samples. Returns samples dropped (ring full).
    pub fn write_frame(&mut self, frame: &[i16; FRAME_SAMPLES]) -> usize {
        for (dst, &src) in self.scratch.iter_mut().zip(frame.iter()) {
            *dst = f32::from(src) / 32768.0;
        }
        let out: &[f32] = match self.resampler.as_mut() {
            Some(r) => match r.process(&self.scratch[..]) {
                Ok(out) => out,
                Err(_) => return FRAME_SAMPLES,
            },
            None => &self.scratch[..],
        };
        out.len() - self.producer.push_slice(out)
    }
}

impl AudioOutput {
    /// Opens the output device (`None` = default) and returns the stream plus the sink
    /// the mixer writes into. Blocks while the device opens.
    pub fn start(device: Option<&str>) -> Result<(Self, OutputSink), MediaError> {
        let device = find_output_device(device)?;
        let name = device_name(&device);
        let supported = device
            .default_output_config()
            .map_err(|e| MediaError::Audio(format!("{name}: {e}")))?;
        let config = supported.config();
        let rate = config.sample_rate;
        let channels = usize::from(config.channels.max(1));

        let ring = HeapRb::<f32>::new((rate * RING_MS / 1000) as usize);
        let (prod, cons) = ring.split();
        let sink = OutputSink::new(prod, rate)?;
        let underruns = Arc::new(AtomicU64::new(0));
        let err_name = name.clone();
        let on_error =
            move |e: cpal::Error| tracing::warn!(device = %err_name, "output stream error: {e}");
        let u = (underruns.clone(), sink.callback_frames.clone());
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => build::<f32>(&device, &config, channels, cons, u, on_error),
            cpal::SampleFormat::I16 => build::<i16>(&device, &config, channels, cons, u, on_error),
            cpal::SampleFormat::I32 => build::<i32>(&device, &config, channels, cons, u, on_error),
            cpal::SampleFormat::U16 => build::<u16>(&device, &config, channels, cons, u, on_error),
            cpal::SampleFormat::U8 => build::<u8>(&device, &config, channels, cons, u, on_error),
            cpal::SampleFormat::I8 => build::<i8>(&device, &config, channels, cons, u, on_error),
            cpal::SampleFormat::F64 => build::<f64>(&device, &config, channels, cons, u, on_error),
            other => {
                return Err(MediaError::Audio(format!(
                    "{name}: unsupported sample format {other:?}"
                )));
            }
        }
        .map_err(|e| MediaError::Audio(format!("{name}: {e}")))?;
        stream
            .play()
            .map_err(|e| MediaError::Audio(format!("{name}: {e}")))?;
        tracing::info!(device = %name, rate, channels, "audio output started");
        Ok((
            Self {
                device_name: name,
                device_rate: rate,
                underruns,
                _stream: stream,
            },
            sink,
        ))
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    mut cons: HeapCons<f32>,
    (underruns, callback_frames): (Arc<AtomicU64>, Arc<AtomicU32>),
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    device.build_output_stream::<T, _, _>(
        *config,
        move |data: &mut [T], _info| {
            // Real-time context: no allocation, no locks.
            let mut missing = 0u64;
            for frame in data.chunks_exact_mut(channels) {
                let v = match cons.try_pop() {
                    Some(v) => v,
                    None => {
                        missing += 1;
                        0.0
                    }
                };
                let s = T::from_sample_(v);
                for out in frame.iter_mut() {
                    *out = s;
                }
            }
            if missing > 0 {
                underruns.fetch_add(missing, Ordering::Relaxed);
            }
            callback_frames.fetch_max((data.len() / channels) as u32, Ordering::Relaxed);
        },
        on_error,
        None,
    )
}
