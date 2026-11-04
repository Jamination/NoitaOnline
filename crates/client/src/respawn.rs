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
//! - **Cause of death** (`patch_death_cause`): a slot dies one frame after its fatal hit (it
//!   waits at 0 HP, `wait_for_kill_flag_on_death`, until it is stored), through the damage
//!   model's deferred death (`FUN_00ab6d00` @0x00ab72d5), which calls KillMe (`FUN_00abb7d0`)
//!   with the message "deferred death", damage type 1 and an empty damage info, i.e. no killer.
//!   The fatal hit's own KillMe call (`FUN_00ab9a90` @0x00abb4fc, which returns at once while
//!   the entity waits) is recorded, and the deferred death replays it: KillMe gets the fatal
//!   hit's message, type and damage info, as vanilla's immediate death does. So Message_Death
//!   carries the cause and the killer, GameStatsSystem writes vanilla's `killed_by` from it
//!   (`FUN_00b196c0`), and the ragdoll gets the fatal hit's impulse. Only slots are recorded
//!   (vanilla entities that wait for `kill_now` keep their own deferred death).
//! - **Game-over stats**: vanilla's player death (`FUN_006b77d0`, players-vector entries only)
//!   broadcasts Message_PlayerDied, whose GameStatsSystem handler (`FUN_00b19d80`) finishes
//!   the run's stats (dead flag, `killed_by_extra`, gold, play time) and copies them for the
//!   game-over screen (`FUN_00b18930`: cause of death, gold, time, depth, ...). For this
//!   client's own player that handler runs right after its replayed KillMe (stats are this
//!   client's presentation: nothing in the simulation reads them).
//!
//! Engine addresses are build 0x6794ee3c.

use crate::patches::{expect, rd, rel32, write_code, Exec, PatchError};
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
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
    FATAL_HITS.lock().unwrap_or_else(|p| p.into_inner()).clear();
    LOCAL_DEATH.store(0, Ordering::Relaxed);
}

/// Damage model KillMe (`FUN_00abb7d0`): ECX entity, EDX its DamageModelComponent, stack:
/// message (std::string*), damage type, damage info; the caller pops the stack arguments.
const KILL_ME_FN: u32 = 0x00ab_b7d0;
/// KillMe for a fatal hit (damage application `FUN_00ab9a90`); while the entity waits for
/// `kill_now` it returns at once.
const KILL_ME_HIT_CALL: u32 = 0x00ab_b4fc;
/// KillMe of the deferred death (DamageModelSystem update `FUN_00ab6d00`, once `kill_now` is
/// set): message "deferred death", type 1, a default damage info.
const KILL_ME_DEFERRED_CALL: u32 = 0x00ab_72d5;
/// Entity +0xc: > 0 once killed.
const ENTITY_KILLED: u32 = 0xc;
/// DamageModelComponent: `wait_for_kill_flag_on_death`, `kill_now`.
const DM_WAIT_FOR_KILL_FLAG: u32 = 0x1f5;
const DM_KILL_NOW: u32 = 0x1f6;
/// Damage info (0xe0 bytes; constructor `FUN_0065c1a0`, destructor `FUN_0065c300`, both
/// fastcall): plain fields and two config objects (vtable and plain fields), except three
/// std::strings.
const DAMAGE_INFO_CTOR_FN: u32 = 0x0065_c1a0;
const DAMAGE_INFO_DTOR_FN: u32 = 0x0065_c300;
const DAMAGE_INFO_WORDS: usize = 0xe0 / 4;
const DAMAGE_INFO_PLAIN: [(u32, u32); 2] = [(0, 0x70), (0xa0, 0xc8)];
const DAMAGE_INFO_STRINGS: [u32; 3] = [0x70, 0x88, 0xc8];
/// MSVC std::string (0x18 bytes): `assign(const string&, pos, count)` and the destructor
/// (thiscall).
const STRING_ASSIGN_FN: u32 = 0x0041_d540;
const STRING_DTOR_FN: u32 = 0x0041_cce0;
/// thiscall(manager, entity) -> the entity's GameStatsComponent.
const GAME_STATS_OF_FN: u32 = 0x0091_c090;
/// GameStatsComponent `is_player`.
const GAME_STATS_IS_PLAYER: u32 = 0x78;
/// GameStatsSystem's Message_PlayerDied handler (thiscall(listener (unused), entity,
/// GameStatsComponent, message)).
const PLAYER_DIED_STATS_FN: u32 = 0x00b1_9d80;
/// Message_PlayerDied's vtable; +0x18: the dead player's Entity*.
const MESSAGE_PLAYER_DIED_VTABLE: u32 = 0x00ff_989c;
/// The all-time stats' `death_count` (`StatsGlobalGetValue`: 0x01208940 + 0x1b0, +8).
const GLOBAL_DEATH_COUNT: u32 = 0x0120_8af8;

