//! Microphone noise suppression based on RNNoise (`nnnoiseless`, a pure-Rust port).
//!
//! RNNoise is a small recurrent neural network that estimates per-band gains for
//! speech versus background noise. It works on 10 ms blocks (480 samples at 48 kHz),
//! so one 20 ms Opus frame is exactly two blocks.
//!
//! # Latency
//!
//! RNNoise reconstructs its output by overlap-add with a 20 ms window, so its output
//! lags its input by one block (10 ms). That delay is inherent to the algorithm.
//!
//! # Fade-in after creation or [`Denoiser::reset`]
//!
//! The first block RNNoise produces after a fresh start covers the period before the
//! first input sample, so it is near silence (the "fade-in artifact" mentioned in the
//! `nnnoiseless` docs). The first frame after a start or reset is therefore handled
//! specially:
//!
//! * block A (first 10 ms) is passed through dry, which continues seamlessly from
//!   whatever was sent before the denoiser was switched on;
//! * block B (second 10 ms) is an equal-power crossfade from the dry signal into the
//!   denoised output, which from then on is 10 ms behind the input.
//!
//! The switch from undelayed to delayed audio necessarily repeats 10 ms of audio; the
//! crossfade turns that into a smooth splice instead of a click.
//!
//! # Allocation
//!
//! [`Denoiser::new`] allocates everything: the active RNNoise state, a fresh spare
//! state for [`Denoiser::reset`], and the scratch buffers. It also runs one silent
//! block through a throwaway state so the thread-local FFT plan caches of the
//! constructing thread are filled. Construct the denoiser on the thread that calls
//! [`Denoiser::process`] and `process` never allocates, not even on the first call.
//!
//! `nnnoiseless` has no in-place reset (its state holds heap buffers), and cloning a
//! state allocates. `reset` therefore swaps in the preallocated spare the first time it
//! is called (an exact reset, identical to a new denoiser). Later resets keep the
//! current network state and only redo the fade-in handling above: the discarded
//! first block hides the stale overlap-add tail, and the network's recurrent state
//! re-adapts within roughly 100 ms. Recreating the denoiser (for example when the audio
//! stream is rebuilt) restores an exact reset.

use nnnoiseless::DenoiseState;

/// Samples per denoiser frame: 20 ms at 48 kHz mono.
pub const DENOISE_FRAME_LEN: usize = 960;

/// Samples per RNNoise block: 10 ms at 48 kHz.
const BLOCK_LEN: usize = DenoiseState::FRAME_SIZE;

/// RNNoise expects f32 samples in the range of an `i16`.
const I16_SCALE: f32 = 32767.0;

const _: () = assert!(DENOISE_FRAME_LEN == 2 * BLOCK_LEN);

/// Noise suppressor for 20 ms, 48 kHz mono frames. Allocates only in `new`.
pub struct Denoiser {
    /// State used by `process`.
    state: Box<DenoiseState<'static>>,
    /// Second state; untouched (fresh) until the first `reset` swaps it in.
    spare: Box<DenoiseState<'static>>,
    /// Whether `spare` is still in its freshly constructed condition.
    spare_fresh: bool,
    /// Input block scaled to the i16 range.
    scratch_in: [f32; BLOCK_LEN],
    /// RNNoise output block in the i16 range.
    scratch_out: [f32; BLOCK_LEN],
    /// Equal-power fade-in curve, `sin(pi/2 * t)`; the fade-out is the reverse.
    fade_in: [f32; BLOCK_LEN],
    /// True until the first frame after creation or reset has been processed.
    priming: bool,
}

impl Denoiser {
    /// Creates a denoiser. Allocates all state up front.
    ///
    /// Call this on the thread that will call [`Denoiser::process`] so that
    /// `process` is allocation-free from its first call (see the module docs).
    pub fn new() -> Self {
        let mut fade_in = [0.0; BLOCK_LEN];
        for (i, f) in fade_in.iter_mut().enumerate() {
            let t = (i as f32 + 0.5) / BLOCK_LEN as f32;
            *f = (std::f32::consts::FRAC_PI_2 * t).sin();
        }

        // Fill this thread's FFT plan caches with a throwaway state, so neither the
        // active nor the spare state sees any input before the first real frame.
        let mut warm_up = DenoiseState::new();
        let silence = [0.0; BLOCK_LEN];
        let mut sink = [0.0; BLOCK_LEN];
        warm_up.process_frame(&mut sink, &silence);

        Self {
            state: DenoiseState::new(),
            spare: DenoiseState::new(),
            spare_fresh: true,
            scratch_in: [0.0; BLOCK_LEN],
            scratch_out: [0.0; BLOCK_LEN],
            fade_in,
            priming: true,
        }
    }

