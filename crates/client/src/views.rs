//! Every player's view, for the engine systems that vanilla ties to its one camera.
//!
//! Vanilla Noita has one camera, centred on the local player. Many simulation systems test
//! against it: particle effects are only created on screen, CameraBound entities far from it
//! are frozen and killed, animals only start hunting near it, rain falls around it. Inside
//! the lockstep window the engine camera is the shared simulation camera (the first live
//! player), so those systems would run around player 1 only. Their patches test against
//! every player's view instead ("as if each player's own camera were the camera").
//!
//! A player's view is modelled deterministically: its centre is the player's focus centre at
//! window entry (`no_focus_freeze`, slot order); its rect is vanilla's render rect (431×242
//! world px around the camera centre) grown by [`VIEW_MARGIN`] on every side, which covers
//! the vanilla camera's offset from its player (mouse look-ahead, capped at 50 px by
//! `camera_max_distance_from_character`, plus recoil and smoothing lag).
//!
//! The views are written by the game thread inside the window before the world update and
//! read during it, also by pool workers (grid tiles); the pool's job hand-off orders them.

use std::sync::atomic::{AtomicU32, Ordering};

pub const MAX_VIEWS: usize = 16;
/// Half of vanilla's render rect (431×242: VIRTUAL_RESOLUTION 427×242 plus the border the
/// grid render adds), rounded up.
pub const VIEW_HALF_W: i32 = 216;
pub const VIEW_HALF_H: i32 = 121;
/// Extra room on every side for the vanilla camera's offset from its player.
pub const VIEW_MARGIN: i32 = 64;

static COUNT: AtomicU32 = AtomicU32::new(0);
static CENTRES: [[AtomicU32; 2]; MAX_VIEWS] = [const { [AtomicU32::new(0), AtomicU32::new(0)] }; MAX_VIEWS];

/// Publish this window's focus centres (slot order). Game thread, inside the window.
pub fn set_foci(foci: &[(f32, f32)]) {
    let foci = &foci[..foci.len().min(MAX_VIEWS)];
    for (slot, &(x, y)) in CENTRES.iter().zip(foci) {
        slot[0].store(x.to_bits(), Ordering::Relaxed);
        slot[1].store(y.to_bits(), Ordering::Relaxed);
    }
    COUNT.store(foci.len() as u32, Ordering::Release);
}

/// Number of views (0 before the first window published the players, or when no player
/// is alive).
pub fn count() -> usize {
    COUNT.load(Ordering::Acquire) as usize
}

/// Centre of view `i` (world px).
pub fn centre(i: usize) -> (f32, f32) {
    (f32::from_bits(CENTRES[i][0].load(Ordering::Relaxed)), f32::from_bits(CENTRES[i][1].load(Ordering::Relaxed)))
}

/// Every view centre, slot order.
pub fn centres() -> Vec<(f32, f32)> {
    (0..count()).map(centre).collect()
}

/// Rect of view `i`: `[x0, y0, x1, y1)` in world px.
pub fn rect(i: usize) -> [i32; 4] {
    let (x, y) = centre(i);
    let (x, y) = (x.floor() as i32, y.floor() as i32);
    let (hw, hh) = (VIEW_HALF_W + VIEW_MARGIN, VIEW_HALF_H + VIEW_MARGIN);
    [x - hw, y - hh, x + hw, y + hh]
}

/// Every view rect, slot order.
pub fn rects() -> Vec<[i32; 4]> {
    (0..count()).map(rect).collect()
}

/// Whether the world pixel (x, y) lies in some player's view.
pub fn in_any(x: i32, y: i32) -> bool {
    (0..count()).any(|i| {
        let r = rect(i);
        x >= r[0] && x < r[2] && y >= r[1] && y < r[3]
    })
}

/// The view whose centre is nearest to (x, y), as (index, squared distance); the lowest
/// slot wins ties. None without views.
pub fn nearest(x: f32, y: f32) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for i in 0..count() {
        let (cx, cy) = centre(i);
        let d = (x - cx) * (x - cx) + (y - cy) * (y - cy);
        if best.is_none_or(|(_, b)| d < b) {
            best = Some((i, d));
        }
    }
    best
}

/// For each row `y` of the union of all view rects (top to bottom), the row's disjoint
/// x-intervals `[x0, x1)` in ascending order: every pixel of the union exactly once.
pub fn union_rows(rects: &[[i32; 4]], mut f: impl FnMut(i32, &[(i32, i32)])) {
    let Some(y0) = rects.iter().map(|r| r[1]).min() else { return };
    let y1 = rects.iter().map(|r| r[3]).max().unwrap_or(y0);
    let mut spans: Vec<(i32, i32)> = Vec::with_capacity(rects.len());
    let mut merged: Vec<(i32, i32)> = Vec::with_capacity(rects.len());
    for y in y0..y1 {
        spans.clear();
        spans.extend(rects.iter().filter(|r| y >= r[1] && y < r[3]).map(|r| (r[0], r[2])));
        if spans.is_empty() {
            continue;
        }
        spans.sort_unstable();
        merged.clear();
        for &(a, b) in &spans {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        f(y, &merged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_rows_covers_each_pixel_once() {
        let rects = [[0, 0, 10, 4], [5, 2, 20, 6], [30, 3, 32, 5]];
        let mut seen = std::collections::HashSet::new();
        union_rows(&rects, |y, spans| {
            for &(a, b) in spans {
                for x in a..b {
                    assert!(seen.insert((x, y)));
                }
            }
        });
        let want: usize = (0..6)
            .map(|y| (0..40).filter(|&x| rects.iter().any(|r| x >= r[0] && x < r[2] && y >= r[1] && y < r[3])).count())
            .sum();
        assert_eq!(seen.len(), want);
    }
}
