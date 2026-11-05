//! Mono resampler wrapper around rubato with preallocated buffers.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, PolynomialDegree, Resampler};

use crate::MediaError;

/// Fixed-input mono resampler. Every call consumes exactly [`MonoResampler::chunk`]
/// input samples and returns a variable number of output samples. No allocation
/// happens after construction.
pub struct MonoResampler {
    inner: Async<f32>,
    chunk: usize,
    out: Vec<f32>,
}

impl MonoResampler {
    /// Creates a resampler from `from_hz` to `to_hz` consuming `chunk` samples per call.
    ///
    /// Uses cubic polynomial interpolation: cheap enough for voice, without the sinc
    /// filter cost (see rubato's guidance for voice applications).
    pub fn new(from_hz: u32, to_hz: u32, chunk: usize) -> Result<Self, MediaError> {
        let ratio = f64::from(to_hz) / f64::from(from_hz);
        let inner = Async::<f32>::new_poly(
            ratio,
            1.1,
            PolynomialDegree::Cubic,
            chunk,
            1,
            FixedAsync::Input,
        )
        .map_err(|e| MediaError::Audio(format!("resampler: {e}")))?;
        let out = vec![0.0; inner.output_frames_max()];
        Ok(Self { inner, chunk, out })
    }

    /// Number of input samples consumed per [`MonoResampler::process`] call.
    pub fn chunk(&self) -> usize {
        self.chunk
    }

    /// Maximum number of output samples one call can produce.
    pub fn max_output(&self) -> usize {
        self.out.len()
    }

    /// Resamples exactly `chunk` samples. Returns the produced samples.
    pub fn process(&mut self, input: &[f32]) -> Result<&[f32], MediaError> {
        let frames_in = self.inner.input_frames_next();
        if input.len() < frames_in {
            return Err(MediaError::Audio("resampler input too short".into()));
        }
        let capacity = self.out.len();
        let adapter_in = InterleavedSlice::new(&input[..frames_in], 1, frames_in)
            .map_err(|e| MediaError::Audio(format!("resampler input: {e}")))?;
        let mut adapter_out = InterleavedSlice::new_mut(&mut self.out[..], 1, capacity)
            .map_err(|e| MediaError::Audio(format!("resampler output: {e}")))?;
        let (_read, written) = self
            .inner
            .process_into_buffer(&adapter_in, &mut adapter_out, None)
            .map_err(|e| MediaError::Audio(format!("resample: {e}")))?;
        Ok(&self.out[..written])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_rate_on_average() {
        let mut r = MonoResampler::new(44_100, 48_000, 441).unwrap();
        let input = vec![0.25f32; 441];
        let mut produced = 0usize;
        for _ in 0..100 {
            produced += r.process(&input).unwrap().len();
        }
        // 100 x 10 ms = 1 s at 48 kHz, minus the resampler's startup delay.
        assert!((47_000..=48_100).contains(&produced), "produced {produced}");
    }
}
