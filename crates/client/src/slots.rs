//! Player slots versus vanilla's single player.
//!
//! Vanilla Noita has one player: the entity in the game mode's players vector
//! (`[GAME_MODE]+0x58`), whose ControlsComponent the engine's input system drives. In a session
//! every client holds every player's slot entity (driven by the mod from the lockstep ticks,
//! ControlsComponent field `enabled` = 0) and the players vector holds only this client's inert
//! input proxy. Engine code written for "the player" therefore needs help in two ways:
//!
//! - Polymorph (`polymorph_slots`): the engine turns "the player" into the new creature only for
//!   players-vector entries (`FUN_006b6d60` on polymorph, `FUN_006b7590` on its end), and fires a
//!   polymorphed player's attacks only for an enabled ControlsComponent (ControlsSystem
//!   `FUN_00a8d7f0`). Both run here for slots too, identically on every client; the slot
//!   registry follows the slot's entity to the creature and back (the mod reads it).
//! - Presentation (`local_presentation`, with `sound_rng`): things vanilla does only for its
//!   one player and that change nothing in the simulation happen for this client's own slot
//!   only: the gamepad camera look-ahead, aiming reticle and rumble; log messages, sounds and
//!   the out-of-air screen darkening inside "is player" gates (every slot has `player_unit`,
//!   so every client showed them for every player); the low-health indicator (it read the
//!   players vector, i.e. the proxy). The gates themselves, and everything else in those
//!   blocks, still run for every player on every client. Sounds draw from their own RNG
//!   (`sound_rng`), so playing them on one client only cannot shift the simulation's.
//!
//! Engine addresses are build 0x6794ee3c.

use crate::patches::{expect, rd, rel32, write_code, Exec, PatchError};
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU8, Ordering};

/// Game mode object (`[0x01204bc0]`, the `this` of the player swaps below); +0x58/+0x5c: the
/// players vector (`Entity*`).
const PLAYERS_BEGIN: u32 = 0x58;
const PLAYERS_END: u32 = 0x5c;
/// EntityManager*; GetEntityById: thiscall(manager, id).
const ENTITY_MANAGER: u32 = 0x0120_4b98;
const ENTITY_BY_ID_FN: u32 = 0x0056_eba0;
/// Entity: +0 id, +0xc > 0 once killed.
const ENTITY_DEAD: u32 = 0xc;
/// thiscall(manager, entity) -> the entity's first enabled ControlsComponent.
const CONTROLS_OF_FN: u32 = 0x0091_5400;
/// ControlsComponent fields: `enabled` (the engine's input system drives it only when set),
/// `polymorph_hax`, `mButtonDownFire`.
const CONTROLS_ENABLED: u32 = 0x1c4;
const CONTROLS_POLYMORPH_HAX: u32 = 0x1b0;
const CONTROLS_FIRE_DOWN: u8 = 0x48;

/// Polymorph start, player branch of the game-effect apply (`FUN_00b041e0`):
/// `call FUN_006b6d60` (thiscall(game mode, old, new, cessation)): replaces `old` by `new` in
/// the players vector and makes `new` a polymorphed player (tag `polymorphed_player`, its AI
/// off, ControlsComponent with `polymorph_hax` and `enabled` set and Run held, fog-of-war
/// radius, worm or teleportable setup, stats).
const POLY_START_CALL: u32 = 0x00b0_5a3a;
const POLY_START_FN: u32 = 0x006b_6d60;
/// Polymorph end, game-effect update (`FUN_00b015d0`), after the stored player was restored:
/// `call FUN_006b7590` (thiscall(game mode, creature, restored)): replaces the creature by
/// the restored player in the players vector and gives it a short game effect (0x50).
const POLY_END_CALL: u32 = 0x00b0_19ed;
const POLY_END_FN: u32 = 0x006b_7590;

/// ControlsSystem (`FUN_00a8d7f0`) for a ControlsComponent whose `enabled` is clear: after the
/// confusion swap it returns (`jz 0x00a8ecef` / `jmp 0x00a8ecef`). The polymorphed player's
/// attack block (@0x00a8e919: `polymorph_hax` and Fire pressed this frame and
/// `polymorph_next_attack_frame` reached -> one of the creature's AI attacks, chosen with the
/// global LCG, fired toward the aim) is only reached for an enabled one. Its exit
/// (@0x00a8ec1a: `cmp byte [esi+0x1c4],0; jz 0x00a8ec56`) continues with the enabled path's
/// confusion swap and key-binding destructors.
const CONTROLS_DISABLED_JZ: u32 = 0x00a8_d860;
const CONTROLS_DISABLED_JMP: u32 = 0x00a8_d884;
const CONTROLS_RETURN: u32 = 0x00a8_ecef;
const CONTROLS_POLY_BLOCK: u32 = 0x00a8_e919;
const CONTROLS_POLY_EXIT: u32 = 0x00a8_ec1a;
const CONTROLS_POLY_EXIT_ORIG: [u8; 9] = [0x80, 0xBE, 0xC4, 0x01, 0x00, 0x00, 0x00, 0x74, 0x33];
const CONTROLS_CONFUSION: u32 = 0x00a8_ec23;
const CONTROLS_DESTRUCTORS: u32 = 0x00a8_ec56;
/// The function's exception state (`[esp+0x358]`, -1 = nothing to unwind).
const CONTROLS_EH_STATE: [u8; 4] = [0x58, 0x03, 0x00, 0x00];

