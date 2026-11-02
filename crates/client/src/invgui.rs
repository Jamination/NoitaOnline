//! Noita's own inventory GUI (InventoryGuiComponent: HUD, quick bar, tooltips, the inventory
//! screen with wand/spell drag and drop, number keys, dropping items) in a lockstep session.
//!
//! The GUI system reads this client's keyboard and mouse directly and edits the inventory
//! at once, so on its own it would change the simulation on one client only. Here:
//! - The engine's own system call of the GUI update ([`GUI_UPDATE_FN`]) is skipped for every
//!   player (it runs inside the simulation window). The mod runs the GUI of its own player
//!   outside the window instead ([`gui_update`]), with the render camera and render RNG,
//!   like any other presentation.
//! - While that call runs, every engine function through which the GUI changes the
//!   simulation is intercepted: instead of acting, it appends an *op* (entity ids and
//!   values) to a buffer. The mod sends the ops with its input, and every client applies
//!   them in the tick ([`apply_ops`]) by calling the same engine functions with the same
//!   arguments, so the effect is the engine's own and identical everywhere.
//!
//! Intercepted functions (build 0x6794ee3c):
//! - `FUN_00b4ffa0` set active item (number keys, clicks on the quick bar);
//! - `FUN_00b5eb00` / `FUN_00b5eb90` take an item out of / put it into a container (moves and
//!   swaps of wands, items and spells between the quick bar, the inventory and wands);
//! - `FUN_00d0e670` send Message_ThrowItem (an item dragged out of the inventory is thrown);
//! - `FUN_00d20850` send Message_ItemAboutToBeEaten (an item eaten from the inventory);
//! - `FUN_00b905f0` pick up an item (the full-inventory "replace which item" screen);
//! - `FUN_00b4ed70` sort a container's children by slot (when the screen opens).
//!
//! The replace screen never pauses a session ([`OPEN_SWAP_FN`] is skipped everywhere); the
//! picker's own client runs it after the window ([`swap_update`]). The container capacity
//! queries the simulation shares with the GUI run with the GUI globals set to the
//! container owner's ([`wrap_capacity`]). Direct field writes of the GUI (item slots, spell
//! `uses_remaining` shown in tooltips, the inventory refresh request) are undone by the mod
//! around the call (`files/players.lua`).