    /// Denoises one frame in place. Samples are f32 in -1.0..=1.0. Never allocates.
    ///
    /// Output samples are clamped to -1.0..=1.0. The denoised signal lags the input by
    /// 10 ms (see the module docs).
    pub fn process(&mut self, frame: &mut [f32; DENOISE_FRAME_LEN]) {
        let priming = std::mem::replace(&mut self.priming, false);
        let (blocks, _) = frame.as_chunks_mut::<BLOCK_LEN>();
        for (index, block) in blocks.iter_mut().enumerate() {
            for (dst, &sample) in self.scratch_in.iter_mut().zip(block.iter()) {
                *dst = sample * I16_SCALE;
            }
            // The returned voice-activity probability is not needed.
            let _vad = self
                .state
                .process_frame(&mut self.scratch_out, &self.scratch_in);

            match (priming, index) {
                // First block after a start: RNNoise's output is the fade-in
                // artifact, so keep the dry input.
                (true, 0) => {
                    for sample in block.iter_mut() {
                        *sample = sample.clamp(-1.0, 1.0);
                    }
                }
                // Second block: crossfade dry (undelayed) into wet (10 ms behind).
                (true, _) => {
                    for (i, sample) in block.iter_mut().enumerate() {
                        let wet = self.scratch_out[i] / I16_SCALE;
                        let fade_in = self.fade_in[i];
                        let fade_out = self.fade_in[BLOCK_LEN - 1 - i];
                        *sample = (*sample * fade_out + wet * fade_in).clamp(-1.0, 1.0);
                    }
                }
                (false, _) => {
                    for (sample, &wet) in block.iter_mut().zip(self.scratch_out.iter()) {
                        *sample = (wet / I16_SCALE).clamp(-1.0, 1.0);
                    }
                }
            }
        }
    }

    /// Clears internal state (call when suppression is switched on).
    ///
    /// Call it whenever the sequence of frames passed to [`Denoiser::process`] has a
    /// gap, for example when suppression is switched on or the microphone is unmuted
    /// while suppression stays on. Never allocates or frees; the first call is an
    /// exact reset, later calls are approximate (see the module docs).
    pub fn reset(&mut self) {
        if self.spare_fresh {
            // The used state moves into the spare slot instead of being freed here.
            std::mem::swap(&mut self.state, &mut self.spare);
            self.spare_fresh = false;
        }
        self.priming = true;
    }
}

impl Default for Denoiser {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Denoiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Denoiser")
            .field("spare_fresh", &self.spare_fresh)
            .field("priming", &self.priming)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    /// Deterministic xorshift noise, uniform in -1.0..1.0.
    struct Noise(u32);