/// `FUN_00a8d5f0` (fastcall(entity)): the active gamepad if the entity's ControlsComponent
/// is enabled, else null. A slot's never is, so the gamepad-only presentation paths below
/// never ran for this client's own player.
const GAMEPAD_OF_FN: u32 = 0x00a8_d5f0;
/// Presentation-only users of `FUN_00a8d5f0`:
/// - PlatformShooterPlayerSystem camera block (only for the entity with
///   `center_camera_on_this_entity`, i.e. this client's own slot): gamepad look-ahead toward
///   `mGamepadAimingVectorRaw` instead of the mouse (@0x00bf4865);
/// - `FUN_00bf69f0` (called by that block when the gamepad state changes): shows the aiming
///   reticle sprite while a gamepad is used (@0x00bf6a37);
/// - controller rumble, `pad->vtbl[2](strength, duration)`: damage (@0x00abab15), frozen /
///   on fire (@0x00b062bc), bleeding (@0x00b44b79), wand (@0x00b2b0b5), kick / items
///   (@0x00c2f811, @0x00c300f1), player shooter (@0x00bf661f, @0x00bf3635).
/// The AI's and the ControlsSystem's own uses (simulation) are left alone.
const GAMEPAD_VIEW_CALLS: [u32; 10] = [
    0x00bf_4865, 0x00bf_6a37, 0x00ab_ab15, 0x00b0_62bc, 0x00b4_4b79, 0x00b2_b0b5, 0x00c2_f811, 0x00c3_00f1, 0x00bf_661f,
    0x00bf_3635,
];

/// `FUN_0063e880` (fastcall(entity) -> AL): the entity has tag `player_unit`, which every
/// slot has on every client.
const IS_PLAYER_FN: u32 = 0x0063_e880;
/// Presentation callees (thiscall unless noted) and the stack bytes they pop.
const LOG_FN: (u32, u16) = (0x006c_4ad0, 8); // GamePrint: log line
const IMPORTANT_FN: (u32, u16) = (0x006c_4f90, 12); // GamePrintImportant
const LOG_LINE_FN: (u32, u16) = (0x006c_50f0, 8); // log line (text object)
const SEND_GAME_LOG_FN: (u32, u16) = (0x00d1_f680, 4); // Message_GameLog to the entity's GameLogComponent
const SOUND_FN: (u32, u16) = (0x0047_d660, 12); // positional sound event
const LOOP_SOUND_FN: (u32, u16) = (0x00a3_c7c0, 0); // fastcall: named looping sound, intensity
const COPY4_FN: (u32, u16) = (0x0089_5100, 4); // 4-float copy (post-fx overlay colour)
/// Player-scoped presentation inside "is player" gates: (gate `call FUN_0063e880` testing the
/// subject entity, [(presentation call site, callee)]). The gates stay as they are (the
/// simulation in those blocks still runs for every player); the presentation calls run only
/// when the subject is this client's own player.
const GATED_PRESENTATION: &[(u32, &[(u32, (u32, u16))])] = &[
    // FUN_00affd80 teleport: "$log_teleported" (its forced camera move stays)
    (0x00af_fe07, &[(0x00af_fe44, LOG_FN)]),
    // FUN_00b041e0 game effect applied: "$log_polymorph", "$log_blindness"
    (0x00b0_51f6, &[(0x00b0_5232, LOG_FN)]),
    (0x00b0_4c70, &[(0x00b0_4ca7, LOG_FN)]),
    // FUN_00ab9a90 damage: "$log_gamefx_savinggrace", "$log_gamefx_respawn",
    // "$logdesc_gods_are_afraid" (the entity it spawns stays)
    (0x00ab_b17e, &[(0x00ab_b1b2, LOG_FN)]),
    (0x00ab_b28f, &[(0x00ab_b301, IMPORTANT_FN)]),
    (0x00ab_a7c2, &[(0x00ab_a860, IMPORTANT_FN)]),
    // FUN_009e3730 wand: "$log_recharging" and its sound, "$log_castdelaying"
    (0x009e_38a4, &[(0x009e_38e7, LOG_LINE_FN), (0x009e_3922, SOUND_FN)]),
    (0x009e_39a2, &[(0x009e_39e5, LOG_LINE_FN)]),
    // FUN_00b2a310 wand: action consumed, out of mana (x2), casting an empty wand
    (0x00b2_ad1b, &[(0x00b2_ad4b, SOUND_FN)]),
    (0x00b2_adc6, &[(0x00b2_adf6, SOUND_FN)]),
    (0x00b2_ae65, &[(0x00b2_ae95, SOUND_FN)]),
    (0x00b2_af0f, &[(0x00b2_af3f, SOUND_FN)]),
    // FUN_00b015d0 game effect update: invisibility on/off sound
    (0x00b0_29f3, &[(0x00b0_2a4f, SOUND_FN)]),
    // FUN_00ab81b0 breathing: suffocating / underwater sounds, out-of-air screen darkening
    (0x00ab_8402, &[(0x00ab_846f, LOOP_SOUND_FN), (0x00ab_84e4, LOOP_SOUND_FN)]),
    (0x00ab_851c, &[(0x00ab_85a4, COPY4_FN)]),
    // FUN_00b4f080 item equipped: "$log_equipped" and the equip sound
    (0x00b4_f386, &[(0x00b4_f428, SEND_GAME_LOG_FN), (0x00b4_f48c, SOUND_FN)]),
    // FUN_00b906a0 item pick-up: "$log_pickedup"
    (0x00b9_1428, &[(0x00b9_14ec, SEND_GAME_LOG_FN)]),
];
/// Whether the entity the last patched gate tested is this client's own player.
static SUBJECT_LOCAL: AtomicU8 = AtomicU8::new(0);