use crate::patches::{expect, rd, rel32, write_code, Exec, PatchError};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// InventoryGuiSystem update of one component: thiscall(system, Entity*, InventoryGuiComponent*), `ret 8`.
const GUI_UPDATE_FN: u32 = 0x00b7_d8d0;
const GUI_UPDATE_PROLOGUE: [u8; 6] = [0x55, 0x8B, 0xEC, 0x83, 0xE4, 0xF8];
/// Set active item: ECX owner, EDX item (null = none), stack (flag, p4), caller cleans.
const SET_ACTIVE_FN: u32 = 0x00b4_ffa0;
const SET_ACTIVE_PROLOGUE: [u8; 5] = [0x55, 0x8B, 0xEC, 0x6A, 0xFF];
/// GUI: take item ECX out of its container (Message_MoveItem, RemoveFromParent), `ret`.
const TAKE_ITEM_FN: u32 = 0x00b5_eb00;
const TAKE_ITEM_PROLOGUE: [u8; 6] = [0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x08];
/// GUI: put item EDX into container ECX (unequips the dragged active item first, AddChild,
/// wand refresh, Message_MoveItem), `ret`.
const PUT_ITEM_FN: u32 = 0x00b5_eb90;
const PUT_ITEM_PROLOGUE: [u8; 5] = [0x55, 0x8B, 0xEC, 0x6A, 0xFF];
/// SendMessage<Message_ThrowItem>(Entity*, msg*), stdcall.
const SEND_THROW_FN: u32 = 0x00d0_e670;
/// SendMessage<Message_ItemAboutToBeEaten>(Entity*, msg*), stdcall.
const SEND_EAT_FN: u32 = 0x00d2_0850;
const SEND_PROLOGUE: [u8; 7] = [0x55, 0x8B, 0xEC, 0x53, 0x8B, 0x5D, 0x0C];
/// Message_ThrowItem constructor (fastcall ECX = 0x34-byte message).
const THROW_MSG_CTOR: u32 = 0x009d_f180;
const THROW_MSG_SIZE: usize = 0x34;
/// Message_ItemAboutToBeEaten vtable; the message is 0x1c bytes: +0x14 item, +0x18 eaten flag.
const EAT_MSG_VTABLE: u32 = 0x0104_2bd4;
/// IMessage destructor body (frees the message's string vector at +8), fastcall ECX.
const MSG_DTOR_FN: u32 = 0x009d_a2c0;
/// Entity::Kill (marks the entity and its children dead), fastcall ECX.
const KILL_FN: u32 = 0x0044_df60;
/// EntityManager*; GetEntityById: thiscall(manager, id), `ret 4`.
const ENTITY_MANAGER: u32 = 0x0120_4b98;
const ENTITY_BY_ID_FN: u32 = 0x0056_eba0;
/// ItemComponent type index (cdecl, no arguments); component buffers at manager+0x2c.
const ITEM_COMPONENT_TYPE_FN: u32 = 0x0095_8780;
/// ItemComponent::inventory_slot (ivec2).
const ITEM_SLOT: u32 = 0xd8;
/// GUI globals read by the take/put functions: the entity whose GUI runs, the dragged item's
/// entity id, and that entity's inventory_full container.
const GUI_OWNER: u32 = 0x0122_24f0;
const GUI_FULL: u32 = 0x0122_24f8;
const GUI_DRAG_ID: u32 = 0x0122_24fc;
const GUI_PENDING_EQUIP: u32 = 0x0122_2500;
/// "Inventory screen open" (byte), set by the GUI and read by the simulation (item pickup
/// refuses with the screen open; `GameIsInventoryOpen`).
const GUI_INVENTORY_OPEN: u32 = 0x0122_2510;
/// Co-op player list (`[0x01204bc0]+0x58..+0x5c`, Entity*): the HUD of a player is laid out
/// below those of the players listed before it (UI_COOP_* magic numbers, FUN_00b750b0); a
/// player not in the list counts every entry. The engine lists only its own spawned player
/// (our input proxy).
const PLAYER_LIST_OWNER: u32 = 0x0120_4bc0;
/// Full-inventory pickup: FUN_00b811b0 (ECX picker, EDX item, `ret`) opens the "replace which
/// item" screen by setting the picker/item globals and the GameGlobal pause bit 2; while that
/// bit is set DeathMatch::Update runs only FUN_00b813a0 (the screen: local input, then throws
/// the replaced item and picks up the new one, clears the bit).
const OPEN_SWAP_FN: u32 = 0x00b8_11b0;
const SWAP_UI_FN: u32 = 0x00b8_13a0;
const SWAP_PICKER: u32 = 0x0122_2520;
const SWAP_ITEM: u32 = 0x0122_2524;
const PAUSE_SWAP_BIT: u32 = 2;
/// Pick up item EDX for ECX, stack (out*, a, b), caller cleans.
const PICKUP_FN: u32 = 0x00b9_05f0;
/// Pause-state notifier (runs every mod's OnPausedChanged), fastcall ECX = pause flags.
const PAUSE_NOTIFY_FN: u32 = 0x006f_4d10;
const PUSH_EBP_SEH_PROLOGUE: [u8; 5] = [0x55, 0x8B, 0xEC, 0x6A, 0xFF];
const PAUSE_NOTIFY_PROLOGUE: [u8; 6] = [0x55, 0x8B, 0xEC, 0x83, 0xE4, 0xF8];
/// Entity: +0 id, +0xc dead (> 0).
const ENTITY_DEAD: u32 = 0xc;

const K_GUI: u32 = 0;
const K_EQUIP: u32 = 1;
const K_TAKE: u32 = 2;
const K_PUT: u32 = 3;
const K_THROW: u32 = 4;
const K_EAT: u32 = 5;
const K_PICKUP: u32 = 6;
const K_OPEN_SWAP: u32 = 7;
const K_SKIP: u32 = 8;
const K_SORT: u32 = 9;
/// Sort a child vector (`std::vector<Entity*>*`, cdecl) by inventory slot, recursing into
/// wands; the GUI runs it over the inventories when the screen opens.
const SORT_CHILDREN_FN: u32 = 0x00b4_ed70;
/// Entity: child vector pointer.
const CHILDREN: u32 = 0x90;

