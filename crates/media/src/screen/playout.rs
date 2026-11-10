//! Screen-share playout clock and encoded-frame buffer.
//!
//! Pictures are shown on the RTP timestamp clock, up to one second behind the
//! newest frame. Arrival jitter and a slow viewer become delay. Once arrivals
//! have been calm for a while, the delay drifts back toward what the jitter
//! needs by skipping one picture at a time. Frames are never shown in a burst.
//!
//! A publisher does not send unchanged pictures, so the RTP clock can jump between
//! two frames. Such a gap is neither a slower frame rate nor an underrun: only a
//! picture that arrives after it was due counts as one.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// H.264 RTP clock rate.
pub(super) const RTP_CLOCK_HZ: u32 = 90_000;
/// A viewer may sit this far behind the newest picture.
pub(super) const MAX_PLAYOUT_DELAY: Duration = Duration::from_secs(1);
/// Delay before the first picture. Grows toward [`MAX_PLAYOUT_DELAY`] when
/// arrivals are uneven and decays back once they are calm. The preroll is
/// still at least three frames.
pub(super) const MIN_PLAYOUT_DELAY: Duration = Duration::from_millis(50);
/// No underrun or delay increase for this long before the delay may decay.
const DECAY_CALM: Duration = Duration::from_secs(5);
/// At most one picture is skipped per interval to lower the delay.
const DECAY_INTERVAL: Duration = Duration::from_millis(250);
/// An RTP step this many times the frame estimate (in quarters) is a skipped
/// stretch of unchanged screen, not a frame interval.
const GAP_FACTOR_Q4: u64 = 10;
/// Encoded access units kept when the decoder falls behind the cap. Past this
/// the buffer is discarded and the viewer waits for the next keyframe.
const MAX_BUFFERED_FRAMES: usize = 180;
const MAX_BUFFER_SPAN: Duration = Duration::from_millis(1200);

/// What to do with the oldest buffered access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    /// Leave it queued until more media arrives or the preroll timer fires.
    Buffer,
    /// Show it at `at` (never earlier).
    Present {
        /// Wall-clock time to hand the picture to the UI.
        at: Instant,
    },
    /// Decode it so later pictures stay valid, and do not show it.
    DecodeOnly,
}

/// Maps RTP timestamps onto a steady display clock.
#[derive(Debug)]
pub(super) struct PlayoutClock {
    delay: Duration,
    origin_rtp: u32,
    origin_at: Instant,
    started: bool,
    frame_ticks: u32,
    frame_samples: u32,
    buffering_since: Option<Instant>,
    last_shown: Option<Instant>,
    last_rtp: Option<u32>,
    last_arrival: Option<Instant>,
    jitter_q4: u32,
    skipped: u64,
    /// Pictures that arrived after they were due.
    late: u64,
    /// Start of the current calm window. Cleared by an underrun or a raise.
    calm_since: Option<Instant>,
    last_decay: Option<Instant>,
}

impl PlayoutClock {
    /// Clock that has not shown a frame yet.
    pub(super) fn new() -> Self {
        Self {
            delay: MIN_PLAYOUT_DELAY,
            origin_rtp: 0,
            origin_at: Instant::now(),
            started: false,
            frame_ticks: RTP_CLOCK_HZ / 30,
            frame_samples: 0,
            buffering_since: None,
            last_shown: None,
            last_rtp: None,
            last_arrival: None,
            jitter_q4: 0,
            skipped: 0,
            late: 0,
            calm_since: None,
            last_decay: None,
        }
    }

    /// Forgets the current timeline after a gap or a new keyframe.
    pub(super) fn reset(&mut self) {
        let delay = self.delay;
        let frame_ticks = self.frame_ticks;
        let jitter_q4 = self.jitter_q4;
        let late = self.late;
        *self = Self::new();
        self.delay = delay;
        self.frame_ticks = frame_ticks;
        self.jitter_q4 = jitter_q4;
        self.late = late;
    }

    /// Estimated time between pictures.
    pub(super) fn frame_duration(&self) -> Duration {
        ticks_to_duration(self.frame_ticks.max(1)).max(Duration::from_millis(1))
    }

