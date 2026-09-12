//! Audio hand-off between the emulation thread and whatever plays it.
//!
//! [`AudioOutput`] is a bounded ring of interleaved stereo `i16` samples at
//! [`AUDIO_SAMPLE_RATE`](crate::emulator::AUDIO_SAMPLE_RATE). The emulation thread pushes each
//! frame's samples into it; an audio device callback (on its own thread) pops what it needs. The
//! two clocks are never exactly the same, so the ring bounds the drift instead of correcting it:
//! when it overflows, the *oldest* samples are dropped so latency never grows; when it runs dry,
//! the reader gets fewer samples than it asked for and pads with silence.

use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;
use crate::emulator::AUDIO_SAMPLE_RATE;

/// Thread-safe sample ring; see the [module docs](self).
pub struct AudioOutput {
    /// Interleaved stereo samples.
    samples: Mutex<VecDeque<i16>>,

    /// Most stereo frames the ring may hold.
    max_frames: AtomicUsize,

    /// Current emulation speed multiplier as `f32` bits, for the consumer (it may pitch-shift
    /// playback to match when sped-up audio is not muted).
    speed: AtomicU32,

    /// Whether sped-up playback needs pitch scaling on the consumer side: `true` for cores that
    /// emit `speed`× more samples per real second when sped up (mGBA, melonDS), `false` for
    /// SameBoy, which already renders at the host rate (pitched) at any speed.
    fast_forward_scales_pitch: AtomicBool
}

impl AudioOutput {
    /// Create an empty ring that holds at most `max_latency_ms` of audio.
    pub fn new(max_latency_ms: u32) -> Self {
        let s = Self {
            samples: Mutex::new(VecDeque::new()),
            max_frames: AtomicUsize::new(0),
            speed: AtomicU32::new(1.0f32.to_bits()),
            fast_forward_scales_pitch: AtomicBool::new(true)
        };
        s.set_max_latency_ms(max_latency_ms);
        s
    }

    /// Change how much audio the ring may hold (older samples are dropped if it now holds more).
    pub fn set_max_latency_ms(&self, max_latency_ms: u32) {
        let frames = (AUDIO_SAMPLE_RATE as usize * max_latency_ms.max(1) as usize).div_ceil(1000);
        self.max_frames.store(frames, Ordering::Relaxed);
        self.trim(&mut self.lock());
    }

    /// Most stereo frames the ring holds.
    #[inline]
    pub fn max_frames(&self) -> usize {
        self.max_frames.load(Ordering::Relaxed)
    }

    /// Append interleaved stereo samples, dropping the oldest if the ring would overflow.
    pub fn push(&self, samples: &[i16]) {
        debug_assert!(samples.len() % 2 == 0, "audio samples must be interleaved stereo");
        if samples.is_empty() {
            return
        }
        let mut ring = self.lock();
        ring.extend(samples.iter().copied());
        self.trim(&mut ring);
    }

    /// Pop up to `into.len() / 2` stereo frames into `into`, returning how many frames were
    /// written. The rest of `into` is untouched; the caller pads it with silence.
    pub fn read(&self, into: &mut [i16]) -> usize {
        let wanted = into.len() / 2 * 2;
        let mut ring = self.lock();
        let available = ring.len().min(wanted);
        for (out, sample) in into.iter_mut().zip(ring.drain(..available)) {
            *out = sample;
        }
        available / 2
    }

    /// Drop everything queued.
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Stereo frames currently queued.
    pub fn queued_frames(&self) -> usize {
        self.lock().len() / 2
    }

    /// Current emulation speed multiplier.
    #[inline]
    pub fn speed(&self) -> f32 {
        f32::from_bits(self.speed.load(Ordering::Relaxed))
    }

    /// Publish the current emulation speed multiplier.
    #[inline]
    pub fn set_speed(&self, speed: f32) {
        self.speed.store(speed.to_bits(), Ordering::Relaxed);
    }

    /// See [`AudioOutput`]'s `fast_forward_scales_pitch`.
    #[inline]
    pub fn fast_forward_scales_pitch(&self) -> bool {
        self.fast_forward_scales_pitch.load(Ordering::Relaxed)
    }

    /// See [`AudioOutput`]'s `fast_forward_scales_pitch`.
    #[inline]
    pub fn set_fast_forward_scales_pitch(&self, scales: bool) {
        self.fast_forward_scales_pitch.store(scales, Ordering::Relaxed);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<i16>> {
        self.samples.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn trim(&self, ring: &mut VecDeque<i16>) {
        let max = self.max_frames() * 2;
        if ring.len() > max {
            ring.drain(..ring.len() - max);
        }
    }
}

#[cfg(test)]
mod test {
    use super::AudioOutput;
    use alloc::vec::Vec;

    fn frames(from: i16, count: usize) -> Vec<i16> {
        (0..count as i16).flat_map(|i| [from + i, -(from + i)]).collect()
    }

    #[test]
    fn overflow_keeps_the_newest() {
        let ring = AudioOutput::new(1); // 48 frames
        assert_eq!(ring.max_frames(), 48);
        ring.push(&frames(0, 40));
        ring.push(&frames(100, 40));
        assert_eq!(ring.queued_frames(), 48);

        let mut out = [0i16; 96];
        assert_eq!(ring.read(&mut out), 48);
        // the first 32 frames of the first push were dropped
        assert_eq!(&out[..4], &[32, -32, 33, -33]);
        assert_eq!(&out[92..], &[138, -138, 139, -139]);
        assert_eq!(ring.queued_frames(), 0);
    }

    #[test]
    fn underrun_returns_what_there_is() {
        let ring = AudioOutput::new(100);
        ring.push(&frames(0, 3));
        let mut out = [7i16; 10];
        assert_eq!(ring.read(&mut out), 3);
        assert_eq!(&out[..6], &[0, 0, 1, -1, 2, -2]);
        assert_eq!(&out[6..], &[7, 7, 7, 7], "unwritten samples are left for the caller to pad");
        assert_eq!(ring.read(&mut out), 0);
    }

    #[test]
    fn clear_and_shrink() {
        let ring = AudioOutput::new(100);
        ring.push(&frames(0, 500));
        ring.set_max_latency_ms(1);
        assert_eq!(ring.queued_frames(), 48);
        ring.clear();
        assert_eq!(ring.queued_frames(), 0);
    }
}