/// Op bytes (little endian), as sent in the input:
/// - `1` equip: item id u32 (0 = none), flag u8, p4 u32
/// - `2` take: item id u32
/// - `3` put: container id u32, item id u32, slot x i32, slot y i32, dragged item id u32,
///   container is the GUI's inventory_full u8
/// - `4` throw: item id u32, message bytes +0x14..+0x34 (from/to positions)
/// - `5` eat: item id u32
/// - `6` pick up: item id u32, slot x i32, slot y i32, a u8, b u8
fn op_len(kind: u8) -> Option<usize> {
    match kind as u32 {
        K_EQUIP => Some(9),
        K_TAKE => Some(4),
        K_PUT => Some(21),
        K_THROW => Some(4 + THROW_MSG_SIZE - 0x14),
        K_EAT => Some(4),
        K_PICKUP => Some(14),
        K_SORT => Some(4),
        _ => None,
    }
}

/// Address of the recording flag (a byte in the stub area): set only while the mod runs
/// the GUI itself.
static FLAG: AtomicU32 = AtomicU32::new(0);
static SYSTEM: AtomicU32 = AtomicU32::new(0);
/// (Entity*, InventoryGuiComponent*) of the local player, as last seen in the system update.
static LOCAL: Mutex<(u32, u32, u32)> = Mutex::new((0, 0, 0));
static OPS: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// The local player's pending "replace which item" screen: (picker Entity*, item Entity*).
static SWAP: Mutex<(u32, u32)> = Mutex::new((0, 0));
/// This client's "inventory screen open" flag, kept out of the simulation between GUI calls.
static INVENTORY_OPEN: AtomicU32 = AtomicU32::new(0);

/// Hook stub at `f`: `cmp byte [flag],0; j(e|ne) orig; <capture>; ret n; orig: <prologue>; jmp f+len`.
/// `skip_when_set`: the capture path runs while the flag is set (record hooks); otherwise
/// while it is clear (the GUI update hook).
unsafe fn hook(ex: &mut Exec, flag: u32, f: u32, prologue: &[u8], kind: u32, ret_n: u16, skip_when_set: bool) -> Result<(), PatchError> {
    expect(f, prologue)?;
    let cb = ((on_hook as extern "C" fn(u32, u32, u32, *const u32)) as u32).to_le_bytes();
    let fl = flag.to_le_bytes();
    let mut s = vec![0x80, 0x3D, fl[0], fl[1], fl[2], fl[3], 0x00];
    let jcc_at = s.len();
    s.extend_from_slice(&[0x0F, if skip_when_set { 0x84 } else { 0x85 }, 0, 0, 0, 0]);
    // mov eax,esp; push eax; push edx; push ecx; push kind; mov eax,cb; call eax; add esp,16
    s.extend_from_slice(&[0x89, 0xE0, 0x50, 0x52, 0x51, 0x6A, kind as u8, 0xB8, cb[0], cb[1], cb[2], cb[3], 0xFF, 0xD0, 0x83, 0xC4, 0x10]);
    if ret_n == 0 {
        s.push(0xC3);
    } else {
        s.extend_from_slice(&[0xC2, ret_n as u8, (ret_n >> 8) as u8]);
    }
    let orig = s.len();
    let rel = (orig - (jcc_at + 6)) as u32;
    s[jcc_at + 2..jcc_at + 6].copy_from_slice(&rel.to_le_bytes());
    s.extend_from_slice(prologue);
    s.extend_from_slice(&[0xE9, 0, 0, 0, 0]);
    let at = ex.put(&s);
    let jmp_end = at + s.len() as u32;
    let back = rel32(jmp_end, f + prologue.len() as u32);
    let n = s.len();
    std::ptr::copy_nonoverlapping(back.as_ptr(), (at as usize + n - 4) as *mut u8, 4);
    let mut jmp = vec![0xE9];
    jmp.extend_from_slice(&rel32(f + 5, at));
    jmp.resize(prologue.len(), 0x90);
    write_code(f, &jmp)
}