    /// Current preroll / cushion.
    pub(super) fn delay(&self) -> Duration {
        self.delay
    }

    /// Pictures skipped because the buffer was past [`MAX_PLAYOUT_DELAY`].
    pub(super) fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Pictures that arrived after they were due (each one was an underrun).
    pub(super) fn late(&self) -> u64 {
        self.late
    }

    /// Records one access unit as it arrives, before it is shown.
    ///
    /// A picture that arrives more than a frame after it was due means the
    /// buffer ran dry: that is an [`Self::underrun`]. A gap in the RTP clock
    /// moves the due time with it, so an idle stretch is not one.
    pub(super) fn observe(&mut self, rtp_ts: u32, arrival: Instant) {
        if let Some(prev) = self.last_rtp {
            let delta = rtp_ts.wrapping_sub(prev);
            if delta > 0 && delta <= RTP_CLOCK_HZ * 2 {
                if delta < RTP_CLOCK_HZ && !self.is_gap(delta) {
                    self.note_frame_interval(delta);
                }
                self.note_jitter(prev, rtp_ts, arrival);
            }
        }
        self.last_rtp = Some(rtp_ts);
        self.last_arrival = Some(arrival);
        if self.started
            && let Some(due) = self.due(rtp_ts)
            && arrival > due + self.frame_duration()
        {
            self.late = self.late.saturating_add(1);
            self.underrun();
        }
        self.raise_delay(self.target_from_jitter());
    }

    /// Chooses what to do with the oldest queued picture at `now`.
    ///
    /// `oldest` and `newest` are RTP timestamps of the frames still buffered.
    pub(super) fn step(&mut self, oldest: u32, newest: u32, now: Instant) -> Step {
        let delta = newest.wrapping_sub(oldest);
        if delta > RTP_CLOCK_HZ * 2 {
            self.restart(oldest, now);
            return Step::Present { at: now };
        }
        let span = ticks_to_duration(delta);
        if !self.started {
            let since = *self.buffering_since.get_or_insert(now);
            let ready =
                span >= self.preroll() || now.saturating_duration_since(since) >= self.preroll();
            if !ready {
                return Step::Buffer;
            }
            self.restart(oldest, now);
            return Step::Present { at: now };
        }
        if delta > duration_to_ticks(MAX_PLAYOUT_DELAY) {
            self.skipped = self.skipped.saturating_add(1);
            return Step::DecodeOnly;
        }
        if self.decay(span, now) {
            return Step::DecodeOnly;
        }
        let rtp_due = self.due(oldest).unwrap_or(now);
        let earliest = self
            .last_shown
            .map(|shown| shown + self.frame_duration())
            .unwrap_or(now);
        let at = if rtp_due > earliest {
            rtp_due
        } else {
            earliest
        };
        if at > now {
            Step::Present { at }
        } else {
            Step::Present { at: now }
        }
    }

    /// The picture was handed to the UI at `at`. The next one is not shown
    /// sooner than one frame after this.
    pub(super) fn did_show(&mut self, at: Instant) {
        self.last_shown = Some(at);
    }

    /// The buffer ran dry. The next pictures wait for [`Self::preroll`] again.
    pub(super) fn underrun(&mut self) {
        let extra = self.frame_duration().max(Duration::from_millis(20));
        self.delay = (self.delay + extra).min(MAX_PLAYOUT_DELAY);
        self.calm_since = None;
        self.started = false;
        self.buffering_since = None;
        self.last_shown = None;
    }

    /// When the picture stamped `rtp` should be shown on the current timeline.
    fn due(&self, rtp: u32) -> Option<Instant> {
        self.origin_at
            .checked_add(ticks_to_duration(rtp.wrapping_sub(self.origin_rtp)))
    }

    /// Whether `delta` skips over unchanged pictures rather than being one frame
    /// interval. The first interval is always taken.
    fn is_gap(&self, delta: u32) -> bool {
        self.frame_samples > 0 && u64::from(delta) * 4 > u64::from(self.frame_ticks) * GAP_FACTOR_Q4
    }