/// Low-health indicator (render, `FUN_006f21d0`): `low_health_indicator_alpha` from the HP of
/// the players-vector entities, `call FUN_0090c1b0` (thiscall(manager, entity) -> its
/// DamageModelComponent) @0x006f25c6. In a session the vector holds only the inert proxy.
const LOW_HP_DAMAGE_MODEL_CALL: u32 = 0x006f_25c6;
const DAMAGE_MODEL_OF_FN: u32 = 0x0090_c1b0;

/// Sound events (`FUN_0047b990`, from `FUN_0047d660`) randomise pitch and volume with two
/// steps of the global LCG: `imul ecx,[0x0120741c],0x343fd` @0x0047be26,
/// `mov [0x0120741c],eax` @0x0047be82.
const SOUND_LCG_READ: u32 = 0x0047_be26;
const SOUND_LCG_WRITE: u32 = 0x0047_be82;
const GLOBAL_LCG: u32 = 0x0120_741c;
static SOUND_LCG: AtomicU32 = AtomicU32::new(0x2545_f491);

/// Damage flash (the screen flashes red when the player is hurt). The camera object
/// (`[GameGlobal+0x0c]`) holds the flash amount (+0x5c, drawn by the render `FUN_006f21d0`)
/// and its fade rate (+0x60). The PlatformShooterPlayerSystem's damage handler
/// (`FUN_00bf6660`, every slot on every client) raises it: `call FUN_006f2f10`
/// (thiscall(camera, message)) @0x00bf66be, which draws the global LCG, on some paths only
/// while the current amount is under a limit. The world update fades it: `call FUN_006f1d40`
/// (ECX = camera, XMM1 = dt; also the screen shake) @0x006b32ae.
const FLASH_HIT_CALL: u32 = 0x00bf_66be;
const FLASH_HIT_FN: u32 = 0x006f_2f10;
const FLASH_DECAY_CALL: u32 = 0x006b_32ae;
const FLASH_DECAY_FN: u32 = 0x006f_1d40;
const CAM_FLASH: u32 = 0x5c;
const CAM_FLASH_RATE: u32 = 0x60;
/// Game mode (`[0x01204bc0]`); +0x90 set: the flash fades [`FLASH_FAST_K`] times faster.
const GAME_MODE_PTR: u32 = 0x0120_4bc0;
const GAME_MODE_FAST_FADE: u32 = 0x90;
const FLASH_FAST_K: u32 = 0x0115_2668;
const FLASH_ONE: u32 = 0x0105_3780;
const FLASH_EPS: u32 = 0x0105_3424;
/// Engine constants of the fade (expected, then read where the engine reads them).
const FLASH_CONSTANTS: [(u32, f32); 2] = [(FLASH_ONE, 1.0), (FLASH_EPS, 0.001)];
static FLASH_ON: AtomicU8 = AtomicU8::new(0);
/// This client's own flash amount and fade rate (f32 bits): its own player's hits only.
static LOCAL_FLASH: AtomicU32 = AtomicU32::new(0);
static LOCAL_FLASH_RATE: AtomicU32 = AtomicU32::new(0x3f80_0000);
/// The shared flash, parked while the render shows the local one.
static SHARED_FLASH: AtomicU32 = AtomicU32::new(0);
static SHARED_FLASH_RATE: AtomicU32 = AtomicU32::new(0);
static FLASH_PARKED: AtomicU8 = AtomicU8::new(0);
/// The decay call's dt, kept by its stub.
static FLASH_DT: AtomicU32 = AtomicU32::new(0);