/// Patch entry (see the module docs).
///
/// # Safety
/// Game thread, noita.exe build 0x6794ee3c, before the world exists.
pub(crate) unsafe fn install(ex: &mut Exec) -> Result<(), PatchError> {
    let flag = ex.alloc_data(4);
    FLAG.store(flag, Ordering::Relaxed);
    let always = ex.alloc_data(4);
    *(always as *mut u8) = 1;
    hook(ex, flag, GUI_UPDATE_FN, &GUI_UPDATE_PROLOGUE, K_GUI, 8, false)?;
    hook(ex, flag, SET_ACTIVE_FN, &SET_ACTIVE_PROLOGUE, K_EQUIP, 0, true)?;
    hook(ex, flag, TAKE_ITEM_FN, &TAKE_ITEM_PROLOGUE, K_TAKE, 0, true)?;
    hook(ex, flag, PUT_ITEM_FN, &PUT_ITEM_PROLOGUE, K_PUT, 0, true)?;
    hook(ex, flag, SEND_THROW_FN, &SEND_PROLOGUE, K_THROW, 8, true)?;
    hook(ex, flag, SEND_EAT_FN, &SEND_PROLOGUE, K_EAT, 8, true)?;
    hook(ex, flag, PICKUP_FN, &PUSH_EBP_SEH_PROLOGUE, K_PICKUP, 0, true)?;
    // The replace screen never pauses a session; it opens for the picker's own client only.
    hook(ex, always, OPEN_SWAP_FN, &PUSH_EBP_SEH_PROLOGUE, K_OPEN_SWAP, 0, true)?;
    // Closing the screen inside our call would run the mods' OnPausedChanged (re-entering
    // Lua from an FFI call); nothing was paused.
    hook(ex, flag, PAUSE_NOTIFY_FN, &PAUSE_NOTIFY_PROLOGUE, K_SKIP, 0, true)?;
    hook(ex, flag, SORT_CHILDREN_FN, &GUI_UPDATE_PROLOGUE, K_SORT, 0, true)?;
    wrap_capacity(ex, CAPACITY_FN, &PUSH_EBP_SEH_PROLOGUE)?;
    wrap_capacity(ex, FREE_SLOTS_FN, &PUSH_EBP_SEH_PROLOGUE)?;
    Ok(())
}

/// Container capacity (`FUN_00b5d7d0`) and free slots (`FUN_00b5d940`), fastcall ECX =
/// container: for an inventory_quick/inventory_full container they use the GUI globals
/// (this client's own player's inventories and Inventory2Component), and the simulation calls
/// them too (picking up a spell checks the picker's free spell slots). So the globals are set
/// to the container owner's around every call.
const CAPACITY_FN: u32 = 0x00b5_d7d0;
const FREE_SLOTS_FN: u32 = 0x00b5_d940;
const GUI_QUICK: u32 = 0x0122_24f4;
const GUI_INVENTORY2: u32 = 0x0122_2504;
/// Inventory2Component of an entity (enabled), thiscall(manager, entity).
const INVENTORY2_FN: u32 = 0x0091_5bd0;
static CAPACITY_SAVED: Mutex<[u32; 3]> = Mutex::new([0; 3]);

unsafe fn wrap_capacity(ex: &mut Exec, f: u32, prologue: &[u8]) -> Result<(), PatchError> {
    expect(f, prologue)?;
    let prep = ((capacity_enter as extern "C" fn(u32)) as u32).to_le_bytes();
    let rest = ((capacity_leave as extern "C" fn()) as u32).to_le_bytes();
    // push ecx; push ecx; mov eax,prep; call eax; add esp,4; pop ecx; call tramp;
    // push eax; mov eax,rest; call eax; pop eax; ret; tramp: <prologue>; jmp f+len
    let mut s = vec![0x51, 0x51, 0xB8, prep[0], prep[1], prep[2], prep[3], 0xFF, 0xD0, 0x83, 0xC4, 0x04, 0x59];
    let call_at = s.len();
    s.extend_from_slice(&[0xE8, 0, 0, 0, 0]);
    s.extend_from_slice(&[0x50, 0xB8, rest[0], rest[1], rest[2], rest[3], 0xFF, 0xD0, 0x58, 0xC3]);
    let tramp = s.len();
    s[call_at + 1..call_at + 5].copy_from_slice(&((tramp - (call_at + 5)) as u32).to_le_bytes());
    s.extend_from_slice(prologue);
    s.extend_from_slice(&[0xE9, 0, 0, 0, 0]);
    let at = ex.put(&s);
    let n = s.len();
    let back = rel32(at + n as u32, f + prologue.len() as u32);
    std::ptr::copy_nonoverlapping(back.as_ptr(), (at as usize + n - 4) as *mut u8, 4);
    let mut jmp = vec![0xE9];
    jmp.extend_from_slice(&rel32(f + 5, at));
    jmp.resize(prologue.len(), 0x90);
    write_code(f, &jmp)
}

/// The entity's name (MSVC std::string at +0x14: data/inline buffer, size +0x24, capacity +0x28).
unsafe fn entity_name<'a>(e: u32) -> &'a [u8] {
    let len = rd(e + 0x24) as usize;
    let data = if rd(e + 0x28) > 15 { rd(e + 0x14) } else { e + 0x14 };
    std::slice::from_raw_parts(data as *const u8, len)
}

