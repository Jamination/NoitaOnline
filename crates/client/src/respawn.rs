//! Player death and respawn versus vanilla's single player (see files/players.lua).
//!
//! - **Keeping the player** (`save` / `restore`): a dying player is stored with the engine's
//!   own entity serializer, the one polymorph uses to keep the player while it is a creature
//!   (`FUN_00b041e0` polymorph start: `CSerialSaver` + `FUN_00a59d50`; `FUN_00b015d0` polymorph
//!   end: `CSerialLoader` + new entity + `FUN_00a5a190`). The record holds the whole entity
//!   with its children: perks' components and effect entities, max HP, wands, spells, items,
//!   robe sprites. A respawn loads it as a new entity, as a polymorph's end does. A player
//!   who dies as a creature is stored by the polymorph itself (the effect's
//!   `mSerializedData`, base64 by `FUN_00afece0`): `save_base64` decodes that record.
//! - **Game over** (`sim_enter` / `sim_leave`): vanilla's game over (Lua
//!   `GameTriggerGameOver`, `FUN_006b84f0`) sets the game mode's flag +0x90, which the world
//!   update reads inside the simulation window (damage flash fade `FUN_006f1d40` @0x006f2088,
//!   camera update `FUN_006b3ff0`). A dead player's game over is this client's alone, so inside
//!   the window the flag holds the session's shared value (set when every player is dead,
//!   identically on every client) and outside it this client's own (the game-over menu, drawn
//!   by the game mode update before the window, and the pause handling read it there).
//! - **Respawn button** (`patch_game_over_button`): the game-over menu (`FUN_006e51e0`) offers
//!   "New game", which queues `FUN_006ceb00`. While this client's player is dead and another
//!   lives, the button reads "Respawn" and only records the press; the mod then shows its
//!   respawn picker. Otherwise it is vanilla's.
//!
//! Engine addresses are build 0x6794ee3c.

use crate::patches::{expect, rd, rel32, write_code, Exec, PatchError};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

/// EntityManager* (`[0x01204b98]`).
const ENTITY_MANAGER: u32 = 0x0120_4b98;
/// fastcall(manager) -> new empty Entity* (EntityCreateNew, polymorph end).
const ENTITY_NEW_FN: u32 = 0x0056_e590;
/// fastcall(entity, serializer): write the entity and its children (polymorph start).
const ENTITY_SAVE_FN: u32 = 0x00a5_9d50;
/// (ECX entity, EDX serializer, [esp+4] &{x, y}), caller pops: read an entity written by
/// `ENTITY_SAVE_FN` into `entity`, placed at x, y (polymorph end).
const ENTITY_LOAD_FN: u32 = 0x00a5_a190;
/// `network_utils::CSerialSaver` constructor: thiscall(this, most_derived), ret 4.
const SAVER_CTOR_FN: u32 = 0x0044_e080;
/// `network_utils::CSerialLoader` constructor: thiscall(this, &std::string, most_derived), ret 8.
const LOADER_CTOR_FN: u32 = 0x0044_e790;
/// Serializer destructor body (frees the buffer string at +4): fastcall(this).
const STREAM_DTOR_FN: u32 = 0x0044_ec70;
/// Serializer object size (vbtable ptr, std::string buffer at +4, positions, ISerializer
/// virtual base at +0x2c).
const STREAM_SIZE: usize = 0x30;
/// thiscall(manager, entity) -> the entity's first enabled Inventory2Component.
const INVENTORY2_OF_FN: u32 = 0x0091_5bd0;
/// Inventory2Component::mForceRefresh, set by polymorph end on the restored player.
const INVENTORY2_FORCE_REFRESH: u32 = 0x6b;

/// Game mode (`[0x01204bc0]`); +0x90: game over.
const GAME_MODE_PTR: u32 = 0x0120_4bc0;
const GAME_MODE_GAME_OVER: u32 = 0x90;
/// Game global (`[0x0122374c]`) +0x44 -> +0x14: music; +0xdd: music off (set by game over).
const GG_AUDIO: u32 = 0x44;
const AUDIO_MUSIC: u32 = 0x14;
const MUSIC_OFF: u32 = 0xdd;