const MAX_SLOTS: usize = 32;
#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU32 = AtomicU32::new(0);
/// Entity id per session slot (slot order); follows polymorphs.
static SLOT_IDS: [AtomicU32; MAX_SLOTS] = [ZERO; MAX_SLOTS];
static SLOT_COUNT: AtomicU32 = AtomicU32::new(0);
/// Index of this client's own slot (-1: none).
static LOCAL_SLOT: AtomicI32 = AtomicI32::new(-1);
/// Entity id of this client's input proxy (the engine's own player; 0 in solo mode).
static PROXY_ID: AtomicU32 = AtomicU32::new(0);
/// Set while the ControlsSystem runs the polymorph attack block for a disabled component.
static POLY_ATTACK_ACTIVE: AtomicU8 = AtomicU8::new(0);

/// Register the session's slot entities (slot order), this client's slot index and its input
/// proxy (`proxy_id` 0: none).
pub fn set_slots(ids: &[u32], local: i32, proxy_id: u32) {
    let n = ids.len().min(MAX_SLOTS);
    for (i, &id) in ids.iter().take(n).enumerate() {
        SLOT_IDS[i].store(id, Ordering::Relaxed);
    }
    SLOT_COUNT.store(n as u32, Ordering::Relaxed);
    LOCAL_SLOT.store(if (local as usize) < n { local } else { -1 }, Ordering::Relaxed);
    PROXY_ID.store(proxy_id, Ordering::Relaxed);
}

/// Current entity id of every slot (after polymorphs), into `out`. Returns the slot count.
pub fn slot_ids(out: &mut [u32]) -> usize {
    let n = (SLOT_COUNT.load(Ordering::Relaxed) as usize).min(out.len());
    for (i, o) in out.iter_mut().take(n).enumerate() {
        *o = SLOT_IDS[i].load(Ordering::Relaxed);
    }
    n
}

fn slot_of(id: u32) -> Option<usize> {
    if id == 0 {
        return None;
    }
    let n = SLOT_COUNT.load(Ordering::Relaxed) as usize;
    (0..n).find(|&i| SLOT_IDS[i].load(Ordering::Relaxed) == id)
}

/// Entity id of this client's own slot (0: none).
pub fn local_id() -> u32 {
    let i = LOCAL_SLOT.load(Ordering::Relaxed);
    if i < 0 {
        0
    } else {
        SLOT_IDS[i as usize].load(Ordering::Relaxed)
    }
}

unsafe fn id_of(e: u32) -> u32 {
    if e == 0 {
        0
    } else {
        rd(e)
    }
}

unsafe fn entity(id: u32) -> u32 {
    let mgr = rd(ENTITY_MANAGER);
    if id == 0 || mgr == 0 {
        return 0;
    }
    let f: extern "thiscall" fn(u32, u32) -> u32 = std::mem::transmute(ENTITY_BY_ID_FN as usize);
    let e = f(mgr, id);
    if e == 0 || rd(e + ENTITY_DEAD) as i32 > 0 {
        return 0;
    }
    e
}

/// Whether `e` is this client's own player.
pub(crate) unsafe fn is_local(e: u32) -> bool {
    let id = local_id();
    id != 0 && id_of(e) == id
}

/// Run `f` (a players-vector update of the game mode `gm` for `e`) with `e` standing in the
/// vector's first entry if it is not in the vector, as it would be in vanilla; the entry is
/// restored afterwards, so the vector keeps only the input proxy outside these calls.
unsafe fn as_vanilla_player(gm: u32, e: u32, f: impl FnOnce()) {
    let begin = rd(gm + PLAYERS_BEGIN);
    let n = rd(gm + PLAYERS_END).wrapping_sub(begin) / 4;
    if n == 0 || (0..n).any(|k| rd(begin + 4 * k) == e) {
        f();
        return;
    }
    let first = begin as *mut u32;
    let saved = *first;
    *first = e;
    f();
    *(rd(gm + PLAYERS_BEGIN) as *mut u32) = saved;
}

/// Polymorph start for a slot: vanilla's player setup of the creature, with the creature's
/// ControlsComponent left to the mod (`enabled` cleared again: the engine's input system
/// would otherwise write this client's keyboard into it). `polymorph_hax` stays set.
unsafe extern "thiscall" fn poly_start(gm: u32, old: u32, new: u32, cessation: u32) {
    let orig: unsafe extern "thiscall" fn(u32, u32, u32, u32) = std::mem::transmute(POLY_START_FN as usize);
    let Some(slot) = slot_of(id_of(old)) else {
        orig(gm, old, new, cessation);
        return;
    };
    as_vanilla_player(gm, old, || orig(gm, old, new, cessation));
    let mgr = rd(ENTITY_MANAGER);
    let controls_of: extern "thiscall" fn(u32, u32) -> u32 = std::mem::transmute(CONTROLS_OF_FN as usize);
    let c = if mgr == 0 { 0 } else { controls_of(mgr, new) };
    if c != 0 {
        *((c + CONTROLS_ENABLED) as *mut u8) = 0;
    }
    SLOT_IDS[slot].store(id_of(new), Ordering::Relaxed);
}