extern "C" fn capacity_enter(container: u32) {
    unsafe {
        *CAPACITY_SAVED.lock().unwrap_or_else(|p| p.into_inner()) = [rd(GUI_QUICK), rd(GUI_FULL), rd(GUI_INVENTORY2)];
        let owner = if container != 0 { rd(container + 0x94) } else { 0 };
        let mgr = rd(ENTITY_MANAGER);
        if owner == 0 || mgr == 0 {
            return;
        }
        let inv2: extern "thiscall" fn(u32, u32) -> u32 = std::mem::transmute(INVENTORY2_FN as usize);
        let inv = inv2(mgr, owner);
        if inv == 0 {
            return;
        }
        let (mut quick, mut full) = (0, 0);
        let v = rd(owner + CHILDREN);
        if v != 0 {
            let (mut p, end) = (rd(v), rd(v + 4));
            while p < end {
                let c = rd(p);
                match entity_name(c) {
                    b"inventory_quick" => quick = c,
                    b"inventory_full" => full = c,
                    _ => {}
                }
                p += 4;
            }
        }
        *(GUI_QUICK as *mut u32) = quick;
        *(GUI_FULL as *mut u32) = full;
        *(GUI_INVENTORY2 as *mut u32) = inv;
    }
}

extern "C" fn capacity_leave() {
    unsafe {
        let s = *CAPACITY_SAVED.lock().unwrap_or_else(|p| p.into_inner());
        *(GUI_QUICK as *mut u32) = s[0];
        *(GUI_FULL as *mut u32) = s[1];
        *(GUI_INVENTORY2 as *mut u32) = s[2];
    }
}

/// The entity in `e`'s tree (down to `depth` levels) whose child vector is `vec`.
unsafe fn find_container(e: u32, vec: u32, depth: u32) -> Option<u32> {
    if e == 0 {
        return None;
    }
    let v = rd(e + CHILDREN);
    if v == vec {
        return Some(e);
    }
    if v == 0 || depth == 0 {
        return None;
    }
    let (mut p, end) = (rd(v), rd(v + 4));
    while p < end {
        if let Some(found) = find_container(rd(p), vec, depth - 1) {
            return Some(found);
        }
        p += 4;
    }
    None
}

unsafe fn id_of(e: u32) -> u32 {
    if e == 0 { 0 } else { rd(e) }
}

unsafe fn entity(id: u32) -> u32 {
    if id == 0 {
        return 0;
    }
    let mgr = rd(ENTITY_MANAGER);
    if mgr == 0 {
        return 0;
    }
    let f: extern "thiscall" fn(u32, u32) -> u32 = std::mem::transmute(ENTITY_BY_ID_FN as usize);
    let e = f(mgr, id);
    if e == 0 || rd(e + ENTITY_DEAD) as i32 > 0 {
        return 0;
    }
    e
}

/// The ItemComponent of entity `e` (null if none).
unsafe fn item_component(e: u32) -> u32 {
    let mgr = rd(ENTITY_MANAGER);
    if e == 0 || mgr == 0 {
        return 0;
    }
    let tid: extern "C" fn() -> i32 = std::mem::transmute(ITEM_COMPONENT_TYPE_FN as usize);
    let t = tid();
    let bufs = rd(mgr + 0x2c);
    let n = (rd(mgr + 0x30) - bufs) as i32 >> 2;
    if t < 0 || t >= n {
        return 0;
    }
    let buf = rd(bufs + t as u32 * 4);
    if buf == 0 {
        return 0;
    }
    let vt = rd(buf);
    let index: extern "thiscall" fn(u32, u32) -> i32 = std::mem::transmute(rd(vt + 0x2c) as usize);
    let get: extern "thiscall" fn(u32, i32) -> u32 = std::mem::transmute(rd(vt + 0x1c) as usize);
    let i = index(buf, e);
    if i < 0 {
        return 0;
    }
    get(buf, i)
}