/// Game-over menu "New game" button: `push 0x010124fc` ("$menugameover_newgame") builds its
/// label; when clicked, `lea eax,[ebp-0x448]; mov [ebp-0x448],0x006ceb00; push eax;
/// mov ecx,0x012076bc; call FUN_008a5dd0` queues the new game; then 0x006e6299.
const BUTTON_LABEL_PUSH: u32 = 0x006e_61cc;
const BUTTON_LABEL_ORIG: [u8; 5] = [0x68, 0xFC, 0x24, 0x01, 0x01];
const BUTTON_ACTION: u32 = 0x006e_627e;
const BUTTON_ACTION_ORIG: [u8; 27] = [
    0x8D, 0x85, 0xB8, 0xFB, 0xFF, 0xFF, // lea eax,[ebp-0x448]
    0xC7, 0x85, 0xB8, 0xFB, 0xFF, 0xFF, 0x00, 0xEB, 0x6C, 0x00, // mov [ebp-0x448],0x006ceb00
    0x50, // push eax
    0xB9, 0xBC, 0x76, 0x20, 0x01, // mov ecx,0x012076bc
    0xE8, 0x37, 0xFB, 0x1B, 0x00, // call FUN_008a5dd0
];
const BUTTON_DONE: u32 = 0x006e_6299;
const NEW_GAME_FN: u32 = 0x006c_eb00;
const MENU_QUEUE: u32 = 0x0120_76bc;
const MENU_QUEUE_PUSH_FN: u32 = 0x008a_5dd0;
const VANILLA_LABEL: &[u8] = b"$menugameover_newgame\0";
const RESPAWN_LABEL: &[u8] = b"Respawn\0";

/// The button's label text (NUL-terminated), read by the patched `push`.
static mut LABEL: [u8; 32] = [0; 32];

/// Stored players, by session slot index.
static SAVED: Mutex<Vec<Option<Vec<u8>>>> = Mutex::new(Vec::new());

/// The game-over flag inside the window (every player dead), and this client's own outside it.
static SHARED_GAME_OVER: AtomicU8 = AtomicU8::new(0);
static LOCAL_GAME_OVER: AtomicU8 = AtomicU8::new(0);
static IN_WINDOW: AtomicU8 = AtomicU8::new(0);
/// The game-over button respawns (this client's player is dead and another lives).
static RESPAWN_MODE: AtomicU8 = AtomicU8::new(0);
/// The respawn button was clicked since the last `take_press`.
static PRESSED: AtomicU8 = AtomicU8::new(0);

/// Install the game-over button patch.
///
/// # Safety
/// Game thread, noita.exe build 0x6794ee3c, before the world exists.
pub(crate) unsafe fn patch_game_over_button(ex: &mut Exec) -> Result<(), PatchError> {
    expect(BUTTON_LABEL_PUSH, &BUTTON_LABEL_ORIG)?;
    expect(BUTTON_ACTION, &BUTTON_ACTION_ORIG)?;
    set_label(VANILLA_LABEL);
    let label = (std::ptr::addr_of!(LABEL) as u32).to_le_bytes();
    write_code(BUTTON_LABEL_PUSH, &[0x68, label[0], label[1], label[2], label[3]])?;
    // pushad; call button_clicked; popad; jmp BUTTON_DONE
    let click: extern "C" fn() = button_clicked;
    let mut stub = vec![0x60, 0xE8, 0, 0, 0, 0, 0x61, 0xE9, 0, 0, 0, 0];
    let at = ex.put(&stub);
    stub[2..6].copy_from_slice(&rel32(at + 6, click as usize as u32));
    stub[8..12].copy_from_slice(&rel32(at + 12, BUTTON_DONE));
    write_code(at, &stub)?;
    let mut jmp = vec![0xE9];
    jmp.extend_from_slice(&rel32(BUTTON_ACTION + 5, at));
    jmp.resize(BUTTON_ACTION_ORIG.len(), 0x90);
    write_code(BUTTON_ACTION, &jmp)
}

unsafe fn set_label(text: &[u8]) {
    let l = &mut *std::ptr::addr_of_mut!(LABEL);
    let n = text.len().min(l.len());
    l[..n].copy_from_slice(&text[..n]);
    l[n - 1] = 0;
}

/// The game-over button was clicked (game thread, inside the menu update).
extern "C" fn button_clicked() {
    if RESPAWN_MODE.load(Ordering::Relaxed) != 0 {
        PRESSED.store(1, Ordering::Relaxed);
        return;
    }
    // Vanilla: queue the new game.
    let fp: u32 = NEW_GAME_FN;
    unsafe {
        let push: extern "thiscall" fn(u32, *const u32) = std::mem::transmute(MENU_QUEUE_PUSH_FN as usize);
        push(MENU_QUEUE, &fp);
    }
}

