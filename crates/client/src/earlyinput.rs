//! This frame's input for this frame's capture (`early_input`).
//!
//! Noita polls keyboard, mouse and gamepad before each world update, and its ControlsSystem
//! (`FUN_00a8d7f0`) writes that state into the player's ControlsComponent during the update,
//! where vanilla's player reacts to it in the same update. In a session that component is the
//! input proxy's (see `slots`), and the mod captures it in `OnWorldPreUpdate`, i.e. before
//! the update: it read the state written in the previous update, one frame late. Here the
//! ControlsSystem runs for the proxy right before the `OnWorldPreUpdate` dispatch (called
//! from the `early_window` hook, before the simulation window opens), and the system's own
//! update skips the proxy in that frame, so the capture reads this frame's poll, exactly once
//! per frame as in vanilla (press stamps, delay lines, wheel counts, mouse deltas).
//!
//! - The world frame counter reads one more during the call: the press stamps
//!   (`mButtonFrame*`) carry the number of the update that follows, as the system writes them.
//! - The mouse is mapped to the world through the camera of that moment, this client's render
//!   camera (the view the player aimed in), not the shared simulation camera of the window.
//!
//! Engine addresses are build 0x6794ee3c.

use crate::patches::{expect, rd, rel32, write_code, PatchError, GAME_GLOBAL_PTR};
use std::sync::atomic::{AtomicU32, Ordering};

/// ControlsSystem component update: thiscall(system, entity, component), `ret 8`.
const CONTROLS_UPDATE_FN: u32 = 0x00a8_d7f0;
/// Its calls: the system's update loop (`FUN_00c522b0`) and the job of its parallel variant
/// (`FUN_00d8b830`).
const CONTROLS_UPDATE_CALLS: [u32; 2] = [0x00c5_23f7, 0x00d8_b863];
/// The entity id the loop sets while it updates that entity's component (-1 after).
const CURRENT_ENTITY_ID: u32 = 0x0120_4ff0;
/// Component: nonzero while enabled (the loops skip disabled components).
const COMPONENT_ENABLED: u32 = 0x14;

/// The ControlsSystem (`this` of its component updates; recorded on the first one).
static SYSTEM: AtomicU32 = AtomicU32::new(0);
/// The component updated early, and the number of the world update it was updated for.
static EARLY_COMPONENT: AtomicU32 = AtomicU32::new(0);
static EARLY_FRAME: AtomicU32 = AtomicU32::new(u32::MAX);

type UpdateFn = unsafe extern "thiscall" fn(u32, u32, u32);

unsafe extern "thiscall" fn controls_update(system: u32, entity: u32, component: u32) {
    SYSTEM.store(system, Ordering::Relaxed);
    if component == EARLY_COMPONENT.load(Ordering::Relaxed) {
        let gg = rd(GAME_GLOBAL_PTR);
        if gg != 0 && rd(gg) == EARLY_FRAME.load(Ordering::Relaxed) {
            return;
        }
    }
    let f: UpdateFn = std::mem::transmute(CONTROLS_UPDATE_FN as usize);
    f(system, entity, component);
}

/// Run the ControlsSystem for the input proxy now, for the coming world update.
///
/// # Safety
/// Game thread, in DeathMatch::Update before the `OnWorldPreUpdate` dispatch.
pub unsafe fn update_proxy() {
    let system = SYSTEM.load(Ordering::Relaxed);
    let gg = rd(GAME_GLOBAL_PTR);
    if system == 0 || gg == 0 {
        return;
    }
    let Some((entity, component)) = crate::slots::proxy_controls() else { return };
    if *((component + COMPONENT_ENABLED) as *const u8) == 0 {
        return;
    }
    let frame = rd(gg);
    let next = frame.wrapping_add(1);
    *(gg as *mut u32) = next;
    *(CURRENT_ENTITY_ID as *mut u32) = rd(entity);
    let f: UpdateFn = std::mem::transmute(CONTROLS_UPDATE_FN as usize);
    f(system, entity, component);
    *(CURRENT_ENTITY_ID as *mut u32) = u32::MAX;
    *(gg as *mut u32) = frame;
    EARLY_COMPONENT.store(component, Ordering::Relaxed);
    EARLY_FRAME.store(next, Ordering::Relaxed);
}

/// Whether the proxy's ControlsComponent holds this frame's input (updated early for the
/// coming world update).
pub fn fresh() -> bool {
    unsafe {
        let gg = rd(GAME_GLOBAL_PTR);
        gg != 0 && EARLY_FRAME.load(Ordering::Relaxed) == rd(gg).wrapping_add(1)
    }
}

/// New run (`no_next_run`): the old world's ControlsSystem and proxy component are gone.
pub fn reset_run() {
    SYSTEM.store(0, Ordering::Relaxed);
    EARLY_COMPONENT.store(0, Ordering::Relaxed);
    EARLY_FRAME.store(u32::MAX, Ordering::Relaxed);
}

/// Patch `early_input` (module docs; the early call itself is in `early_window`).
///
/// # Safety
/// Game thread, noita.exe build 0x6794ee3c.
pub unsafe fn patch() -> Result<(), PatchError> {
    for site in CONTROLS_UPDATE_CALLS {
        let r = rel32(site + 5, CONTROLS_UPDATE_FN);
        expect(site, &[0xE8, r[0], r[1], r[2], r[3]])?;
    }
    let hook = controls_update as UpdateFn as u32;
    for site in CONTROLS_UPDATE_CALLS {
        let r = rel32(site + 5, hook);
        write_code(site, &[0xE8, r[0], r[1], r[2], r[3]])?;
    }
    Ok(())
}