extern "C" fn on_hook(kind: u32, ecx: u32, edx: u32, sp: *const u32) {
    unsafe {
        let arg = |i: usize| *sp.add(i);
        if kind == K_GUI {
            // The system's own call (inside the window): skipped; remember where it is.
            SYSTEM.store(ecx, Ordering::Relaxed);
            let mut l = LOCAL.lock().unwrap_or_else(|p| p.into_inner());
            if l.0 != 0 && id_of(arg(1)) == l.0 {
                l.1 = arg(1);
                l.2 = arg(2);
            }
            return;
        }
        if kind == K_OPEN_SWAP {
            // Inside the window, on every client: no pause; only the picker's client shows
            // the screen (swap_update), and its choice comes back as ops.
            let l = LOCAL.lock().unwrap_or_else(|p| p.into_inner());
            if l.0 != 0 && id_of(ecx) == l.0 {
                *SWAP.lock().unwrap_or_else(|p| p.into_inner()) = (ecx, edx);
            }
            return;
        }
        let mut ops = OPS.lock().unwrap_or_else(|p| p.into_inner());
        let mut push = |b: &[u8]| ops.extend_from_slice(b);
        match kind {
            K_EQUIP => {
                push(&[K_EQUIP as u8]);
                push(&id_of(edx).to_le_bytes());
                push(&[arg(1) as u8]);
                push(&arg(2).to_le_bytes());
            }
            K_TAKE => {
                push(&[K_TAKE as u8]);
                push(&id_of(ecx).to_le_bytes());
            }
            K_PUT => {
                let ic = item_component(edx);
                let (x, y) = if ic != 0 { (rd(ic + ITEM_SLOT), rd(ic + ITEM_SLOT + 4)) } else { (0, 0) };
                push(&[K_PUT as u8]);
                push(&id_of(ecx).to_le_bytes());
                push(&id_of(edx).to_le_bytes());
                push(&x.to_le_bytes());
                push(&y.to_le_bytes());
                push(&rd(GUI_DRAG_ID).to_le_bytes());
                push(&[(ecx == rd(GUI_FULL)) as u8]);
            }
            K_THROW => {
                push(&[K_THROW as u8]);
                push(&id_of(arg(1)).to_le_bytes());
                push(std::slice::from_raw_parts((arg(2) + 0x14) as *const u8, THROW_MSG_SIZE - 0x14));
            }
            K_EAT => {
                // The GUI sends the message to the item, then (unless handled) to its owner:
                // one op, replayed as that whole sequence.
                let item = rd(arg(2) + 0x14);
                if arg(1) == item {
                    push(&[K_EAT as u8]);
                    push(&id_of(item).to_le_bytes());
                }
            }
            K_PICKUP => {
                let ic = item_component(edx);
                let (x, y) = if ic != 0 { (rd(ic + ITEM_SLOT), rd(ic + ITEM_SLOT + 4)) } else { (0, 0) };
                push(&[K_PICKUP as u8]);
                push(&id_of(edx).to_le_bytes());
                push(&x.to_le_bytes());
                push(&y.to_le_bytes());
                push(&[arg(2) as u8, arg(3) as u8]);
            }
            K_SORT => {
                // Sorting a container's children by slot: find the container (the player,
                // its inventories, their items) whose child vector this is.
                let vec = arg(1);
                let owner = LOCAL.lock().unwrap_or_else(|p| p.into_inner()).1;
                if let Some(e) = find_container(owner, vec, 3) {
                    push(&[K_SORT as u8]);
                    push(&id_of(e).to_le_bytes());
                }
            }
            _ => {}
        }
    }
}

/// The local player entity (id) whose GUI the mod runs.
pub fn set_local(id: u32) {
    *LOCAL.lock().unwrap_or_else(|p| p.into_inner()) = (id, 0, 0);
}

/// Run the local player's GUI once, recording its simulation changes as ops instead of
/// applying them. Returns false if the GUI component has not been seen yet.
///
/// # Safety
/// Game thread, outside the simulation window.
pub unsafe fn gui_update() -> bool {
    let (id, e, comp) = *LOCAL.lock().unwrap_or_else(|p| p.into_inner());
    let sys = SYSTEM.load(Ordering::Relaxed);
    let flag = FLAG.load(Ordering::Relaxed);
    if id == 0 || e == 0 || comp == 0 || sys == 0 || flag == 0 || entity(id) != e {
        return false;
    }
    // Our player is the only one for the layout (vanilla single-player HUD position).
    let list = rd(PLAYER_LIST_OWNER);
    let first = if list != 0 && rd(list + 0x58) < rd(list + 0x5c) { rd(list + 0x58) } else { 0 };
    let listed = if first != 0 { rd(first) } else { 0 };
    if first != 0 {
        *(first as *mut u32) = e;
    }
    *(GUI_INVENTORY_OPEN as *mut u8) = INVENTORY_OPEN.load(Ordering::Relaxed) as u8;
    *(flag as *mut u8) = 1;
    let f: extern "thiscall" fn(u32, u32, u32) = std::mem::transmute(GUI_UPDATE_FN as usize);
    f(sys, e, comp);
    *(flag as *mut u8) = 0;
    INVENTORY_OPEN.store(*(GUI_INVENTORY_OPEN as *const u8) as u32, Ordering::Relaxed);
    *(GUI_INVENTORY_OPEN as *mut u8) = 0;
    if first != 0 {
        *(first as *mut u32) = listed;
    }
    true
}

