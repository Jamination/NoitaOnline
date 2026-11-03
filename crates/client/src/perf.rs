//! Frame-time recording (intervals between consecutive frame marks from the game thread)
//! and an optional frame-rate cap standing in for vsync, which a virtual X display
//! cannot provide.

use std::time::{Duration, Instant};

pub struct FrameTimer {
    last: Option<Instant>,
    /// Every recorded frame interval (µs) since the timer started.
    pub all: Vec<u32>,
    window: Vec<u32>,
    /// Time spent blocked waiting for lockstep ticks during the current window (µs).
    pub window_wait_us: u64,
    /// Frames of the current window whose tick had not arrived when the game needed it.
    window_stalls: u32,
    /// Shortest time between a tick's arrival and the game needing it, over the window's
    /// other frames (µs; `u32::MAX`: none).
    window_margin_min_us: u32,
    /// Minimum frame interval (frame-rate cap); None = uncapped.
    pub min_interval: Option<Duration>,
}

impl Default for FrameTimer {
    fn default() -> Self {
        Self {
            last: None,
            all: Vec::new(),
            window: Vec::new(),
            window_wait_us: 0,
            window_stalls: 0,
            window_margin_min_us: u32::MAX,
            min_interval: None,
        }
    }
}

/// A wait for a tick longer than this is a stall (shorter ones are thread hand-offs).
const STALL_US: u64 = 1000;

pub struct WindowStats {
    pub frames: u32,
    pub avg_us: u32,
    pub p99_us: u32,
    pub max_us: u32,
    pub wait_us: u32,
    pub stalls: u32,
    pub margin_min_us: u32,
}

impl FrameTimer {
    /// Record the game getting its frame's tick after waiting `waited_us`; `margin`: the tick
    /// was already there, it had arrived that long before.
    pub fn tick_taken(&mut self, waited_us: u64, margin: Option<Duration>) {
        self.window_wait_us += waited_us;
        if waited_us > STALL_US {
            self.window_stalls += 1;
        }
        if let Some(m) = margin {
            self.window_margin_min_us = self.window_margin_min_us.min(m.as_micros().min(u32::MAX as u128 - 1) as u32);
        }
    }

    /// Mark the start of a frame: first hold the frame until the cap's interval since the
    /// previous frame start has elapsed, then record the interval.
    pub fn mark(&mut self) -> Option<u32> {
        if let (Some(min), Some(last)) = (self.min_interval, self.last) {
            let target = last + min;
            loop {
                let now = Instant::now();
                if now >= target {
                    break;
                }
                let left = target - now;
                if left > Duration::from_micros(1500) {
                    std::thread::sleep(left - Duration::from_micros(1000));
                } else {
                    std::hint::spin_loop();
                }
            }
        }
        let now = Instant::now();
        let dt = self.last.map(|l| now.duration_since(l).as_micros().min(u32::MAX as u128) as u32);
        self.last = Some(now);
        if let Some(dt) = dt {
            self.all.push(dt);
            self.window.push(dt);
        }
        dt
    }

    pub fn window_len(&self) -> usize {
        self.window.len()
    }

    pub fn take_window(&mut self) -> Option<WindowStats> {
        if self.window.is_empty() {
            return None;
        }
        let mut w = std::mem::take(&mut self.window);
        let n = w.len();
        let sum: u64 = w.iter().map(|&v| v as u64).sum();
        w.sort_unstable();
        let p99 = w[((n * 99).div_ceil(100)).saturating_sub(1).min(n - 1)];
        let wait = std::mem::take(&mut self.window_wait_us);
        Some(WindowStats {
            frames: n as u32,
            avg_us: (sum / n as u64) as u32,
            p99_us: p99,
            max_us: *w.last().unwrap(),
            wait_us: wait.min(u32::MAX as u64) as u32,
            stalls: std::mem::take(&mut self.window_stalls),
            margin_min_us: std::mem::replace(&mut self.window_margin_min_us, u32::MAX),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_percentiles() {
        let mut t = FrameTimer::default();
        t.window = (1..=100).collect();
        let s = t.take_window().unwrap();
        assert_eq!((s.frames, s.avg_us, s.p99_us, s.max_us), (100, 50, 99, 100));
        assert!(t.take_window().is_none());
    }
}