/// Polymorph end for a slot: vanilla's player swap back to the restored entity.
unsafe extern "thiscall" fn poly_end(gm: u32, creature: u32, restored: u32) {
    let orig: unsafe extern "thiscall" fn(u32, u32, u32) = std::mem::transmute(POLY_END_FN as usize);
    let Some(slot) = slot_of(id_of(creature)) else {
        orig(gm, creature, restored);
        return;
    };
    as_vanilla_player(gm, creature, || orig(gm, creature, restored));
    SLOT_IDS[slot].store(id_of(restored), Ordering::Relaxed);
}

/// The gamepad for presentation of `e`: this client's own slot uses the input proxy's (whose
/// ControlsComponent the engine drives), as vanilla's player uses its own.
unsafe extern "fastcall" fn gamepad_of_view(e: u32) -> u32 {
    let orig: unsafe extern "fastcall" fn(u32) -> u32 = std::mem::transmute(GAMEPAD_OF_FN as usize);
    if is_local(e) {
        let proxy = entity(PROXY_ID.load(Ordering::Relaxed));
        if proxy != 0 {
            return orig(proxy);
        }
    }
    orig(e)
}

/// A patched "is player" gate: the engine's answer, unchanged; remembers whether the subject
/// is this client's own player for the presentation calls in the gated block.
unsafe extern "fastcall" fn is_player_noting_subject(e: u32) -> u8 {
    let orig: unsafe extern "fastcall" fn(u32) -> u8 = std::mem::transmute(IS_PLAYER_FN as usize);
    SUBJECT_LOCAL.store(is_local(e) as u8, Ordering::Relaxed);
    orig(e)
}

/// The low-health indicator's DamageModelComponent lookup: our own player's instead of the
/// input proxy's (vanilla: its one player's).
unsafe extern "thiscall" fn damage_model_for_view(mgr: u32, e: u32) -> u32 {
    let orig: unsafe extern "thiscall" fn(u32, u32) -> u32 = std::mem::transmute(DAMAGE_MODEL_OF_FN as usize);
    let proxy = PROXY_ID.load(Ordering::Relaxed);
    if proxy != 0 && id_of(e) == proxy {
        let own = entity(local_id());
        if own != 0 {
            return orig(mgr, own);
        }
    }
    orig(mgr, e)
}

unsafe fn redirect_call(site: u32, orig: u32, new_target: u32) -> Result<(), PatchError> {
    let r = rel32(site + 5, orig);
    expect(site, &[0xE8, r[0], r[1], r[2], r[3]])?;
    let mut call = vec![0xE8];
    call.extend_from_slice(&rel32(site + 5, new_target));
    write_code(site, &call)
}

/// Code with rel32 fields fixed up after placement: `rels` = (offset of the rel32, target).
unsafe fn put_fixed(ex: &mut Exec, code: &[u8], rels: &[(usize, u32)]) -> Result<u32, PatchError> {
    let base = ex.put(code);
    for &(at, target) in rels {
        let end = base + at as u32 + 4;
        write_code(end - 4, &rel32(end, target))?;
    }
    Ok(base)
}

static POLYMORPH_INSTALLED: AtomicU8 = AtomicU8::new(0);

/// Install `polymorph_slots` alone (solo baseline, which runs without the determinism
/// patches but drives its player like a slot). Idempotent; no-op once installed.
///
/// # Safety
/// Game thread, noita.exe build 0x6794ee3c.
pub unsafe fn install_polymorph_only() -> Result<(), PatchError> {
    if POLYMORPH_INSTALLED.load(Ordering::Relaxed) != 0 {
        return Ok(());
    }
    let mut ex = Exec::new()?;
    patch_polymorph(&mut ex)
}

