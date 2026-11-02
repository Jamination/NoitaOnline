//! Shared session calendar. Noita reads the date and time of day for game content
//! (weather re-initialises on hour/day changes, snow depends on month and hour, holiday
//! events, `GameGetDateAndTime*`). Clients in different time zones, or crossing an hour
//! boundary on different frames, would simulate different worlds, so once a session has
//! started every calendar import returns the server's start time plus game time
//! (frame / 60 s), in UTC for both "local" and "system" time.

#![cfg_attr(not(all(windows, target_arch = "x86")), allow(dead_code))]

use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};

/// Unix seconds at session frame 0; 0 = not in a session (pass through to the real clock).
static EPOCH: AtomicI64 = AtomicI64::new(0);

const IAT_GET_LOCAL_TIME: u32 = 0x00f0_7074;
const IAT_GET_SYSTEM_TIME: u32 = 0x00f0_7090;
const IAT_TIME64: u32 = 0x00f0_7410;
const IAT_LOCALTIME64: u32 = 0x00f0_7424;
const GAME_GLOBAL_PTR: u32 = 0x0122_374c;

static ORIG_GET_LOCAL_TIME: AtomicU32 = AtomicU32::new(0);
static ORIG_GET_SYSTEM_TIME: AtomicU32 = AtomicU32::new(0);
static ORIG_TIME64: AtomicU32 = AtomicU32::new(0);
static ORIG_LOCALTIME64: AtomicU32 = AtomicU32::new(0);

pub fn set_epoch(unix_secs: i64) {
    EPOCH.store(unix_secs, Ordering::Relaxed);
}

/// Session time in unix seconds, or None outside a session.
fn now() -> Option<i64> {
    let epoch = EPOCH.load(Ordering::Relaxed);
    if epoch == 0 {
        return None;
    }
    // SAFETY: GAME_GLOBAL_PTR is a static of noita.exe; GameGlobal starts with the frame number.
    let frame = unsafe {
        let gg = *(GAME_GLOBAL_PTR as *const u32);
        if gg == 0 { 0 } else { *(gg as *const u32) }
    };
    Some(epoch + i64::from(frame / 60))
}

/// Calendar fields of unix time `t` (UTC): (year, month 1-12, day, hour, min, sec, weekday 0=Sun, yday 0-365).
pub fn civil(t: i64) -> (i64, u32, u32, u32, u32, u32, u32, u32) {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400) as u32;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    let weekday = (days + 4).rem_euclid(7) as u32;
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    const CUM: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let yday = CUM[month as usize - 1] + day - 1 + u32::from(leap && month > 2);
    (year, month, day, secs / 3600, secs / 60 % 60, secs % 60, weekday, yday)
}

#[repr(C)]
struct SystemTime {
    year: u16,
    month: u16,
    weekday: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    millis: u16,
}

/// MSVC `struct tm`.
#[repr(C)]
struct Tm {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    isdst: i32,
}

unsafe fn fill_systemtime(out: *mut SystemTime, t: i64) {
    let (y, mo, d, h, mi, s, wd, _) = civil(t);
    *out = SystemTime { year: y as u16, month: mo as u16, weekday: wd as u16, day: d as u16, hour: h as u16, minute: mi as u16, second: s as u16, millis: 0 };
}

unsafe extern "system" fn get_local_time(out: *mut SystemTime) {
    match now() {
        Some(t) => fill_systemtime(out, t),
        None => {
            let f: unsafe extern "system" fn(*mut SystemTime) = std::mem::transmute(ORIG_GET_LOCAL_TIME.load(Ordering::Relaxed) as usize);
            f(out)
        }
    }
}

unsafe extern "system" fn get_system_time(out: *mut SystemTime) {
    match now() {
        Some(t) => fill_systemtime(out, t),
        None => {
            let f: unsafe extern "system" fn(*mut SystemTime) = std::mem::transmute(ORIG_GET_SYSTEM_TIME.load(Ordering::Relaxed) as usize);
            f(out)
        }
    }
}

unsafe extern "C" fn time64(out: *mut i64) -> i64 {
    match now() {
        Some(t) => {
            if !out.is_null() {
                *out = t;
            }
            t
        }
        None => {
            let f: unsafe extern "C" fn(*mut i64) -> i64 = std::mem::transmute(ORIG_TIME64.load(Ordering::Relaxed) as usize);
            f(out)
        }
    }
}

unsafe extern "C" fn localtime64(t: *const i64) -> *mut Tm {
    if EPOCH.load(Ordering::Relaxed) == 0 || t.is_null() {
        let f: unsafe extern "C" fn(*const i64) -> *mut Tm = std::mem::transmute(ORIG_LOCALTIME64.load(Ordering::Relaxed) as usize);
        return f(t);
    }
    thread_local! {
        static TM: std::cell::UnsafeCell<Tm> = const { std::cell::UnsafeCell::new(Tm { sec: 0, min: 0, hour: 0, mday: 1, mon: 0, year: 70, wday: 4, yday: 0, isdst: 0 }) };
    }
    let (y, mo, d, h, mi, s, wd, yd) = civil(*t);
    TM.with(|tm| {
        let p = tm.get();
        *p = Tm { sec: s as i32, min: mi as i32, hour: h as i32, mday: d as i32, mon: mo as i32 - 1, year: (y - 1900) as i32, wday: wd as i32, yday: yd as i32, isdst: 0 };
        p
    })
}

#[cfg(all(windows, target_arch = "x86"))]
/// Redirect the calendar imports (idempotent through the patch table).
///
/// # Safety
/// Inside noita.exe build 0x6794ee3c; `write` must make the IAT slot writable.
pub unsafe fn install(write: unsafe fn(u32, &[u8]) -> Result<(), crate::patches::PatchError>) -> Result<(), crate::patches::PatchError> {
    let hooks: [(u32, &AtomicU32, u32); 4] = [
        (IAT_GET_LOCAL_TIME, &ORIG_GET_LOCAL_TIME, get_local_time as *const () as u32),
        (IAT_GET_SYSTEM_TIME, &ORIG_GET_SYSTEM_TIME, get_system_time as *const () as u32),
        (IAT_TIME64, &ORIG_TIME64, time64 as *const () as u32),
        (IAT_LOCALTIME64, &ORIG_LOCALTIME64, localtime64 as *const () as u32),
    ];
    for (slot, orig, hook) in hooks {
        orig.store(*(slot as *const u32), Ordering::Relaxed);
        write(slot, &hook.to_le_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::civil;

    #[test]
    fn civil_matches_known_dates() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0, 4, 0));
        // 2024-02-29 13:45:07 UTC, Thursday, yday 59.
        assert_eq!(civil(1_709_214_307), (2024, 2, 29, 13, 45, 7, 4, 59));
        // 2025-12-31 23:59:59 UTC, Wednesday, yday 364.
        assert_eq!(civil(1_767_225_599), (2025, 12, 31, 23, 59, 59, 3, 364));
    }
}
