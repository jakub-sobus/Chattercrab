//! Microphone noise suppression (passthrough stub; replaced by the RNNoise implementation).
/// Samples per denoiser frame: 20 ms at 48 kHz mono.
pub const DENOISE_FRAME_LEN: usize = 960;
/// Noise suppressor for 20 ms, 48 kHz mono frames. Allocates only in `new`.
pub struct Denoiser {
    _private: (),
}
impl Denoiser {
    /// Creates a denoiser. Allocates all state up front.
    pub fn new() -> Self {
        Self { _private: () }
    }
    /// Denoises one frame in place. Samples are f32 in -1.0..=1.0. Never allocates.
    pub fn process(&mut self, frame: &mut [f32; DENOISE_FRAME_LEN]) {
        let _ = frame;
    }
    /// Clears internal state (call when suppression is switched on).
    pub fn reset(&mut self) {}
}
impl Default for Denoiser {
    fn default() -> Self {
        Self::new()
    }
}