    impl Noise {
        fn next(&mut self) -> f32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            (x as f32 / u32::MAX as f32) * 2.0 - 1.0
        }
    }

    /// Voiced-like signal: 150 Hz harmonic series with a 1/k spectral tilt, slight
    /// vibrato, and a syllable envelope (200 ms raised-cosine "syllables" separated by
    /// 100 ms pauses). Returns the sample and whether it lies inside a syllable.
    fn voice(n: usize) -> (f32, bool) {
        let t = n as f32 / RATE;
        let period = 0.3;
        let phase = t % period;
        let on = phase < 0.2;
        let env = if on {
            0.5 - 0.5 * (2.0 * std::f32::consts::PI * phase / 0.2).cos()
        } else {
            0.0
        };
        let f0 = 150.0 + 5.0 * (2.0 * std::f32::consts::PI * 5.0 * t).sin();
        let mut s = 0.0;
        for k in 1..=20 {
            let kf = k as f32;
            s += (2.0 * std::f32::consts::PI * f0 * kf * t).sin() / kf;
        }
        (0.15 * env * s, on)
    }

    fn energy(samples: &[f32]) -> f64 {
        samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum()
    }

    fn db(ratio: f64) -> f64 {
        10.0 * ratio.log10()
    }

    /// Runs `input` through a fresh denoiser frame by frame.
    fn run(input: &[f32]) -> Vec<f32> {
        let mut d = Denoiser::new();
        let mut out = Vec::with_capacity(input.len());
        let (frames, _) = input.as_chunks::<DENOISE_FRAME_LEN>();
        for chunk in frames {
            let mut frame = *chunk;
            d.process(&mut frame);
            out.extend_from_slice(&frame);
        }
        out
    }

    /// Colored noise normalised to `rms`: white noise through a one-pole low-pass with
    /// coefficient `pole` (0.9 gives a pink-ish fan hiss, 0.99 a brown rumble).
    fn colored_noise(seed: u32, pole: f32, rms: f32, len: usize) -> Vec<f32> {
        let mut noise = Noise(seed);
        let mut lp = 0.0;
        let raw: Vec<f32> = (0..len)
            .map(|_| {
                lp = pole * lp + (1.0 - pole) * noise.next();
                lp
            })
            .collect();
        let raw_rms = (energy(&raw) / len as f64).sqrt() as f32;
        raw.iter().map(|v| v / raw_rms * rms).collect()
    }

    #[test]
    fn colored_noise_is_strongly_attenuated() {
        let len = DENOISE_FRAME_LEN * 250;
        // Skip the first second while the network adapts.
        let skip = 48_000;
        for (pole, rms) in [(0.9, 0.02), (0.9, 0.05), (0.99, 0.05)] {
            let input = colored_noise(0x1234_5678, pole, rms, len);
            let output = run(&input);
            let reduction = db(energy(&input[skip..]) / energy(&output[skip..]).max(1e-12));
            println!("colored noise (pole {pole}, rms {rms}) reduction: {reduction:.1} dB");
            assert!(
                reduction >= 10.0,
                "only {reduction:.1} dB of noise reduction"
            );
        }
    }

    /// Flat white noise is only attenuated by 1 to 2 dB when the network has never
    /// heard speech (broadband flat noise looks partly speech-like to RNNoise). Once
    /// speech has been present, the same noise is removed strongly. This test covers
    /// the realistic case: a talker, then background hiss.
    #[test]
    fn white_noise_after_speech_is_strongly_attenuated() {
        let len = DENOISE_FRAME_LEN * 250;
        let mut noise = Noise(0x1234_5678);
        let input: Vec<f32> = (0..len)
            .map(|n| 0.03 * noise.next() + if n < 96_000 { voice(n).0 } else { 0.0 })
            .collect();
        let output = run(&input);
        // Noise-only region, starting half a second after speech ended.
        let region = 120_000..len;
        let reduction = db(energy(&input[region.clone()]) / energy(&output[region]).max(1e-12));
        println!("white noise after speech reduction: {reduction:.1} dB");
        assert!(
            reduction >= 10.0,
            "only {reduction:.1} dB of noise reduction"
        );
    }

    /// Mixes the voiced-like signal with white noise and checks that denoising raises
    /// the contrast between syllables and pauses (the pauses contain only noise).
    ///
    /// What it proves: RNNoise removes much more energy from noise-only regions than
    /// from regions containing the harmonic signal. What it does not prove: real speech
    /// quality. RNNoise is trained on speech, and a synthetic harmonic tone is only a
    /// rough proxy, so the retention threshold is deliberately loose.
    #[test]
    fn keeps_voiced_signal_while_removing_noise() {
        let mut noise = Noise(0xdead_beef);
        let len = DENOISE_FRAME_LEN * 250;
        let mut input = Vec::with_capacity(len);
        let mut clean = Vec::with_capacity(len);
        let mut on_mask = Vec::with_capacity(len);
        for n in 0..len {
            let (v, on) = voice(n);
            clean.push(v);
            on_mask.push(on);
            input.push(v + 0.02 * noise.next());
        }
        let output = run(&input);

        // Compare against the output shifted back by the 10 ms algorithmic delay.
        let delay = BLOCK_LEN;
        let skip = 48_000;
        let (mut in_on, mut in_off, mut out_on, mut out_off, mut clean_on) =
            (0.0, 0.0, 0.0, 0.0, 0.0);
        for n in skip..len - delay {
            let x = f64::from(input[n]);
            let y = f64::from(output[n + delay]);
            let c = f64::from(clean[n]);
            if on_mask[n] {
                in_on += x * x;
                out_on += y * y;
                clean_on += c * c;
            } else {
                in_off += x * x;
                out_off += y * y;
            }
        }
        let contrast_in = db(in_on / in_off);
        let contrast_out = db(out_on / out_off.max(1e-12));
        let retention = db(out_on / clean_on);
        println!(
            "syllable/pause contrast: {contrast_in:.1} dB in, {contrast_out:.1} dB out; \
             voiced energy retained: {retention:.1} dB"
        );
        assert!(
            contrast_out - contrast_in >= 6.0,
            "contrast improved by only {:.1} dB",
            contrast_out - contrast_in
        );
        assert!(retention >= -12.0, "voiced signal lost {retention:.1} dB");
    }

    #[test]
    fn output_is_delayed_by_one_block() {
        // A noise burst through a fresh denoiser: find the lag that best matches.
        let mut noise = Noise(42);
        let input: Vec<f32> = (0..DENOISE_FRAME_LEN * 100)
            .map(|n| {
                let (v, _) = voice(n);
                v + 0.01 * noise.next()
            })
            .collect();
        let output = run(&input);
        let range = 48_000..input.len() - 2 * DENOISE_FRAME_LEN;
        let best = (0..DENOISE_FRAME_LEN)
            .max_by(|&a, &b| {
                let corr = |lag: usize| -> f64 {
                    range
                        .clone()
                        .map(|n| f64::from(input[n]) * f64::from(output[n + lag]))
                        .sum()
                };
                corr(a).total_cmp(&corr(b))
            })
            .unwrap();
        assert_eq!(best, BLOCK_LEN);
    }

    #[test]
    fn output_stays_in_range_and_frame_length_is_fixed() {
        let mut d = Denoiser::new();
        let mut noise = Noise(7);
        for i in 0..100 {
            let mut frame = [0.0; DENOISE_FRAME_LEN];
            for s in frame.iter_mut() {
                // Includes full-scale and slightly out-of-range input.
                *s = 1.2 * noise.next().signum() * if i % 2 == 0 { 1.0 } else { 0.5 };
            }
            d.process(&mut frame);
            assert_eq!(frame.len(), DENOISE_FRAME_LEN);
            assert!(frame.iter().all(|s| (-1.0..=1.0).contains(s)));
        }
    }

    #[test]
    fn first_block_after_start_is_dry() {
        let mut d = Denoiser::new();
        let mut noise = Noise(99);
        let mut frame = [0.0; DENOISE_FRAME_LEN];
        for s in frame.iter_mut() {
            *s = 0.3 * noise.next();
        }
        let dry = frame;
        d.process(&mut frame);
        assert_eq!(frame[..BLOCK_LEN], dry[..BLOCK_LEN]);
    }

    #[test]
    fn first_reset_is_exact() {
        let mut noise = Noise(0x0bad_cafe);
        let warm: Vec<[f32; DENOISE_FRAME_LEN]> = (0..20)
            .map(|_| std::array::from_fn(|_| 0.2 * noise.next()))
            .collect();
        let probe: Vec<[f32; DENOISE_FRAME_LEN]> = (0..20)
            .map(|n| std::array::from_fn(|i| voice(n * DENOISE_FRAME_LEN + i).0))
            .collect();

        let mut fresh = Denoiser::new();
        let mut used = Denoiser::new();
        for frame in &warm {
            let mut f = *frame;
            used.process(&mut f);
        }
        used.reset();
        for frame in &probe {
            let (mut a, mut b) = (*frame, *frame);
            fresh.process(&mut a);
            used.process(&mut b);
            assert_eq!(a, b);
        }
    }

    #[test]
    fn later_resets_restart_the_fade_in() {
        let mut d = Denoiser::new();
        let mut noise = Noise(5);
        let mut frame: [f32; DENOISE_FRAME_LEN] = std::array::from_fn(|_| 0.2 * noise.next());
        d.process(&mut frame);
        d.reset();
        d.reset();
        let mut frame: [f32; DENOISE_FRAME_LEN] = std::array::from_fn(|_| 0.2 * noise.next());
        let dry = frame;
        d.process(&mut frame);
        assert_eq!(frame[..BLOCK_LEN], dry[..BLOCK_LEN]);
    }

    #[test]
    fn processing_cost() {
        let mut d = Denoiser::new();
        let mut noise = Noise(3);
        let frames: Vec<[f32; DENOISE_FRAME_LEN]> = (0..500)
            .map(|n| {
                std::array::from_fn(|i| voice(n * DENOISE_FRAME_LEN + i).0 + 0.02 * noise.next())
            })
            .collect();
        let start = std::time::Instant::now();
        for frame in &frames {
            let mut f = *frame;
            d.process(&mut f);
            std::hint::black_box(&f);
        }
        let per_frame = start.elapsed() / frames.len() as u32;
        let core = per_frame.as_secs_f64() / 0.020 * 100.0;
        println!("denoise cost: {per_frame:?} per 20 ms frame ({core:.2}% of one core)");
        // Generous bounds so slow machines never flake: real time in debug builds,
        // a quarter of real time in release builds.
        let bound_ms = if cfg!(debug_assertions) { 20 } else { 5 };
        assert!(per_frame < std::time::Duration::from_millis(bound_ms));
    }
}