/// Patch `polymorph_slots` (module docs).
///
/// # Safety
/// Game thread, noita.exe build 0x6794ee3c, before the world exists.
pub(crate) unsafe fn patch_polymorph(ex: &mut Exec) -> Result<(), PatchError> {
    let jz = rel32(CONTROLS_DISABLED_JZ + 6, CONTROLS_RETURN);
    expect(CONTROLS_DISABLED_JZ, &[0x0F, 0x84, jz[0], jz[1], jz[2], jz[3]])?;
    let jmp = rel32(CONTROLS_DISABLED_JMP + 5, CONTROLS_RETURN);
    expect(CONTROLS_DISABLED_JMP, &[0xE9, jmp[0], jmp[1], jmp[2], jmp[3]])?;
    expect(CONTROLS_POLY_BLOCK, &[0x80, 0xBE, 0xB0, 0x01, 0x00, 0x00, 0x00])?;
    expect(CONTROLS_POLY_EXIT, &CONTROLS_POLY_EXIT_ORIG)?;
    let start = rel32(POLY_START_CALL + 5, POLY_START_FN);
    expect(POLY_START_CALL, &[0xE8, start[0], start[1], start[2], start[3]])?;
    let end = rel32(POLY_END_CALL + 5, POLY_END_FN);
    expect(POLY_END_CALL, &[0xE8, end[0], end[1], end[2], end[3]])?;

    let flag = (POLY_ATTACK_ACTIVE.as_ptr() as u32).to_le_bytes();
    // Disabled component: polymorph_hax set -> run the attack block with AL = Fire held.
    //   cmp byte [esi+0x1b0],0; je RETURN; mov byte [flag],1; mov al,[esi+0x48]; jmp BLOCK
    let hax = (CONTROLS_POLYMORPH_HAX as u16).to_le_bytes();
    let entry = put_fixed(
        ex,
        &[
            0x80, 0xBE, hax[0], hax[1], 0x00, 0x00, 0x00, //
            0x0F, 0x84, 0, 0, 0, 0, //
            0xC6, 0x05, flag[0], flag[1], flag[2], flag[3], 0x01, //
            0x8A, 0x46, CONTROLS_FIRE_DOWN, //
            0xE9, 0, 0, 0, 0,
        ],
        &[(9, CONTROLS_RETURN), (24, CONTROLS_POLY_BLOCK)],
    )?;
    // Block exit: from the disabled path -> clear the flag and the exception state, return;
    // else the original `cmp byte [esi+0x1c4],0; jz DESTRUCTORS`, then the confusion swap.
    let en = (CONTROLS_ENABLED as u16).to_le_bytes();
    let s = CONTROLS_EH_STATE;
    let exit = put_fixed(
        ex,
        &[
            0x80, 0x3D, flag[0], flag[1], flag[2], flag[3], 0x00, // cmp byte [flag],0
            0x75, 0x12, // jne ours
            0x80, 0xBE, en[0], en[1], 0x00, 0x00, 0x00, // cmp byte [esi+0x1c4],0
            0x0F, 0x84, 0, 0, 0, 0, // je DESTRUCTORS
            0xE9, 0, 0, 0, 0, // jmp CONFUSION
            // ours:
            0xC6, 0x05, flag[0], flag[1], flag[2], flag[3], 0x00, // mov byte [flag],0
            0xC7, 0x84, 0x24, s[0], s[1], s[2], s[3], 0xFF, 0xFF, 0xFF, 0xFF, // mov dword [esp+0x358],-1
            0xE9, 0, 0, 0, 0, // jmp RETURN
        ],
        &[(18, CONTROLS_DESTRUCTORS), (23, CONTROLS_CONFUSION), (46, CONTROLS_RETURN)],
    )?;

    let mut j = vec![0x0F, 0x84];
    j.extend_from_slice(&rel32(CONTROLS_DISABLED_JZ + 6, entry));
    write_code(CONTROLS_DISABLED_JZ, &j)?;
    let mut j = vec![0xE9];
    j.extend_from_slice(&rel32(CONTROLS_DISABLED_JMP + 5, entry));
    write_code(CONTROLS_DISABLED_JMP, &j)?;
    let mut j = vec![0xE9];
    j.extend_from_slice(&rel32(CONTROLS_POLY_EXIT + 5, exit));
    j.resize(CONTROLS_POLY_EXIT_ORIG.len(), 0x90);
    write_code(CONTROLS_POLY_EXIT, &j)?;

    let start_hook: unsafe extern "thiscall" fn(u32, u32, u32, u32) = poly_start;
    redirect_call(POLY_START_CALL, POLY_START_FN, start_hook as usize as u32)?;
    let end_hook: unsafe extern "thiscall" fn(u32, u32, u32) = poly_end;
    redirect_call(POLY_END_CALL, POLY_END_FN, end_hook as usize as u32)?;
    POLYMORPH_INSTALLED.store(1, Ordering::Relaxed);
    Ok(())
}

unsafe fn expect_call(site: u32, f: u32) -> Result<(), PatchError> {
    let r = rel32(site + 5, f);
    expect(site, &[0xE8, r[0], r[1], r[2], r[3]])
}