/// Run the local player's pending "replace which item" screen once (outside the window),
/// recording the replacement (throw the old item, pick up the new one) as ops. Returns true
/// while the screen stays open.
///
/// # Safety
/// Game thread, outside the simulation window.
pub unsafe fn swap_update() -> bool {
    let (picker, item) = *SWAP.lock().unwrap_or_else(|p| p.into_inner());
    let flag = FLAG.load(Ordering::Relaxed);
    let gg = rd(crate::patches::GAME_GLOBAL_PTR);
    let close = || *SWAP.lock().unwrap_or_else(|p| p.into_inner()) = (0, 0);
    if picker == 0 || flag == 0 || gg == 0 {
        return false;
    }
    if entity(id_of(picker)) != picker || entity(id_of(item)) != item {
        close();
        return false;
    }
    let pause = rd(gg + 0x48);
    let saved = [rd(SWAP_PICKER), rd(SWAP_ITEM), rd(pause)];
    *(SWAP_PICKER as *mut u32) = picker;
    *(SWAP_ITEM as *mut u32) = item;
    *(pause as *mut u32) |= PAUSE_SWAP_BIT;
    // The screen writes the new item's slot before picking it up; the pick-up op carries it.
    let ic = item_component(item);
    let slot = if ic != 0 { (rd(ic + ITEM_SLOT), rd(ic + ITEM_SLOT + 4)) } else { (0, 0) };
    *(flag as *mut u8) = 1;
    let f: extern "C" fn() = std::mem::transmute(SWAP_UI_FN as usize);
    f();
    *(flag as *mut u8) = 0;
    if ic != 0 {
        *((ic + ITEM_SLOT) as *mut u32) = slot.0;
        *((ic + ITEM_SLOT + 4) as *mut u32) = slot.1;
    }
    let open = rd(pause) & PAUSE_SWAP_BIT != 0;
    *(pause as *mut u32) = saved[2];
    *(SWAP_PICKER as *mut u32) = saved[0];
    *(SWAP_ITEM as *mut u32) = saved[1];
    if !open {
        close();
    }
    open
}

/// Ops recorded since the last call.
pub fn take_ops() -> Vec<u8> {
    std::mem::take(&mut *OPS.lock().unwrap_or_else(|p| p.into_inner()))
}

unsafe fn set_active(owner: u32, item: u32, flag: u32, p4: u32) {
    std::arch::asm!(
        "push {p4}",
        "push {flag}",
        "call {f}",
        "add esp, 8",
        f = in(reg) SET_ACTIVE_FN,
        p4 = in(reg) p4,
        flag = in(reg) flag,
        in("ecx") owner,
        in("edx") item,
        clobber_abi("C"),
    );
}

unsafe fn pick_up(owner: u32, item: u32, a: u32, b: u32) {
    let mut out = 0u32;
    let outp = &mut out as *mut u32 as u32;
    std::arch::asm!(
        "push {b}",
        "push {a}",
        "push {o}",
        "call {f}",
        "add esp, 12",
        f = in(reg) PICKUP_FN,
        b = in(reg) b,
        a = in(reg) a,
        o = in(reg) outp,
        in("ecx") owner,
        in("edx") item,
        clobber_abi("C"),
    );
}