    fn note_frame_interval(&mut self, delta: u32) {
        self.frame_samples = self.frame_samples.saturating_add(1);
        self.frame_ticks = if self.frame_samples < 8 {
            let samples = u64::from(self.frame_samples);
            ((u64::from(self.frame_ticks) * (samples - 1) + u64::from(delta)) / samples) as u32
        } else {
            (self.frame_ticks * 7 + delta) / 8
        };
    }

    fn preroll(&self) -> Duration {
        let three_frames = self.frame_duration() * 3;
        self.delay
            .max(three_frames)
            .clamp(MIN_PLAYOUT_DELAY, MAX_PLAYOUT_DELAY)
    }

    fn restart(&mut self, rtp: u32, now: Instant) {
        self.started = true;
        self.origin_rtp = rtp;
        self.origin_at = now;
        self.buffering_since = None;
        self.last_shown = None;
    }

    fn target_from_jitter(&self) -> Duration {
        let jitter = Duration::from_secs_f64(f64::from(self.jitter_ms()) / 1000.0);
        let target = jitter * 3 + self.frame_duration() * 2;
        target.clamp(MIN_PLAYOUT_DELAY, MAX_PLAYOUT_DELAY)
    }

    fn jitter_ms(&self) -> f32 {
        (self.jitter_q4 as f32 / 16.0) * 1000.0 / (RTP_CLOCK_HZ as f32)
    }

    fn note_jitter(&mut self, prev_rtp: u32, rtp: u32, arrival: Instant) {
        let Some(prev_arrival) = self.last_arrival else {
            return;
        };
        let rtp_delta = i64::from(rtp.wrapping_sub(prev_rtp));
        let arrival_ticks = arrival
            .saturating_duration_since(prev_arrival)
            .as_secs_f64()
            * f64::from(RTP_CLOCK_HZ);
        let d = i64::from(
            u32::try_from(
                (arrival_ticks as i64 - rtp_delta)
                    .unsigned_abs()
                    .min(u64::from(RTP_CLOCK_HZ)),
            )
            .unwrap_or(RTP_CLOCK_HZ),
        );
        let j = i64::from(self.jitter_q4);
        let next = (j + (d * 16 - j) / 16).max(0);
        self.jitter_q4 = u32::try_from(next).unwrap_or(u32::MAX);
    }

    fn raise_delay(&mut self, target: Duration) {
        let target = target.clamp(MIN_PLAYOUT_DELAY, MAX_PLAYOUT_DELAY);
        let slack = self.frame_duration() / 2;
        if target <= self.delay.saturating_add(slack) {
            return;
        }
        let add = target - self.delay;
        self.delay = target;
        self.calm_since = None;
        if self.started {
            self.origin_at += add;
        }
    }

    /// Lowers the delay by one frame when it sits well above what the jitter
    /// needs, the last underrun or raise was [`DECAY_CALM`] ago, and the
    /// buffer holds more than the target. Returns true when the oldest picture
    /// should be skipped for that.
    ///
    /// Later pictures become due one frame earlier, so the picture after the
    /// skipped one lands one frame after the last shown, not in a burst.
    fn decay(&mut self, span: Duration, now: Instant) -> bool {
        let calm_since = *self.calm_since.get_or_insert(now);
        if now.saturating_duration_since(calm_since) < DECAY_CALM {
            return false;
        }
        if self
            .last_decay
            .is_some_and(|last| now.saturating_duration_since(last) < DECAY_INTERVAL)
        {
            return false;
        }
        let frame = self.frame_duration();
        let floor = self.target_from_jitter() + frame;
        if self.delay < floor || span < floor {
            return false;
        }
        let Some(origin_at) = self.origin_at.checked_sub(frame) else {
            return false;
        };
        self.origin_at = origin_at;
        self.delay -= frame;
        self.last_decay = Some(now);
        true
    }
}

/// Shared clock the RTP reader and the decoder thread both touch.
#[derive(Clone)]
pub(super) struct SharedClock {
    inner: Arc<Mutex<PlayoutClock>>,
}