/// Patch `local_presentation` (module docs): the gamepad for our own player's presentation,
/// player-scoped presentation in "is player" gates only for our own player, and the
/// low-health indicator from our own player's HP. Needs `sound_rng` (the gated sounds).
///
/// # Safety
/// Game thread, noita.exe build 0x6794ee3c, before the world exists.
pub(crate) unsafe fn patch_presentation(ex: &mut Exec) -> Result<(), PatchError> {
    for &site in &GAMEPAD_VIEW_CALLS {
        expect_call(site, GAMEPAD_OF_FN)?;
    }
    for &(gate, calls) in GATED_PRESENTATION {
        expect_call(gate, IS_PLAYER_FN)?;
        for &(site, (f, _)) in calls {
            expect_call(site, f)?;
        }
    }
    expect_call(LOW_HP_DAMAGE_MODEL_CALL, DAMAGE_MODEL_OF_FN)?;

    let pad: unsafe extern "fastcall" fn(u32) -> u32 = gamepad_of_view;
    for &site in &GAMEPAD_VIEW_CALLS {
        redirect_call(site, GAMEPAD_OF_FN, pad as usize as u32)?;
    }
    let note: unsafe extern "fastcall" fn(u32) -> u8 = is_player_noting_subject;
    let flag = (SUBJECT_LOCAL.as_ptr() as u32).to_le_bytes();
    // One stub per callee: cmp byte [SUBJECT_LOCAL],0; jne callee; ret n (EAX unused by
    // these call sites).
    let mut stubs: Vec<(u32, u32)> = Vec::new();
    for &(gate, calls) in GATED_PRESENTATION {
        redirect_call(gate, IS_PLAYER_FN, note as usize as u32)?;
        for &(site, (f, ret)) in calls {
            let stub = match stubs.iter().find(|s| s.0 == f) {
                Some(s) => s.1,
                None => {
                    let mut code = vec![0x80, 0x3D, flag[0], flag[1], flag[2], flag[3], 0x00, 0x0F, 0x85, 0, 0, 0, 0];
                    if ret == 0 {
                        code.push(0xC3);
                    } else {
                        code.extend_from_slice(&[0xC2, ret as u8, (ret >> 8) as u8]);
                    }
                    let s = put_fixed(ex, &code, &[(9, f)])?;
                    stubs.push((f, s));
                    s
                }
            };
            redirect_call(site, f, stub)?;
        }
    }
    let dm: unsafe extern "thiscall" fn(u32, u32) -> u32 = damage_model_for_view;
    redirect_call(LOW_HP_DAMAGE_MODEL_CALL, DAMAGE_MODEL_OF_FN, dm as usize as u32)?;
    patch_flash(ex)
}

/// The damage flash: our own player's hits on a local copy (`flash_hit`), its decay alongside
/// the shared one (`flash_decay`), the copy shown by the render ([`flash_render_enter`]).
unsafe fn patch_flash(ex: &mut Exec) -> Result<(), PatchError> {
    expect_call(FLASH_HIT_CALL, FLASH_HIT_FN)?;
    expect_call(FLASH_DECAY_CALL, FLASH_DECAY_FN)?;
    for (addr, v) in FLASH_CONSTANTS {
        expect(addr, &v.to_le_bytes())?;
    }
    // At the setter call (ECX = camera, [esp] = message, EBP = the handler's frame with the
    // entity at [ebp+8]): push msg; push entity; push ecx; call flash_hit; add esp,12; ret 4
    let hit: unsafe extern "C" fn(u32, u32, u32) = flash_hit;
    let hit_stub = put_fixed(
        ex,
        &[
            0xFF, 0x74, 0x24, 0x04, // push dword [esp+4]
            0xFF, 0x75, 0x08, // push dword [ebp+8]
            0x51, // push ecx
            0xE8, 0, 0, 0, 0, // call flash_hit
            0x83, 0xC4, 0x0C, // add esp,12
            0xC2, 0x04, 0x00, // ret 4
        ],
        &[(9, hit as usize as u32)],
    )?;
    // At the decay call (ECX = camera, XMM1 = dt): keep dt, flash_decay(dt), then the
    // original with ECX and XMM1 as they were.
    let decay: unsafe extern "C" fn(u32) = flash_decay;
    let dt = (FLASH_DT.as_ptr() as u32).to_le_bytes();
    let decay_stub = put_fixed(
        ex,
        &[
            0xF3, 0x0F, 0x11, 0x0D, dt[0], dt[1], dt[2], dt[3], // movss [dt],xmm1
            0x60, // pushad
            0xFF, 0x35, dt[0], dt[1], dt[2], dt[3], // push dword [dt]
            0xE8, 0, 0, 0, 0, // call flash_decay
            0x83, 0xC4, 0x04, // add esp,4
            0x61, // popad
            0xF3, 0x0F, 0x10, 0x0D, dt[0], dt[1], dt[2], dt[3], // movss xmm1,[dt]
            0xE9, 0, 0, 0, 0, // jmp FUN_006f1d40
        ],
        &[(16, decay as usize as u32), (33, FLASH_DECAY_FN)],
    )?;
    redirect_call(FLASH_HIT_CALL, FLASH_HIT_FN, hit_stub)?;
    redirect_call(FLASH_DECAY_CALL, FLASH_DECAY_FN, decay_stub)?;
    FLASH_ON.store(1, Ordering::Relaxed);
    Ok(())
}

