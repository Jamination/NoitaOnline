//! Next run: when every player is dead at the same time the game is over for everyone, and
//! every client starts a new game into a new world (the server's next seed, same roster).
//!
//! Vanilla's new game: the game-over menu's "New game" pushes the game-mode screen
//! `FUN_006ceb00` on the menu stack (`0x012076bc`, `FUN_008a5dd0`), which loads the game modes
//! (`FUN_006378a0`); with the game over set, picking a mode calls `FUN_006cc030(mode, 0)`. With
//! the mod list unchanged that starts a fade (`FUN_006c5e40`, advanced by the menus'
//! `FUN_006c5ea0`, e.g. the game-over menu's) whose end runs a lambda (`FUN_009a2430`) setting
//! the game mode's restart request (`[0x01204bc0]+0x7d`). The end of that frame's
//! DeathMatch::Update (`FUN_006b59d0` @0x006b5c08) then calls `FUN_006af010`, which rebuilds the
//! game in place (`FUN_006e9f30`): the world, its entities, Box2D and the mods' Lua states are
//! torn down and recreated, and every mod's `init.lua` runs again (`files/nextrun.lua`,
//! `init.lua`), all inside that call. GameGlobal (`[0x0122374c]`) and the engine's other
//! statics stay, and so does the client runtime (this DLL, its connection and patches). (A
//! changed mod list restarts the application's main loop instead; a session's mods never
//! change.)
//!
//! - `world_teardown` (patch): at the restart call, before the teardown, the background work
//!   that reads the world is joined: the Box2D step (sent to its worker at the frame's render
//!   start, which comes before), the frame's state report (hashed from the buffer swap on,
//!   run now) and the emitter scan (its result is dropped with the world).
//! - [`reset_run`]: every per-world piece of the runtime's state starts as in a fresh process,
//!   identically on every client.
//! - [`press_new_game`]: what the "New game" button and the mode screen do, for scripted runs.
//!
//! Engine addresses are build 0x6794ee3c.

use crate::patches::{expect, rd, redirect_call, Exec, PatchError, GAME_GLOBAL_PTR};
use std::sync::atomic::{AtomicU32, Ordering};

/// The end of DeathMatch::Update's frame (`FUN_006b59d0`): `call FUN_006af010` (fastcall, ECX =
/// the game mode) when a restart was requested (game mode +0x7d).
const RESTART_CALL: u32 = 0x006b_5c08;
const RESTART_FN: u32 = 0x006a_f010;
/// fastcall(mode index, 0): start a new game in that game mode (see the module docs).
const NEW_GAME_FN: u32 = 0x006c_c030;
const NEW_GAME_PROLOGUE: [u8; 8] = [0x55, 0x8B, 0xEC, 0x83, 0xE4, 0xF8, 0x6A, 0xFF];
/// The current game's mode: its index in the modes below (set at the game's start by
/// `FUN_006381f0`, from the mode screen's choice in GameGlobal+0x5c or from `-gamemode`).
const CURRENT_GAME_MODE: u32 = 0x0120_8420;
/// The game modes (`std::vector`, 0xc0-byte entries), filled by `FUN_006378a0` (fastcall,
/// ECX = the vector) when the game-mode screen opens (empty in a game started with
/// `-gamemode`).
const LOAD_GAME_MODES_FN: u32 = 0x0063_78a0;
const LOAD_GAME_MODES_PROLOGUE: [u8; 8] = [0x55, 0x8B, 0xEC, 0x6A, 0xFF, 0x68, 0x07, 0xED];
const GAME_MODES_BEGIN: u32 = 0x0120_8414;
const GAME_MODES_END: u32 = 0x0120_8418;
const GAME_MODE_SIZE: u32 = 0xc0;

/// 1 + the frame at which every player was dead (the run is over); 0 while it runs.
static RUN_OVER: AtomicU32 = AtomicU32::new(0);

/// Every player is dead (`NextRun.all_dead`, inside the window, the same frame everywhere):
/// the next new game starts the next run.
pub fn set_run_over(frame: u32) {
    RUN_OVER.store(frame.wrapping_add(1).max(1), Ordering::Relaxed);
}

/// The frame at which the run ended, if it did.
pub fn run_over() -> Option<u32> {
    match RUN_OVER.load(Ordering::Relaxed) {
        0 => None,
        f => Some(f - 1),
    }
}

/// Patch `world_teardown` (module docs).
///
/// # Safety
/// Patch installation.
pub(crate) unsafe fn patch(ex: &mut Exec) -> Result<(), PatchError> {
    let _ = ex;
    expect(NEW_GAME_FN, &NEW_GAME_PROLOGUE)?;
    expect(LOAD_GAME_MODES_FN, &LOAD_GAME_MODES_PROLOGUE)?;
    redirect_call(RESTART_CALL, RESTART_FN, (restart as extern "fastcall" fn(u32)) as u32)
}

/// The game restarts in place (a new game): nothing of ours may read the old world after this.
extern "fastcall" fn restart(game_mode: u32) {
    world_teardown();
    let f: extern "fastcall" fn(u32) = unsafe { std::mem::transmute(RESTART_FN as usize) };
    f(game_mode);
}

fn world_teardown() {
    crate::box2d::join();
    crate::report::join();
    crate::cosmetic::reset_run();
}

/// Reset every per-run piece of the runtime's state (the old world is gone; call before the
/// new world exists, i.e. from the new game's `init.lua`).
///
/// # Safety
/// Game thread.
pub unsafe fn reset_run() {
    world_teardown();
    crate::patches::reset_run();
    crate::box2d::reset_run();
    crate::cosmetic::reset_run();
    crate::camreaders::reset_run();
    crate::slots::reset_run();
    crate::invgui::reset_run();
    crate::earlyinput::reset_run();
    crate::views::reset_run();
    crate::respawn::reset_run();
    crate::reset_input_trailer();
    RUN_OVER.store(0, Ordering::Relaxed);
}

/// Start a new game in the current game mode, as the game-over menu's "New game" (opening
/// the game-mode screen, which loads the modes) followed by that screen's button for the mode
/// do (scripted runs; outside the simulation window).
/// Errs if there is no game or mode to restart.
///
/// # Safety
/// Game thread, outside the simulation window.
pub unsafe fn press_new_game() -> Result<(), String> {
    let gg = rd(GAME_GLOBAL_PTR);
    if gg == 0 {
        return Err("no game".into());
    }
    let mode = rd(CURRENT_GAME_MODE);
    // The mode screen loads the modes as it opens.
    let load: extern "fastcall" fn(u32) = std::mem::transmute(LOAD_GAME_MODES_FN as usize);
    load(GAME_MODES_BEGIN);
    let modes = rd(GAME_MODES_END).wrapping_sub(rd(GAME_MODES_BEGIN)) / GAME_MODE_SIZE;
    if mode >= modes {
        return Err(format!("game mode {mode} of {modes}"));
    }
    let new_game: extern "fastcall" fn(u32, u32) = std::mem::transmute(NEW_GAME_FN as usize);
    new_game(mode, 0);
    Ok(())
}
