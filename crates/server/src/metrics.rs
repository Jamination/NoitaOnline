//! Process CPU time and per-client bandwidth accounting.

use std::time::Duration;

/// Total user+system CPU time consumed by this process so far.
#[cfg(target_os = "linux")]
pub fn process_cpu_time() -> Duration {
    // /proc/self/stat fields 14 (utime) and 15 (stime) in clock ticks; the comm field may
    // contain spaces, so parse after the closing parenthesis.
    let Ok(s) = std::fs::read_to_string("/proc/self/stat") else { return Duration::ZERO };
    let Some(rest) = s.rfind(')').map(|i| &s[i + 2..]) else { return Duration::ZERO };
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    // Fields after ')' start at field 3 (state); utime is field 14 => index 11.
    let total = ticks(11) + ticks(12);
    let hz = clock_ticks_per_sec();
    Duration::from_nanos(total * 1_000_000_000 / hz)
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_sec() -> u64 {
    extern "C" {
        fn sysconf(name: i32) -> i64;
    }
    const SC_CLK_TCK: i32 = 2;
    let v = unsafe { sysconf(SC_CLK_TCK) };
    if v > 0 {
        v as u64
    } else {
        100
    }
}

#[cfg(windows)]
pub fn process_cpu_time() -> Duration {
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        lo: u32,
        hi: u32,
    }
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessTimes(h: isize, c: *mut FileTime, e: *mut FileTime, k: *mut FileTime, u: *mut FileTime) -> i32;
    }
    let (mut c, mut e, mut k, mut u) = (FileTime::default(), FileTime::default(), FileTime::default(), FileTime::default());
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    if ok == 0 {
        return Duration::ZERO;
    }
    let t = |f: &FileTime| ((f.hi as u64) << 32) | f.lo as u64;
    Duration::from_nanos((t(&k) + t(&u)) * 100)
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn process_cpu_time() -> Duration {
    Duration::ZERO
}

/// Rolling per-second rate tracker over cumulative counters.
#[derive(Default, Clone)]
pub struct Rate {
    last_value: u64,
    pub max_per_sec: f64,
    pub total: u64,
    pub seconds: f64,
}

impl Rate {
    /// Feed the cumulative counter after `dt` seconds; returns the rate for this window.
    pub fn sample(&mut self, cumulative: u64, dt: f64) -> f64 {
        let delta = cumulative.saturating_sub(self.last_value);
        self.last_value = cumulative;
        self.total += delta;
        self.seconds += dt;
        let r = if dt > 0.0 { delta as f64 / dt } else { 0.0 };
        if r > self.max_per_sec {
            self.max_per_sec = r;
        }
        r
    }

    pub fn average(&self) -> f64 {
        if self.seconds > 0.0 {
            self.total as f64 / self.seconds
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_time_advances_with_work() {
        let a = process_cpu_time();
        let mut x = 0u64;
        let t = std::time::Instant::now();
        while t.elapsed() < Duration::from_millis(300) {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
        let b = process_cpu_time();
        assert!(b > a + Duration::from_millis(100), "{a:?} -> {b:?}");
    }

    #[test]
    fn rate_tracks_average_and_peak() {
        let mut r = Rate::default();
        r.sample(1000, 1.0);
        r.sample(4000, 1.0);
        assert_eq!(r.max_per_sec, 3000.0);
        assert_eq!(r.average(), 2000.0);
    }
}