/// A player was hurt (PlatformShooterPlayerSystem's damage handler, every slot on every
/// client): the shared flash as before, which keeps the RNG draws identical everywhere; for
/// our own player also the local flash, with the same draw (the LCG is put back).
unsafe extern "C" fn flash_hit(cam: u32, e: u32, msg: u32) {
    let set: unsafe extern "thiscall" fn(u32, u32) = std::mem::transmute(FLASH_HIT_FN as usize);
    if is_local(e) {
        let lcg = rd(GLOBAL_LCG);
        let mut scratch = [0u32; (CAM_FLASH_RATE / 4 + 1) as usize];
        scratch[(CAM_FLASH / 4) as usize] = LOCAL_FLASH.load(Ordering::Relaxed);
        scratch[(CAM_FLASH_RATE / 4) as usize] = LOCAL_FLASH_RATE.load(Ordering::Relaxed);
        set(scratch.as_mut_ptr() as u32, msg);
        LOCAL_FLASH.store(scratch[(CAM_FLASH / 4) as usize], Ordering::Relaxed);
        LOCAL_FLASH_RATE.store(scratch[(CAM_FLASH_RATE / 4) as usize], Ordering::Relaxed);
        *(GLOBAL_LCG as *mut u32) = lcg;
    }
    set(cam, msg);
}

/// The world update's flash decay (`FUN_006f1d40`, @0x006f209a..0x006f20df), applied to the
/// local flash with the same frame time: `flash -= rate * dt * k` (k = 5 with the game mode's
/// fast-fade flag +0x90, else 1), not below 0; under 0.001 the rate goes back to 1.
unsafe extern "C" fn flash_decay(dt: u32) {
    let f = |a: u32| f32::from_bits(rd(a));
    let gm = rd(GAME_MODE_PTR);
    let k = if gm != 0 && *((gm + GAME_MODE_FAST_FADE) as *const u8) != 0 { f(FLASH_FAST_K) } else { f(FLASH_ONE) };
    let rate = f32::from_bits(LOCAL_FLASH_RATE.load(Ordering::Relaxed));
    let mut v = f32::from_bits(LOCAL_FLASH.load(Ordering::Relaxed)) - rate * f32::from_bits(dt) * k;
    if !(v > 0.0) {
        v = 0.0;
    }
    LOCAL_FLASH.store(v.to_bits(), Ordering::Relaxed);
    if f(FLASH_EPS) > v {
        LOCAL_FLASH_RATE.store(f(FLASH_ONE).to_bits(), Ordering::Relaxed);
    }
}

unsafe fn camera() -> u32 {
    let gg = rd(crate::patches::GAME_GLOBAL_PTR);
    if gg == 0 {
        0
    } else {
        rd(gg + 0x0c)
    }
}

/// After the window, before the render: the camera shows this client's own damage flash; the
/// shared one is parked until [`flash_render_leave`]. Game thread.
///
/// # Safety
/// World initialised.
pub unsafe fn flash_render_enter() {
    let cam = camera();
    if FLASH_ON.load(Ordering::Relaxed) == 0 || cam == 0 || FLASH_PARKED.load(Ordering::Relaxed) != 0 {
        return;
    }
    let p = cam as *mut u32;
    let (a, r) = ((CAM_FLASH / 4) as usize, (CAM_FLASH_RATE / 4) as usize);
    SHARED_FLASH.store(*p.add(a), Ordering::Relaxed);
    SHARED_FLASH_RATE.store(*p.add(r), Ordering::Relaxed);
    *p.add(a) = LOCAL_FLASH.load(Ordering::Relaxed);
    *p.add(r) = LOCAL_FLASH_RATE.load(Ordering::Relaxed);
    FLASH_PARKED.store(1, Ordering::Relaxed);
}

/// Entering the next window: the shared damage flash back on the camera. Game thread.
///
/// # Safety
/// World initialised.
pub unsafe fn flash_render_leave() {
    let cam = camera();
    if FLASH_PARKED.swap(0, Ordering::Relaxed) == 0 || cam == 0 {
        return;
    }
    let p = cam as *mut u32;
    *p.add((CAM_FLASH / 4) as usize) = SHARED_FLASH.load(Ordering::Relaxed);
    *p.add((CAM_FLASH_RATE / 4) as usize) = SHARED_FLASH_RATE.load(Ordering::Relaxed);
}

/// Patch `sound_rng`: sound events randomise pitch and volume with their own LCG instead of
/// the global one, so whether a sound plays (local-only presentation, or the audio state of
/// this client: event lookups, voice stealing) cannot change the simulation's RNG stream.
///
/// # Safety
/// Game thread, noita.exe build 0x6794ee3c, before the world exists.
pub(crate) unsafe fn patch_sound_rng(_ex: &mut Exec) -> Result<(), PatchError> {
    let lcg = GLOBAL_LCG.to_le_bytes();
    expect(SOUND_LCG_READ, &[0x69, 0x0D, lcg[0], lcg[1], lcg[2], lcg[3], 0xFD, 0x43, 0x03, 0x00])?;
    expect(SOUND_LCG_WRITE, &[0xA3, lcg[0], lcg[1], lcg[2], lcg[3]])?;
    let own = (SOUND_LCG.as_ptr() as u32).to_le_bytes();
    write_code(SOUND_LCG_READ + 2, &own)?;
    write_code(SOUND_LCG_WRITE + 1, &own)
}