/// Whether the game-over button respawns (1) or starts a new game (0, vanilla).
pub fn set_respawn_mode(on: bool) {
    RESPAWN_MODE.store(on as u8, Ordering::Relaxed);
    unsafe { set_label(if on { RESPAWN_LABEL } else { VANILLA_LABEL }) };
}

/// Whether the respawn button was clicked since the last call.
pub fn take_press() -> bool {
    PRESSED.swap(0, Ordering::Relaxed) != 0
}

unsafe fn game_mode() -> u32 {
    rd(GAME_MODE_PTR)
}

/// Entering the simulation window: the game-over flag shows the shared state.
///
/// # Safety
/// Game thread.
pub unsafe fn sim_enter() {
    let gm = game_mode();
    if gm == 0 {
        return;
    }
    let p = (gm + GAME_MODE_GAME_OVER) as *mut u8;
    LOCAL_GAME_OVER.store(*p, Ordering::Relaxed);
    *p = SHARED_GAME_OVER.load(Ordering::Relaxed);
    IN_WINDOW.store(1, Ordering::Relaxed);
}

/// Leaving the simulation window: the game-over flag shows this client's own state again.
///
/// # Safety
/// Game thread.
pub unsafe fn sim_leave() {
    if IN_WINDOW.swap(0, Ordering::Relaxed) == 0 {
        return;
    }
    let gm = game_mode();
    if gm != 0 {
        *((gm + GAME_MODE_GAME_OVER) as *mut u8) = LOCAL_GAME_OVER.load(Ordering::Relaxed);
    }
}

/// Every player is dead (inside the window, identically on every client): the world update
/// sees vanilla's game over from now on.
pub fn set_shared_game_over() {
    SHARED_GAME_OVER.store(1, Ordering::Relaxed);
}

/// This client's player respawned: undo its game over (outside the window): the flag, and
/// the music switched off by it. The game-over menu's own state is reset by the next game
/// over (`FUN_006e5080`).
///
/// # Safety
/// Game thread, outside the simulation window.
pub unsafe fn clear_local_game_over() {
    let gm = game_mode();
    if gm != 0 {
        *((gm + GAME_MODE_GAME_OVER) as *mut u8) = 0;
    }
    LOCAL_GAME_OVER.store(0, Ordering::Relaxed);
    let gg = rd(crate::patches::GAME_GLOBAL_PTR);
    let audio = if gg != 0 { rd(gg + GG_AUDIO) } else { 0 };
    let music = if audio != 0 { rd(audio + AUDIO_MUSIC) } else { 0 };
    if music != 0 {
        *((music + MUSIC_OFF) as *mut u8) = 0;
    }
}

/// A new run (new world): no stored players, no game over, vanilla button.
pub fn reset_run() {
    SAVED.lock().unwrap_or_else(|p| p.into_inner()).clear();
    SHARED_GAME_OVER.store(0, Ordering::Relaxed);
    LOCAL_GAME_OVER.store(0, Ordering::Relaxed);
    IN_WINDOW.store(0, Ordering::Relaxed);
    PRESSED.store(0, Ordering::Relaxed);
    set_respawn_mode(false);
}

fn store(slot: usize, bytes: Option<Vec<u8>>) {
    let mut s = SAVED.lock().unwrap_or_else(|p| p.into_inner());
    if s.len() <= slot {
        s.resize(slot + 1, None);
    }
    s[slot] = bytes;
}

/// The serializer's ISerializer virtual base (`stream + vbtable[1]`), as the engine computes it.
unsafe fn serializer_of(stream: u32) -> u32 {
    stream + rd(rd(stream) + 4)
}

/// MSVC std::string (32-bit): buffer (inline if capacity < 16) at +0, size +0x10, capacity +0x14.
unsafe fn string_bytes<'a>(s: u32) -> &'a [u8] {
    let size = rd(s + 0x10) as usize;
    let data = if rd(s + 0x14) >= 16 { rd(s) } else { s };
    std::slice::from_raw_parts(data as *const u8, size)
}