impl SharedClock {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(PlayoutClock::new())),
        }
    }

    pub(super) fn observe(&self, rtp_ts: u32, arrival: Instant) {
        self.lock().observe(rtp_ts, arrival);
    }

    pub(super) fn with_mut<R>(&self, f: impl FnOnce(&mut PlayoutClock) -> R) -> R {
        f(&mut self.lock())
    }

    fn lock(&self) -> MutexGuard<'_, PlayoutClock> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One encoded picture waiting to be decoded.
pub(super) struct TimedAu {
    pub data: Vec<u8>,
    pub rtp_ts: u32,
    pub keyframe: bool,
}

/// Why [`AuBuffer::push`] did not keep the access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PushError {
    /// The buffer was past the cap and has been cleared. Wait for an IDR.
    Overflow,
    /// The receiver has stopped.
    Closed,
}

struct AuState {
    frames: VecDeque<TimedAu>,
    closed: bool,
    overflow: bool,
    /// Bumps on every push, pop, clear, or close so a waiter can tell that the
    /// queue changed even when the length happens to match.
    generation: u64,
}

/// Bounded queue of encoded access units between the RTP reader and the decoder.
pub(super) struct AuBuffer {
    state: Mutex<AuState>,
    cv: Condvar,
}

/// A look at the oldest and newest queued pictures.
#[derive(Debug, Clone, Copy)]
pub(super) struct Peek {
    pub oldest: u32,
    pub newest: u32,
    pub generation: u64,
    pub keyframe: bool,
}

/// Result of waiting for the encoded queue.
#[derive(Debug)]
pub(super) enum Wait {
    /// [`AuBuffer::close`] was called.
    Closed,
    /// The queue was discarded. The decoder must wait for a keyframe.
    Overflow,
    /// Nothing new arrived before the timeout. `queued` is set when pictures
    /// are still sitting there (the preroll timer ran out).
    Timeout {
        /// Pictures still queued, if any.
        queued: Option<Peek>,
    },
    /// A picture is waiting that the caller has not looked at yet.
    Ready(Peek),
}