/// A slot's fatal hit, as KillMe got it: the message, the damage type and a copy of the
/// damage info (engine-built objects in our memory, destroyed with the record).
struct FatalHit {
    id: u32,
    message: Box<[u32; 6]>,
    kind: u32,
    info: Box<[u32; DAMAGE_INFO_WORDS]>,
}

impl Drop for FatalHit {
    fn drop(&mut self) {
        unsafe {
            let string_dtor: extern "thiscall" fn(u32) = std::mem::transmute(STRING_DTOR_FN as usize);
            string_dtor(self.message.as_ptr() as u32);
            let info_dtor: extern "fastcall" fn(u32) = std::mem::transmute(DAMAGE_INFO_DTOR_FN as usize);
            info_dtor(self.info.as_ptr() as u32);
        }
    }
}

/// Slots that took their fatal hit and wait for their deferred death.
static FATAL_HITS: Mutex<Vec<FatalHit>> = Mutex::new(Vec::new());
/// Entity id of this client's own player while it dies (set when it is stored, consumed by its
/// deferred death).
static LOCAL_DEATH: AtomicU32 = AtomicU32::new(0);

/// This client's own player is about to die (`players.lua`, inside the window): its death
/// finishes this client's game-over stats.
pub fn set_local_death(id: u32) {
    LOCAL_DEATH.store(id, Ordering::Relaxed);
}

/// Install `death_cause` (module docs).
///
/// # Safety
/// Game thread, noita.exe build 0x6794ee3c, before the world exists.
pub(crate) unsafe fn patch_death_cause(ex: &mut Exec) -> Result<(), PatchError> {
    let hit: extern "C" fn(u32, u32, u32, u32, u32) = kill_me_hit;
    let deferred: extern "C" fn(u32, u32, u32, u32, u32) = kill_me_deferred;
    for (site, target) in [(KILL_ME_HIT_CALL, hit as usize as u32), (KILL_ME_DEFERRED_CALL, deferred as usize as u32)] {
        expect(site, &[0xE8])?;
        expect(site + 1, &rel32(site + 5, KILL_ME_FN))?;
        // push [esp+0xc] (info); push [esp+0xc] (type); push [esp+0xc] (message); push edx;
        // push ecx; call target; add esp,0x14; ret (the caller pops its three arguments)
        let mut stub = vec![0xFF, 0x74, 0x24, 0x0C, 0xFF, 0x74, 0x24, 0x0C, 0xFF, 0x74, 0x24, 0x0C, 0x52, 0x51, 0xE8, 0, 0, 0, 0, 0x83, 0xC4, 0x14, 0xC3];
        let at = ex.put(&stub);
        stub[15..19].copy_from_slice(&rel32(at + 19, target));
        write_code(at, &stub)?;
        let mut call = vec![0xE8];
        call.extend_from_slice(&rel32(site + 5, at));
        write_code(site, &call)?;
    }
    Ok(())
}

unsafe fn kill_me(e: u32, dm: u32, message: u32, kind: u32, info: u32) {
    let args = [message, kind, info];
    std::arch::asm!(
        "push dword ptr [{a} + 8]",
        "push dword ptr [{a} + 4]",
        "push dword ptr [{a}]",
        "call {f}",
        "add esp, 12",
        a = in(reg) args.as_ptr(),
        f = in(reg) KILL_ME_FN,
        in("ecx") e,
        in("edx") dm,
        clobber_abi("C"),
    );
}