/// Store entity `id` (and its children) for session slot `slot`. Returns the record's size,
/// or -1 if the entity does not exist.
///
/// # Safety
/// Game thread, inside the simulation window.
pub unsafe fn save(slot: usize, id: u32) -> i32 {
    let e = crate::slots::entity(id);
    if e == 0 {
        store(slot, None);
        return -1;
    }
    let mut saver = [0u32; STREAM_SIZE / 4 + 4];
    let this = saver.as_mut_ptr() as u32;
    let ctor: extern "thiscall" fn(u32, u32) -> u32 = std::mem::transmute(SAVER_CTOR_FN as usize);
    ctor(this, 1);
    let save_fn: extern "fastcall" fn(u32, u32) = std::mem::transmute(ENTITY_SAVE_FN as usize);
    save_fn(e, serializer_of(this));
    let bytes = string_bytes(this + 4).to_vec();
    let dtor: extern "fastcall" fn(u32) = std::mem::transmute(STREAM_DTOR_FN as usize);
    dtor(this);
    let n = bytes.len() as i32;
    store(slot, Some(bytes));
    n
}

/// Store a player kept by a polymorph (its effect's `mSerializedData`, base64) for `slot`.
/// Returns the record's size, or -1 if it is not valid base64.
pub fn save_base64(slot: usize, text: &[u8]) -> i32 {
    match base64_decode(text) {
        Some(b) if !b.is_empty() => {
            let n = b.len() as i32;
            store(slot, Some(b));
            n
        }
        _ => {
            store(slot, None);
            -1
        }
    }
}

/// Whether a player is stored for `slot`.
pub fn has_saved(slot: usize) -> bool {
    SAVED.lock().unwrap_or_else(|p| p.into_inner()).get(slot).is_some_and(|b| b.is_some())
}

/// Load the player stored for `slot` as a new entity at x, y (the record stays stored).
/// Returns the new entity's id, 0 if nothing is stored.
///
/// # Safety
/// Game thread, inside the simulation window.
pub unsafe fn restore(slot: usize, x: f32, y: f32) -> u32 {
    let Some(bytes) = SAVED.lock().unwrap_or_else(|p| p.into_inner()).get(slot).cloned().flatten() else {
        return 0;
    };
    let mgr = rd(ENTITY_MANAGER);
    if mgr == 0 {
        return 0;
    }
    // A std::string holding the record, for the loader to copy.
    let mut text = [0u32; 6];
    let heap = bytes.as_ptr() as u32;
    text[0] = heap;
    text[4] = bytes.len() as u32;
    text[5] = (bytes.len() as u32).max(16); // capacity >= 16: buffer pointer at +0
    let mut loader = [0u32; STREAM_SIZE / 4 + 4];
    let this = loader.as_mut_ptr() as u32;
    let ctor: extern "thiscall" fn(u32, u32, u32) -> u32 = std::mem::transmute(LOADER_CTOR_FN as usize);
    ctor(this, text.as_ptr() as u32, 1);
    let new_fn: extern "fastcall" fn(u32) -> u32 = std::mem::transmute(ENTITY_NEW_FN as usize);
    let e = new_fn(mgr);
    let pos = [x, y];
    std::arch::asm!(
        "push {pos}",
        "call {f}",
        "add esp, 4",
        pos = in(reg) pos.as_ptr(),
        f = in(reg) ENTITY_LOAD_FN,
        in("ecx") e,
        in("edx") serializer_of(this),
        clobber_abi("C"),
    );
    let dtor: extern "fastcall" fn(u32) = std::mem::transmute(STREAM_DTOR_FN as usize);
    dtor(this);
    let inv_of: extern "thiscall" fn(u32, u32) -> u32 = std::mem::transmute(INVENTORY2_OF_FN as usize);
    let inv = inv_of(mgr, e);
    if inv != 0 {
        *((inv + INVENTORY2_FORCE_REFRESH) as *mut u8) = 1;
    }
    rd(e)
}

fn base64_decode(text: &[u8]) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let body: Vec<u8> = text.iter().copied().filter(|&c| c != b'=').collect();
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    for chunk in body.chunks(4) {
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= val(c)? << (18 - 6 * i);
        }
        let n = chunk.len().saturating_sub(1);
        for i in 0..n {
            out.push((acc >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::base64_decode;

    #[test]
    fn base64() {
        assert_eq!(base64_decode(b"TWFu").unwrap(), b"Man");
        assert_eq!(base64_decode(b"TWE=").unwrap(), b"Ma");
        assert_eq!(base64_decode(b"TQ==").unwrap(), b"M");
        assert_eq!(base64_decode(b"").unwrap(), b"");
        assert!(base64_decode(b"T!==").is_none());
    }
}