impl AuBuffer {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(AuState {
                frames: VecDeque::new(),
                closed: false,
                overflow: false,
                generation: 0,
            }),
            cv: Condvar::new(),
        })
    }

    pub(super) fn push(&self, au: TimedAu) -> Result<(), PushError> {
        let mut guard = self.lock();
        if guard.closed {
            return Err(PushError::Closed);
        }
        guard.frames.push_back(au);
        guard.generation = guard.generation.wrapping_add(1);
        let overflow =
            guard.frames.len() > MAX_BUFFERED_FRAMES || span_of(&guard.frames) > MAX_BUFFER_SPAN;
        if overflow {
            guard.frames.clear();
            guard.overflow = true;
            guard.generation = guard.generation.wrapping_add(1);
        }
        self.cv.notify_all();
        if overflow {
            Err(PushError::Overflow)
        } else {
            Ok(())
        }
    }

    pub(super) fn close(&self) {
        let mut guard = self.lock();
        guard.closed = true;
        guard.generation = guard.generation.wrapping_add(1);
        self.cv.notify_all();
    }

    pub(super) fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Takes and clears the overflow flag.
    pub(super) fn take_overflow(&self) -> bool {
        let mut guard = self.lock();
        let overflow = guard.overflow;
        guard.overflow = false;
        overflow
    }

    pub(super) fn pop(&self) -> Option<TimedAu> {
        let mut guard = self.lock();
        let au = guard.frames.pop_front()?;
        guard.generation = guard.generation.wrapping_add(1);
        self.cv.notify_all();
        Some(au)
    }

    /// Timestamp span from the oldest queued picture to the newest.
    pub(super) fn span(&self) -> Duration {
        span_of(&self.lock().frames)
    }

    /// Waits until `generation` changes, the buffer closes, or `timeout` elapses.
    pub(super) fn wait_generation(&self, generation: u64, timeout: Duration) -> Wait {
        let mut guard = self.lock();
        let deadline = Instant::now() + timeout;
        loop {
            if guard.closed {
                return Wait::Closed;
            }
            if guard.overflow {
                return Wait::Overflow;
            }
            if guard.generation != generation
                && !guard.frames.is_empty()
                && let Some(peek) = peek_of(&guard)
            {
                return Wait::Ready(peek);
            }
            let now = Instant::now();
            if now >= deadline {
                return Wait::Timeout {
                    queued: peek_of(&guard),
                };
            }
            let wait = deadline.saturating_duration_since(now);
            let waited = self.cv.wait_timeout(guard, wait);
            let (next, status) = match waited {
                Ok(pair) => pair,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard = next;
            if status.timed_out() && !guard.closed && !guard.overflow {
                let changed = guard.generation != generation && !guard.frames.is_empty();
                if !changed {
                    return Wait::Timeout {
                        queued: peek_of(&guard),
                    };
                }
            }
        }
    }

    /// Sleeps until `deadline` or until the buffer is closed.
    /// Returns false when the receiver has stopped.
    pub(super) fn sleep_until(&self, deadline: Instant) -> bool {
        let mut guard = self.lock();
        loop {
            if guard.closed {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            let waited = self
                .cv
                .wait_timeout(guard, deadline.saturating_duration_since(now));
            guard = match waited {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    fn lock(&self) -> MutexGuard<'_, AuState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn peek_of(state: &AuState) -> Option<Peek> {
    let first = state.frames.front()?;
    let last = state.frames.back()?;
    Some(Peek {
        oldest: first.rtp_ts,
        newest: last.rtp_ts,
        generation: state.generation,
        keyframe: first.keyframe,
    })
}

fn span_of(frames: &VecDeque<TimedAu>) -> Duration {
    let (Some(first), Some(last)) = (frames.front(), frames.back()) else {
        return Duration::ZERO;
    };
    ticks_to_duration(last.rtp_ts.wrapping_sub(first.rtp_ts))
}

pub(super) fn ticks_to_duration(ticks: u32) -> Duration {
    Duration::from_nanos(u64::from(ticks) * 1_000_000_000 / u64::from(RTP_CLOCK_HZ))
}

fn duration_to_ticks(duration: Duration) -> u32 {
    let ticks = duration.as_secs_f64() * f64::from(RTP_CLOCK_HZ);
    if ticks <= 1.0 {
        1
    } else if ticks >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        ticks.round() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn preroll_holds_then_shows_the_first_frame_immediately() {
        let mut clock = PlayoutClock::new();
        clock.frame_ticks = 1_500;
        let t0 = Instant::now();
        assert_eq!(clock.step(0, 0, t0), Step::Buffer);
        let newest = 6 * 1_500;
        match clock.step(0, newest, t0) {
            Step::Present { at } => assert_eq!(at, t0),
            other => panic!("expected the first frame, got {other:?}"),
        }
        assert!(clock.started);
    }

    #[test]
    fn preroll_starts_when_the_wall_timer_expires() {
        let mut clock = PlayoutClock::new();
        let t0 = Instant::now();
        assert_eq!(clock.step(0, 0, t0), Step::Buffer);
        // Three frames at the default 30 fps, above the 50 ms minimum.
        assert_eq!(clock.step(0, 0, at(t0, 99)), Step::Buffer);
        match clock.step(0, 0, at(t0, 100)) {
            Step::Present { at: shown } => assert_eq!(shown, at(t0, 100)),
            other => panic!("expected start after the timer, got {other:?}"),
        }
    }

    #[test]
    fn preroll_at_60_fps_is_the_minimum_delay() {
        let mut clock = PlayoutClock::new();
        clock.frame_ticks = 1_500;
        let t0 = Instant::now();
        assert_eq!(clock.step(0, 0, t0), Step::Buffer);
        assert_eq!(clock.step(0, 0, at(t0, 49)), Step::Buffer);
        assert!(matches!(clock.step(0, 0, at(t0, 50)), Step::Present { .. }));
        assert_eq!(clock.delay(), MIN_PLAYOUT_DELAY);
    }

    /// Steady 60 fps arrivals and a decoder that pops each picture when it is due.
    /// Optionally the publisher skips unchanged pictures (`gap`) or the network
    /// holds frames back (`stall`).
    struct Sim {
        clock: PlayoutClock,
        t0: Instant,
        now: Instant,
        next: u32,
        queue: VecDeque<u32>,
        shown: Vec<Instant>,
        decode_only: u32,
        /// From frame `.0` on, timestamps (and sending) are `.1` ticks later.
        gap: Option<(u32, u32)>,
        /// From frame `.0` on, nothing arrives before `.1` after the start.
        stall: Option<(u32, Duration)>,
    }

    impl Sim {
        const TICKS: u32 = 1_500;

        fn new(delay: Duration) -> Self {
            let mut clock = PlayoutClock::new();
            clock.frame_ticks = Self::TICKS;
            clock.delay = delay;
            let t0 = Instant::now();
            Self {
                clock,
                t0,
                now: t0,
                next: 0,
                queue: VecDeque::new(),
                shown: Vec::new(),
                decode_only: 0,
                gap: None,
                stall: None,
            }
        }

        fn rtp(&self, n: u32) -> u32 {
            let skipped = match self.gap {
                Some((from, ticks)) if n >= from => ticks,
                _ => 0,
            };
            n * Self::TICKS + skipped
        }

        fn arrival(&self, n: u32) -> Instant {
            let sent = self.t0 + ticks_to_duration(self.rtp(n));
            match self.stall {
                Some((from, until)) if n >= from => sent.max(self.t0 + until),
                _ => sent,
            }
        }

        fn run_until(&mut self, end: Duration) {
            let end = self.t0 + end;
            while self.now < end {
                while self.arrival(self.next) <= self.now {
                    let rtp = self.rtp(self.next);
                    self.clock.observe(rtp, self.arrival(self.next));
                    self.queue.push_back(rtp);
                    self.next += 1;
                }
                let (Some(&oldest), Some(&newest)) = (self.queue.front(), self.queue.back()) else {
                    self.now = self.arrival(self.next);
                    continue;
                };
                match self.clock.step(oldest, newest, self.now) {
                    Step::Buffer => self.now = self.arrival(self.next),
                    Step::DecodeOnly => {
                        self.queue.pop_front();
                        self.decode_only += 1;
                    }
                    Step::Present { at } => {
                        self.queue.pop_front();
                        self.now = self.now.max(at);
                        self.clock.did_show(at);
                        self.shown.push(at);
                    }
                }
            }
        }

        fn assert_no_burst(&self) {
            let frame = ticks_to_duration(Self::TICKS);
            for pair in self.shown.windows(2) {
                let gap = pair[1].saturating_duration_since(pair[0]);
                assert!(
                    gap + Duration::from_millis(1) >= frame,
                    "pictures {gap:?} apart"
                );
            }
        }
    }

    #[test]
    fn a_gap_in_the_timestamps_is_not_a_slower_frame_rate() {
        let mut clock = PlayoutClock::new();
        let t0 = Instant::now();
        for n in 0..20u32 {
            clock.observe(n * 1_500, t0 + ticks_to_duration(n * 1_500));
        }
        let frame = clock.frame_duration();
        assert_eq!(frame, ticks_to_duration(1_500));
        // 600 ms of unchanged screen, then the next picture.
        let rtp = 19 * 1_500 + 54_000;
        clock.observe(rtp, t0 + ticks_to_duration(rtp));
        assert_eq!(clock.frame_duration(), frame);
        clock.observe(rtp + 1_500, t0 + ticks_to_duration(rtp + 1_500));
        assert_eq!(clock.frame_duration(), frame);
    }

    #[test]
    fn the_first_interval_sets_the_frame_rate() {
        let mut clock = PlayoutClock::new();
        let t0 = Instant::now();
        // A 5 fps share is far slower than the 30 fps default.
        clock.observe(0, t0);
        clock.observe(18_000, at(t0, 200));
        clock.observe(36_000, at(t0, 400));
        assert_eq!(clock.frame_duration(), Duration::from_millis(200));
    }

    #[test]
    fn an_idle_gap_neither_underruns_nor_raises_the_delay() {
        let mut sim = Sim::new(MIN_PLAYOUT_DELAY);
        sim.gap = Some((120, 54_000));
        sim.run_until(Duration::from_millis(1_900));
        assert!(sim.clock.started);
        let frame = sim.clock.frame_duration();
        let delay = sim.clock.delay();
        let shown = sim.shown.len();

        sim.run_until(Duration::from_secs(4));
        assert_eq!(sim.clock.frame_duration(), frame);
        assert_eq!(sim.clock.delay(), delay);
        assert_eq!(sim.clock.late(), 0);
        assert!(sim.clock.started);
        assert!(
            sim.shown.len() > shown + 60,
            "playback stopped after the gap"
        );
        sim.assert_no_burst();
        // The picture after the gap keeps to the RTP clock: one frame plus
        // 600 ms after the one before it, without a new preroll.
        let longest = sim
            .shown
            .windows(2)
            .map(|pair| pair[1].saturating_duration_since(pair[0]))
            .max()
            .unwrap_or_default();
        let expected = ticks_to_duration(Sim::TICKS + 54_000);
        assert!(
            longest.abs_diff(expected) < Duration::from_millis(2),
            "longest pause {longest:?}, expected {expected:?}"
        );
    }

    #[test]
    fn a_frame_that_arrives_after_it_was_due_is_an_underrun() {
        let mut sim = Sim::new(MIN_PLAYOUT_DELAY);
        // Frames from 2 s on are held back by the network for 300 ms.
        sim.stall = Some((120, Duration::from_millis(2_300)));
        sim.run_until(Duration::from_millis(1_900));
        let delay = sim.clock.delay();
        assert_eq!(sim.clock.late(), 0);

        sim.run_until(Duration::from_secs(4));
        assert_eq!(sim.clock.late(), 1);
        assert!(sim.clock.delay() > delay, "delay {:?}", sim.clock.delay());
        assert!(sim.clock.started, "playback should resume");
    }

    #[test]
    fn delay_decays_toward_the_jitter_target_under_steady_arrivals() {
        let mut sim = Sim::new(Duration::from_millis(500));
        sim.run_until(Duration::from_secs(4));
        assert_eq!(sim.clock.delay(), Duration::from_millis(500));
        assert_eq!(sim.decode_only, 0);

        sim.run_until(Duration::from_secs(20));
        let target = sim.clock.target_from_jitter();
        let frame = sim.clock.frame_duration();
        assert!(sim.decode_only > 0);
        assert!(sim.clock.delay() >= target);
        assert!(
            sim.clock.delay() <= target + frame * 2,
            "delay {:?}, target {target:?}",
            sim.clock.delay()
        );
        assert_eq!(sim.clock.skipped(), 0);
        sim.assert_no_burst();
        // The decoder keeps up with a shorter buffer.
        let span = ticks_to_duration(sim.queue.len() as u32 * Sim::TICKS);
        assert!(span <= sim.clock.delay() + frame, "buffer still {span:?}");
    }

    #[test]
    fn no_decay_during_the_calm_window_after_an_underrun() {
        let mut sim = Sim::new(Duration::from_millis(500));
        sim.run_until(Duration::from_secs(6));
        assert!(sim.decode_only > 0, "decay should have started");
        sim.clock.underrun();
        let raised = sim.clock.delay();
        let skipped = sim.decode_only;

        sim.run_until(Duration::from_millis(10_500));
        assert_eq!(sim.decode_only, skipped, "decayed inside the calm window");
        assert_eq!(sim.clock.delay(), raised);

        sim.run_until(Duration::from_secs(13));
        assert!(
            sim.decode_only > skipped,
            "decay should resume after the window"
        );
        assert!(sim.clock.delay() < raised);
        sim.assert_no_burst();
    }

    #[test]
    fn steady_frames_stay_one_interval_apart() {
        let mut clock = PlayoutClock::new();
        let ticks = 1_500u32;
        clock.frame_ticks = ticks;
        let t0 = Instant::now();
        let newest = 8 * ticks;
        let Step::Present { at } = clock.step(0, newest, t0) else {
            panic!("preroll should start");
        };
        clock.did_show(at);
        let mut now = at;
        let mut shown = vec![now];
        for n in 1..8u32 {
            let rtp = n * ticks;
            match clock.step(rtp, newest, now) {
                Step::Present { at } => {
                    now = at;
                    clock.did_show(at);
                    shown.push(at);
                }
                other => panic!("frame {n}: {other:?}"),
            }
        }
        let expected = ticks_to_duration(ticks);
        for pair in shown.windows(2) {
            let gap = pair[1].saturating_duration_since(pair[0]);
            let drift = gap.abs_diff(expected);
            assert!(
                drift < Duration::from_millis(2),
                "gap {gap:?}, expected {expected:?}"
            );
        }
    }

    #[test]
    fn a_late_frame_does_not_make_the_next_one_immediate() {
        let mut clock = PlayoutClock::new();
        clock.frame_ticks = 1_500;
        clock.started = true;
        let t0 = Instant::now();
        clock.origin_rtp = 0;
        clock.origin_at = t0;
        clock.last_shown = Some(t0);
        let late = at(t0, 40);
        clock.did_show(late);
        match clock.step(3_000, 3_000, late) {
            Step::Present { at } => {
                let gap = at.saturating_duration_since(late);
                assert!(
                    gap >= Duration::from_millis(15),
                    "next frame followed after only {gap:?}"
                );
            }
            other => panic!("expected a timed frame, got {other:?}"),
        }
    }

    #[test]
    fn a_buffer_deeper_than_one_second_skips_until_it_fits() {
        let mut clock = PlayoutClock::new();
        clock.frame_ticks = 1_500;
        clock.started = true;
        let t0 = Instant::now();
        clock.origin_rtp = 0;
        clock.origin_at = t0;
        clock.last_shown = Some(t0);
        let newest = 119 * 1_500;
        let now = at(t0, 50);
        let mut oldest_i = 0u32;
        loop {
            let oldest = oldest_i * 1_500;
            match clock.step(oldest, newest, now) {
                Step::DecodeOnly => oldest_i += 1,
                Step::Present { .. } => break,
                Step::Buffer => panic!("should not rebuffer while frames are queued"),
            }
            assert!(oldest_i < 120, "skipped every frame");
        }
        let remain = newest - oldest_i * 1_500;
        assert!(remain <= 90_000, "still {remain} ticks behind");
        assert!(clock.skipped() > 0);
    }

    #[test]
    fn underrun_waits_for_a_deeper_preroll() {
        let mut clock = PlayoutClock::new();
        clock.frame_ticks = 1_500;
        let t0 = Instant::now();
        let _ = clock.step(0, 6 * 1_500, t0);
        assert!(clock.started);
        let before = clock.delay();
        clock.underrun();
        assert!(!clock.started);
        assert!(clock.delay() > before);
        assert_eq!(clock.step(0, 0, t0), Step::Buffer);
    }

    #[test]
    fn overflow_clears_the_encoded_buffer() {
        let buffer = AuBuffer::new();
        let mut overflowed = false;
        for n in 0..200u32 {
            let result = buffer.push(TimedAu {
                data: vec![n as u8],
                rtp_ts: n * 1_500,
                keyframe: n == 0,
            });
            if result == Err(PushError::Overflow) {
                overflowed = true;
                break;
            }
        }
        assert!(overflowed);
        assert!(buffer.pop().is_none());
        assert!(buffer.take_overflow());
        assert!(!buffer.take_overflow());
    }

    #[test]
    fn close_wakes_a_waiter() {
        let buffer = AuBuffer::new();
        let waiting = Arc::clone(&buffer);
        let thread = std::thread::spawn(move || waiting.wait_generation(0, Duration::from_secs(5)));
        buffer.close();
        match thread.join() {
            Ok(Wait::Closed) => {}
            Ok(other) => panic!("waiter returned {other:?}"),
            Err(_) => panic!("waiter thread panicked"),
        }
    }
}