/// Apply the ops of player entity `owner_id` (inside the window, every client, tick order).
/// Ops naming entities that no longer exist are skipped, identically everywhere. Returns
/// the number of ops applied, or -1 if the bytes are malformed.
///
/// # Safety
/// Game thread, inside the simulation window.
pub unsafe fn apply_ops(owner_id: u32, data: &[u8]) -> i32 {
    let owner = entity(owner_id);
    if owner == 0 {
        return 0;
    }
    let u32_at = |p: usize| u32::from_le_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
    let mut p = 0;
    let mut applied = 0;
    while p < data.len() {
        let kind = data[p];
        let Some(n) = op_len(kind) else { return -1 };
        if p + 1 + n > data.len() {
            return -1;
        }
        let a = p + 1;
        p = a + n;
        // The GUI globals belong to this client's own GUI; the engine functions read them
        // as "the player whose GUI acts", so they are set for the op and restored.
        let saved = [rd(GUI_OWNER), rd(GUI_FULL), rd(GUI_DRAG_ID), rd(GUI_PENDING_EQUIP)];
        *(GUI_OWNER as *mut u32) = owner;
        match kind as u32 {
            K_EQUIP => {
                let item_id = u32_at(a);
                let item = entity(item_id);
                if item_id == 0 || item != 0 {
                    set_active(owner, item, data[a + 4] as u32, u32_at(a + 5));
                    applied += 1;
                }
            }
            K_TAKE => {
                let item = entity(u32_at(a));
                if item != 0 {
                    let f: extern "fastcall" fn(u32) = std::mem::transmute(TAKE_ITEM_FN as usize);
                    f(item);
                    applied += 1;
                }
            }
            K_PUT => {
                let container = entity(u32_at(a));
                let item = entity(u32_at(a + 4));
                let ic = item_component(item);
                if container != 0 && item != 0 {
                    if ic != 0 {
                        *((ic + ITEM_SLOT) as *mut u32) = u32_at(a + 8);
                        *((ic + ITEM_SLOT + 4) as *mut u32) = u32_at(a + 12);
                    }
                    *(GUI_DRAG_ID as *mut u32) = u32_at(a + 16);
                    *(GUI_FULL as *mut u32) = if data[a + 20] != 0 { container } else { 0 };
                    let f: extern "fastcall" fn(u32, u32) = std::mem::transmute(PUT_ITEM_FN as usize);
                    f(container, item);
                    applied += 1;
                }
            }
            K_THROW => {
                let item = entity(u32_at(a));
                if item != 0 {
                    let mut msg = [0u32; THROW_MSG_SIZE / 4];
                    let m = msg.as_mut_ptr() as u32;
                    let ctor: extern "fastcall" fn(u32) -> u32 = std::mem::transmute(THROW_MSG_CTOR as usize);
                    ctor(m);
                    std::ptr::copy_nonoverlapping(data[a + 4..].as_ptr(), (m + 0x14) as *mut u8, THROW_MSG_SIZE - 0x14);
                    let send: extern "stdcall" fn(u32, u32) = std::mem::transmute(SEND_THROW_FN as usize);
                    send(item, m);
                    let dtor: extern "fastcall" fn(u32) = std::mem::transmute(MSG_DTOR_FN as usize);
                    dtor(m);
                    applied += 1;
                }
            }
            K_EAT => {
                let item = entity(u32_at(a));
                if item != 0 {
                    let mut msg = [0u32; 7];
                    msg[0] = EAT_MSG_VTABLE;
                    msg[5] = item;
                    let m = msg.as_mut_ptr() as u32;
                    let send: extern "stdcall" fn(u32, u32) = std::mem::transmute(SEND_EAT_FN as usize);
                    send(item, m);
                    if *((m + 0x18) as *const u8) == 0 {
                        send(owner, m);
                    }
                    if *((m + 0x18) as *const u8) != 0 {
                        let kill: extern "fastcall" fn(u32) = std::mem::transmute(KILL_FN as usize);
                        kill(item);
                    }
                    let dtor: extern "fastcall" fn(u32) = std::mem::transmute(MSG_DTOR_FN as usize);
                    dtor(m);
                    applied += 1;
                }
            }
            K_PICKUP => {
                let item = entity(u32_at(a));
                // Only an item lying in the world: the screen picks up what the player stands at.
                if item != 0 && rd(item + 0x94) == 0 {
                    let ic = item_component(item);
                    if ic != 0 {
                        *((ic + ITEM_SLOT) as *mut u32) = u32_at(a + 4);
                        *((ic + ITEM_SLOT + 4) as *mut u32) = u32_at(a + 8);
                    }
                    pick_up(owner, item, data[a + 12] as u32, data[a + 13] as u32);
                    applied += 1;
                }
            }
            K_SORT => {
                let e = entity(u32_at(a));
                if e != 0 && rd(e + CHILDREN) != 0 {
                    let f: extern "C" fn(u32) = std::mem::transmute(SORT_CHILDREN_FN as usize);
                    f(rd(e + CHILDREN));
                    applied += 1;
                }
            }
            _ => unreachable!(),
        }
        *(GUI_OWNER as *mut u32) = saved[0];
        *(GUI_FULL as *mut u32) = saved[1];
        *(GUI_DRAG_ID as *mut u32) = saved[2];
        *(GUI_PENDING_EQUIP as *mut u32) = saved[3];
    }
    applied
}