/// A fatal hit's KillMe: a slot waiting for `kill_now` (it returns at once) keeps the hit for
/// its deferred death; the first fatal hit counts, as vanilla's death ends the entity there.
extern "C" fn kill_me_hit(e: u32, dm: u32, message: u32, kind: u32, info: u32) {
    unsafe {
        let waits = rd(e + ENTITY_KILLED) as i32 <= 0
            && *((dm + DM_WAIT_FOR_KILL_FLAG) as *const u8) != 0
            && *((dm + DM_KILL_NOW) as *const u8) == 0;
        let id = rd(e);
        if waits && crate::slots::is_slot(id) {
            let mut hits = FATAL_HITS.lock().unwrap_or_else(|p| p.into_inner());
            if !hits.iter().any(|h| h.id == id) {
                hits.push(FatalHit { id, message: copy_string(message), kind, info: copy_damage_info(info) });
            }
        }
        kill_me(e, dm, message, kind, info);
    }
}

/// The deferred death's KillMe: a slot dies of its fatal hit. This client's own player's
/// death then finishes the game-over stats as vanilla's player death does.
extern "C" fn kill_me_deferred(e: u32, dm: u32, message: u32, kind: u32, info: u32) {
    unsafe {
        let id = rd(e);
        let hit = {
            let mut hits = FATAL_HITS.lock().unwrap_or_else(|p| p.into_inner());
            hits.iter().position(|h| h.id == id).map(|i| hits.remove(i))
        };
        match &hit {
            Some(h) => kill_me(e, dm, h.message.as_ptr() as u32, h.kind, h.info.as_ptr() as u32),
            None => kill_me(e, dm, message, kind, info),
        }
        drop(hit);
        if id != 0 && LOCAL_DEATH.compare_exchange(id, 0, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            player_died_stats(e);
        }
    }
}

/// A std::string copy of the one at `src`, in our memory.
unsafe fn copy_string(src: u32) -> Box<[u32; 6]> {
    let s = Box::new([0, 0, 0, 0, 0, 15]);
    let assign: extern "thiscall" fn(u32, u32, u32, u32) -> u32 = std::mem::transmute(STRING_ASSIGN_FN as usize);
    assign(s.as_ptr() as u32, src, 0, u32::MAX);
    s
}

/// A copy of the damage info at `src`, in our memory.
unsafe fn copy_damage_info(src: u32) -> Box<[u32; DAMAGE_INFO_WORDS]> {
    let info = Box::new([0u32; DAMAGE_INFO_WORDS]);
    let at = info.as_ptr() as u32;
    let ctor: extern "fastcall" fn(u32) -> u32 = std::mem::transmute(DAMAGE_INFO_CTOR_FN as usize);
    ctor(at);
    for (a, b) in DAMAGE_INFO_PLAIN {
        std::ptr::copy_nonoverlapping((src + a) as *const u8, (at + a) as *mut u8, (b - a) as usize);
    }
    let assign: extern "thiscall" fn(u32, u32, u32, u32) -> u32 = std::mem::transmute(STRING_ASSIGN_FN as usize);
    for o in DAMAGE_INFO_STRINGS {
        assign(at + o, src + o, 0, u32::MAX);
    }
    info
}

/// Vanilla's player death broadcasts Message_PlayerDied (`FUN_006b77d0`), and GameStatsSystem
/// finishes the run's stats for the game-over screen (`FUN_00b19d80`: `killed_by_extra`,
/// gold, play time, then `FUN_00b18930`, which formats the cause of death from `killed_by`).
/// Slots are not in the players vector, so this runs that handler for this client's own
/// player. It also adds the run to the all-time stats, whose `death_count` world generation
/// reads (`StatsGlobalGetValue`: the starting wand's spell, biome modifiers): it differs between
/// clients that died different numbers of times, so it is kept at its session start.
unsafe fn player_died_stats(e: u32) {
    let mgr = rd(ENTITY_MANAGER);
    if mgr == 0 {
        return;
    }
    let stats_of: extern "thiscall" fn(u32, u32) -> u32 = std::mem::transmute(GAME_STATS_OF_FN as usize);
    let stats = stats_of(mgr, e);
    if stats == 0 || *((stats + GAME_STATS_IS_PLAYER) as *const u8) == 0 {
        return;
    }
    let mut message = [0u32; 8];
    message[0] = MESSAGE_PLAYER_DIED_VTABLE;
    message[6] = e;
    let deaths = rd(GLOBAL_DEATH_COUNT);
    let handler: extern "thiscall" fn(u32, u32, u32, u32) = std::mem::transmute(PLAYER_DIED_STATS_FN as usize);
    handler(0, e, stats, message.as_ptr() as u32);
    *(GLOBAL_DEATH_COUNT as *mut u32) = deaths;
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
