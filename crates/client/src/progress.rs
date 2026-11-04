//! Persistent progress: the parts of a player's save (`save00`) that Noita's simulation reads.
//!
//! Vanilla builds the world partly from the save: persistent flags (spell, perk and secret
//! unlocks: `spawn_requires_flag` in the spell tables, biome modifiers, the intro, pixel
//! scenes like the sun rocks, ...; Lua `HasFlagPersistent` and about a dozen native readers),
//! the orbs collected over all runs (`GameGetOrbCountAllTime`: fish in the holy mountains),
//! the all-time `death_count` (`StatsGlobalGetValue`: the starting wand's spell, the starting
//! potion, biome modifiers) and the bones wands that apparitions carry (`SpawnApparition`).
//! Two players with different saves generate different worlds, so a session plays with the
//! host's (its first player's) progress on every client: each client sends its own snapshot
//! in `Hello` ([`snapshot`]), the server puts the host's into run 0's `Start`, and every
//! client installs it before the world is generated ([`apply`]).
//!
//! What a player earns in the session still reaches that player's own save, as in vanilla:
//!
//! * The engine keeps the flags (and the orbs) as a `PersistentFlagManager`, a
//!   `std::unordered_set<std::string>` loaded from the save's directory, one file per flag.
//!   `Add` (`FUN_0083cfe0`) writes the file when the key is new to the set, `Remove`
//!   (`FUN_0083d170`) deletes it when the key is in the set. During a session the set holds
//!   the session's view (host snapshot plus what the session added); this module keeps the
//!   player's own set beside it, and every `Add`/`Remove` call goes through a hook that runs
//!   the engine's function against the own set's membership (so the file is written or
//!   deleted exactly when vanilla would for this player's save) and then sets the session
//!   view's membership. Changes made inside the simulation window (identical on every
//!   client) or before the run's first frame change the session view; those from this
//!   client's own code outside the window (its inventory GUI marking a spell as seen) only
//!   reach its own save. `AddFlagPersistent` returns whether the key is new to the session.
//! * Every new game reloads the flags from the save (WorldStateSystem's message handler
//!   `FUN_00c41a60`, after the mods' `init.lua`): a hook carries the session view over.
//! * `death_count` holds the host's value; the all-time stats are saved
//!   (`FUN_00b182f0`, at the game's exit) with this player's own count plus its own deaths
//!   in the session ([`own_death_count_delta`]).
//! * Bones: `SpawnApparition` lists the save's `persistent/bones_new/` (`FUN_007a3d10`) and
//!   loads a random file of it as the apparition's wand. In a session the list is the host's
//!   files, which `init.lua` installs as virtual files under [`BONES_DIR`]. Nothing is written
//!   to the save.

use crate::patches::{expect, rd, redirect_call, write_code, PatchError, GAME_GLOBAL_PTR};
use protocol::Progress;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// `PersistentFlagManager*` of `??S00/persistent/flags/` (recreated by `FUN_00c41a60`).
const FLAGS_MGR: u32 = 0x0120_73f4;
/// `PersistentFlagManager*` of `??S00/persistent/orbs_new/`, created on first use by
/// `FUN_00bc60f0` (cdecl, no arguments).
const ORBS_MGR: u32 = 0x0120_7404;
const ORBS_MGR_ENSURE_FN: u32 = 0x00bc_60f0;
/// `bool PersistentFlagManager::Add(const std::string&)` (thiscall, ret 4).
const MGR_ADD_FN: u32 = 0x0083_cfe0;
/// `void PersistentFlagManager::Remove(const std::string&)` (thiscall, ret 4).
const MGR_REMOVE_FN: u32 = 0x0083_d170;
/// Key check both start with (thiscall on the key string, returns bool in AL).
const KEY_VALID_FN: u32 = 0x0083_cc90;
/// While this byte is set, `Add` neither writes nor inserts.
const NO_PERSIST: u32 = 0x0120_4bbb;
/// `std::unordered_set<std::string>` (at the manager's +0) operations, all thiscall:
/// find(out iterator*, key*) ret 8; insert(out pair*, key*, u8) ret 0xc;
/// equal_range(out pair*, key*) ret 8; erase(out iterator*, first, last) ret 0xc.
const SET_FIND_FN: u32 = 0x008b_9b40;
const SET_INSERT_FN: u32 = 0x0092_f060;
const SET_INSERT_ARG: u32 = 0x0122_1d05;
const SET_EQUAL_RANGE_FN: u32 = 0x008b_9cf0;
const SET_ERASE_FN: u32 = 0x008b_9ab0;
/// Every call of `Add` / `Remove` (Lua `AddFlagPersistent` / `RemoveFlagPersistent`, the intro,
/// orb pickups, spell progress, the render init's crash marker, player death).
const ADD_CALLS: [u32; 9] =
    [0x0088_aadf, 0x006c_191f, 0x006c_1951, 0x006b_7ab0, 0x006c_0503, 0x00b8_c458, 0x007d_5787, 0x00b2_8c93, 0x00bc_676f];
const REMOVE_CALLS: [u32; 3] = [0x0088_ac32, 0x0088_b27b, 0x007d_5ad3];
/// The new game's flag reload: `FUN_0057a370` calls WorldStateSystem's handler (fastcall,
/// message in ECX), which recreates the flags manager from the save for message types 1 and 3.
const RELOAD_CALL: u32 = 0x0057_a404;
const RELOAD_FN: u32 = 0x00c4_1a60;
/// The orbs manager's reload (an orb system's message handler, thiscall with one argument:
/// deletes the manager and loads it again from the save), in that system's vtable.
const ORBS_RELOAD_SLOT: u32 = 0x0104_a838;
const ORBS_RELOAD_FN: u32 = 0x00bc_6670;
/// All-time stats save (`??STA/_stats.salakieli`, cdecl), called from the game mode's exit.
const STATS_SAVE_CALL: u32 = 0x006b_1bee;
const STATS_SAVE_FN: u32 = 0x00b1_82f0;
/// The all-time stats' `death_count` (`StatsGlobalGetValue`: 0x01208940 + 0x1b0, +8).
const DEATH_COUNT: u32 = 0x0120_8af8;
/// `std::vector<std::string> ListBones()` (fastcall, result in ECX, returned in EAX), called by
/// SpawnApparition (`FUN_007a4160`) and its spawn (`FUN_007a3df0`).
const BONES_LIST_CALLS: [u32; 2] = [0x007a_3e3c, 0x007a_4562];
const BONES_LIST_FN: u32 = 0x007a_3d10;
/// Platform singleton; vtable +0x24: `FileSystem* GetFileSystem()`.
const PLATFORM: u32 = 0x0122_1bc0;
/// `std::wstring* FileSystem::RealPath(std::wstring* out, int, const std::string* path)`
/// (thiscall, ret 0xc): a `??S00/...` path as a file path of this machine.
const FS_REAL_PATH_FN: u32 = 0x00db_d470;
/// `std::string::assign(const char*, size_t)` (thiscall, ret 8).
const STRING_ASSIGN_FN: u32 = 0x0041_dd60;
const IAT_NEW: u32 = 0x00f0_7500;
const IAT_DELETE: u32 = 0x00f0_7504;

/// The virtual directory (`ModTextFileSetContent`) holding the session's bones files.
pub const BONES_DIR: &str = "mods/noita_online/session_bones/";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Flags,
    Orbs,
}

struct Session {
    /// This player's own save: its flags and orbs (kept current with what it earns).
    own_flags: BTreeSet<String>,
    own_orbs: BTreeSet<String>,
    /// This player's own `death_count` at the session start, and its deaths since.
    own_deaths: u32,
    own_deaths_delta: u32,
    /// The host's bones files (name, contents), sorted by name.
    bones: Vec<(String, Vec<u8>)>,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
/// The session's progress is installed (hooks act); false outside a session.
static ACTIVE: AtomicBool = AtomicBool::new(false);

fn session() -> std::sync::MutexGuard<'static, Option<Session>> {
    SESSION.lock().unwrap_or_else(|p| p.into_inner())
}

/// An MSVC `std::string` the engine only reads (keys, paths). Long strings point at `heap`.
#[repr(C)]
struct KeyString {
    buf: [u8; 16],
    size: u32,
    cap: u32,
    heap: Vec<u8>,
}

impl KeyString {
    fn new(s: &str) -> Box<Self> {
        let mut k = Box::new(KeyString { buf: [0; 16], size: s.len() as u32, cap: 15, heap: Vec::new() });
        if s.len() < 16 {
            k.buf[..s.len()].copy_from_slice(s.as_bytes());
        } else {
            k.heap = Vec::with_capacity(s.len() + 1);
            k.heap.extend_from_slice(s.as_bytes());
            k.heap.push(0);
            k.buf[..4].copy_from_slice(&(k.heap.as_ptr() as u32).to_le_bytes());
            k.cap = s.len() as u32;
        }
        k
    }
    fn addr(&self) -> u32 {
        self as *const Self as u32
    }
}

/// Contents of an MSVC `std::string` at `at`.
unsafe fn read_string(at: u32) -> String {
    let size = rd(at + 0x10) as usize;
    let data = if rd(at + 0x14) >= 16 { rd(at) } else { at };
    String::from_utf8_lossy(std::slice::from_raw_parts(data as *const u8, size)).into_owned()
}

/// The keys of the manager `mgr`'s set (an MSVC `std::unordered_set<std::string>`: a list
/// whose head is at +0, node +0 next, +8 key, plus the bucket vector).
unsafe fn set_keys(mgr: u32) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if mgr == 0 {
        return out;
    }
    let head = rd(mgr);
    let mut n = rd(head);
    while n != head && n != 0 {
        out.insert(read_string(n + 8));
        n = rd(n);
    }
    out
}

unsafe fn set_has(mgr: u32, key: u32) -> bool {
    let find: extern "thiscall" fn(u32, *mut u32, u32) -> u32 = std::mem::transmute(SET_FIND_FN as usize);
    let mut it = 0u32;
    find(mgr, &mut it, key);
    it != rd(mgr)
}

unsafe fn set_insert(mgr: u32, key: u32) {
    let insert: extern "thiscall" fn(u32, *mut [u32; 2], u32, u32) -> u32 = std::mem::transmute(SET_INSERT_FN as usize);
    let mut out = [0u32; 2];
    insert(mgr, &mut out, key, *(SET_INSERT_ARG as *const u8) as u32);
}

unsafe fn set_erase(mgr: u32, key: u32) {
    let equal_range: extern "thiscall" fn(u32, *mut [u32; 2], u32) -> u32 = std::mem::transmute(SET_EQUAL_RANGE_FN as usize);
    let erase: extern "thiscall" fn(u32, *mut u32, u32, u32) -> u32 = std::mem::transmute(SET_ERASE_FN as usize);
    let mut range = [0u32; 2];
    equal_range(mgr, &mut range, key);
    let mut it = 0u32;
    erase(mgr, &mut it, range[0], range[1]);
}

/// Make the manager's set hold exactly `keys` (no files are touched).
unsafe fn set_assign(mgr: u32, keys: &BTreeSet<String>) {
    if mgr == 0 {
        return;
    }
    let have = set_keys(mgr);
    for k in have.difference(keys) {
        set_erase(mgr, KeyString::new(k).addr());
    }
    for k in keys.difference(&have) {
        set_insert(mgr, KeyString::new(k).addr());
    }
}

unsafe fn ensure_orbs_manager() -> u32 {
    if rd(ORBS_MGR) == 0 {
        let ensure: extern "C" fn() = std::mem::transmute(ORBS_MGR_ENSURE_FN as usize);
        ensure();
    }
    rd(ORBS_MGR)
}

/// Which session-managed set `mgr` is (None: inactive, or another manager).
unsafe fn kind_of(mgr: u32) -> Option<Kind> {
    if !ACTIVE.load(Ordering::Relaxed) || mgr == 0 {
        None
    } else if mgr == rd(FLAGS_MGR) {
        Some(Kind::Flags)
    } else if mgr == rd(ORBS_MGR) {
        Some(Kind::Orbs)
    } else {
        None
    }
}

/// Whether a change made now is the session's (every client makes it alike): inside the
/// simulation window, or before the run's first frame (world creation).
unsafe fn shared_change() -> bool {
    if crate::patches::in_window() {
        return true;
    }
    let gg = rd(GAME_GLOBAL_PTR);
    gg == 0 || rd(gg) == 0
}

/// The own set's membership of `name` (and update it to `now`).
fn own_member(kind: Kind, name: &str, now: Option<bool>) -> bool {
    let mut g = session();
    let Some(s) = g.as_mut() else { return false };
    let set = match kind {
        Kind::Flags => &mut s.own_flags,
        Kind::Orbs => &mut s.own_orbs,
    };
    let had = set.contains(name);
    match now {
        Some(true) => {
            set.insert(name.to_owned());
        }
        Some(false) => {
            set.remove(name);
        }
        None => {}
    }
    had
}

/// Give the key the membership `want` in the engine's set.
unsafe fn set_member(mgr: u32, key: u32, have: bool, want: bool) {
    if have && !want {
        set_erase(mgr, key);
    } else if want && !have {
        set_insert(mgr, key);
    }
}

unsafe extern "thiscall" fn add_hook(mgr: u32, key: u32) -> u32 {
    let orig: extern "thiscall" fn(u32, u32) -> u32 = std::mem::transmute(MGR_ADD_FN as usize);
    let valid: extern "thiscall" fn(u32) -> u32 = std::mem::transmute(KEY_VALID_FN as usize);
    let Some(kind) = kind_of(mgr) else { return orig(mgr, key) };
    if valid(key) & 0xff == 0 || *(NO_PERSIST as *const u8) != 0 {
        return orig(mgr, key);
    }
    let name = read_string(key);
    let in_session = set_has(mgr, key);
    let in_own = own_member(kind, &name, Some(true));
    // The engine's Add writes the file exactly when the key is new to the set: run it
    // against this player's own save's membership.
    set_member(mgr, key, in_session, in_own);
    orig(mgr, key);
    set_member(mgr, key, true, in_session || shared_change());
    (!in_session) as u32
}

unsafe extern "thiscall" fn remove_hook(mgr: u32, key: u32) {
    let orig: extern "thiscall" fn(u32, u32) = std::mem::transmute(MGR_REMOVE_FN as usize);
    let valid: extern "thiscall" fn(u32) -> u32 = std::mem::transmute(KEY_VALID_FN as usize);
    let Some(kind) = kind_of(mgr) else { return orig(mgr, key) };
    if valid(key) & 0xff == 0 {
        return orig(mgr, key);
    }
    let name = read_string(key);
    let in_session = set_has(mgr, key);
    let in_own = own_member(kind, &name, Some(false));
    // Remove deletes the file exactly when the key is in the set.
    set_member(mgr, key, in_session, in_own);
    orig(mgr, key);
    set_member(mgr, key, false, in_session && !shared_change());
}

unsafe extern "fastcall" fn reload_hook(message: u32) {
    let orig: extern "fastcall" fn(u32) = std::mem::transmute(RELOAD_FN as usize);
    if !ACTIVE.load(Ordering::Relaxed) {
        return orig(message);
    }
    let view = set_keys(rd(FLAGS_MGR));
    orig(message);
    set_assign(rd(FLAGS_MGR), &view);
}

unsafe extern "thiscall" fn orbs_reload_hook(this: u32, arg: u32) {
    let orig: extern "thiscall" fn(u32, u32) = std::mem::transmute(ORBS_RELOAD_FN as usize);
    if !ACTIVE.load(Ordering::Relaxed) {
        return orig(this, arg);
    }
    let view = set_keys(rd(ORBS_MGR));
    orig(this, arg);
    set_assign(rd(ORBS_MGR), &view);
}

extern "C" fn stats_save_hook() {
    // SAFETY: game thread, at the game mode's exit; the address is the stats' death_count.
    unsafe {
        let orig: extern "C" fn() = std::mem::transmute(STATS_SAVE_FN as usize);
        let own = session().as_ref().filter(|_| ACTIVE.load(Ordering::Relaxed)).map(|s| s.own_deaths.wrapping_add(s.own_deaths_delta));
        let Some(own) = own else { return orig() };
        let session_value = rd(DEATH_COUNT);
        *(DEATH_COUNT as *mut u32) = own;
        orig();
        *(DEATH_COUNT as *mut u32) = session_value;
    }
}

/// `ListBones()` in a session: the host's bones, as their virtual files.
unsafe extern "fastcall" fn bones_list_hook(out: *mut [u32; 3]) -> *mut [u32; 3] {
    let orig: extern "fastcall" fn(*mut [u32; 3]) -> *mut [u32; 3] = std::mem::transmute(BONES_LIST_FN as usize);
    let paths: Option<Vec<String>> = session()
        .as_ref()
        .filter(|_| ACTIVE.load(Ordering::Relaxed))
        .map(|s| s.bones.iter().map(|(n, _)| format!("{BONES_DIR}{n}")).collect());
    let Some(paths) = paths else { return orig(out) };
    *out = [0; 3];
    if paths.is_empty() {
        return out;
    }
    let new: extern "C" fn(usize) -> u32 = std::mem::transmute(rd(IAT_NEW) as usize);
    let assign: extern "thiscall" fn(u32, *const u8, usize) -> u32 = std::mem::transmute(STRING_ASSIGN_FN as usize);
    let buf = new(paths.len() * 0x18);
    for (i, p) in paths.iter().enumerate() {
        let s = buf + i as u32 * 0x18;
        std::ptr::write_bytes(s as *mut u8, 0, 0x18);
        *((s + 0x14) as *mut u32) = 15;
        assign(s, p.as_ptr(), p.len());
    }
    let end = buf + paths.len() as u32 * 0x18;
    *out = [buf, end, end];
    out
}

/// Install the hooks (they only act while a session's progress is installed).
///
/// # Safety
/// noita.exe build 0x6794ee3c, game thread, before the session.
pub unsafe fn install() -> Result<(), PatchError> {
    // The globals the hooks rely on: the flags and orbs managers (Lua AddFlagPersistent's and
    // an orb pickup's `mov ecx,[mgr]` before their Add call) and the all-time death count
    // (the game-over stats' `inc [death_count]`, FUN_00b18930).
    expect(0x007d_5780, &[0x8B, 0x0D, 0xF4, 0x73, 0x20, 0x01])?;
    expect(0x00bc_6768, &[0x8B, 0x0D, 0x04, 0x74, 0x20, 0x01])?;
    expect(0x00b1_9287, &[0xFF, 0x05, 0xF8, 0x8A, 0x20, 0x01])?;
    let add: unsafe extern "thiscall" fn(u32, u32) -> u32 = add_hook;
    let remove: unsafe extern "thiscall" fn(u32, u32) = remove_hook;
    let reload: unsafe extern "fastcall" fn(u32) = reload_hook;
    let stats_save: extern "C" fn() = stats_save_hook;
    let bones_list: unsafe extern "fastcall" fn(*mut [u32; 3]) -> *mut [u32; 3] = bones_list_hook;
    for site in ADD_CALLS {
        redirect_call(site, MGR_ADD_FN, add as usize as u32)?;
    }
    for site in REMOVE_CALLS {
        redirect_call(site, MGR_REMOVE_FN, remove as usize as u32)?;
    }
    redirect_call(RELOAD_CALL, RELOAD_FN, reload as usize as u32)?;
    redirect_call(STATS_SAVE_CALL, STATS_SAVE_FN, stats_save as usize as u32)?;
    for site in BONES_LIST_CALLS {
        redirect_call(site, BONES_LIST_FN, bones_list as usize as u32)?;
    }
    expect(ORBS_RELOAD_SLOT, &ORBS_RELOAD_FN.to_le_bytes())?;
    let orbs_reload: unsafe extern "thiscall" fn(u32, u32) = orbs_reload_hook;
    write_code(ORBS_RELOAD_SLOT, &(orbs_reload as usize as u32).to_le_bytes())?;
    Ok(())
}

/// The save's bones files (name, contents), sorted by name.
unsafe fn read_bones() -> Vec<(String, Vec<u8>)> {
    use std::os::windows::ffi::OsStringExt;
    let platform_vt = rd(PLATFORM);
    let get_fs: extern "thiscall" fn(u32) -> u32 = std::mem::transmute(rd(platform_vt + 0x24) as usize);
    let real_path: extern "thiscall" fn(u32, *mut [u32; 6], u32, u32) -> u32 = std::mem::transmute(FS_REAL_PATH_FN as usize);
    let dir = KeyString::new("??S00/persistent/bones_new/");
    let mut w = [0u32; 6];
    real_path(get_fs(PLATFORM), &mut w, 0, dir.addr());
    let (len, cap) = (w[4] as usize, w[5]);
    let data = if cap >= 8 { w[0] as *const u16 } else { w.as_ptr() as *const u16 };
    let path = std::ffi::OsString::from_wide(std::slice::from_raw_parts(data, len));
    if cap >= 8 {
        let delete: extern "C" fn(u32) = std::mem::transmute(rd(IAT_DELETE) as usize);
        delete(w[0]);
    }
    let mut bones: Vec<(String, Vec<u8>)> = std::fs::read_dir(&path)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| Some((e.file_name().into_string().ok()?, std::fs::read(e.path()).ok()?)))
        .collect();
    bones.sort();
    bones
}

/// This player's persistent progress, for `Hello`.
///
/// # Safety
/// Game thread, inside noita.exe build 0x6794ee3c.
pub unsafe fn snapshot() -> Progress {
    let orbs = ensure_orbs_manager();
    Progress {
        flags: set_keys(rd(FLAGS_MGR)).into_iter().collect(),
        orbs: set_keys(orbs).into_iter().collect(),
        death_count: rd(DEATH_COUNT),
        bones: read_bones(),
    }
}

/// Install the session's progress (`Start.progress` of run 0) over this player's own, before
/// the world is generated (the session start in `init.lua`). Later runs keep the session's
/// (the flag reload carries it over). Returns a summary for the log.
///
/// # Safety
/// Game thread, inside noita.exe build 0x6794ee3c, after [`install`].
pub unsafe fn apply(encoded: &[u8]) -> Result<String, String> {
    let mut g = session();
    if g.is_none() {
        let host = Progress::decode(encoded).map_err(|_| format!("malformed session progress ({} bytes)", encoded.len()))?;
        let flags_mgr = rd(FLAGS_MGR);
        let orbs_mgr = ensure_orbs_manager();
        if flags_mgr == 0 || orbs_mgr == 0 {
            return Err("persistent flag managers missing".into());
        }
        let mut bones = host.bones;
        bones.sort();
        let s = Session {
            own_flags: set_keys(flags_mgr),
            own_orbs: set_keys(orbs_mgr),
            own_deaths: rd(DEATH_COUNT),
            own_deaths_delta: 0,
            bones,
        };
        set_assign(flags_mgr, &host.flags.into_iter().collect());
        set_assign(orbs_mgr, &host.orbs.into_iter().collect());
        *(DEATH_COUNT as *mut u32) = host.death_count;
        *g = Some(s);
        ACTIVE.store(true, Ordering::Relaxed);
    }
    let s = g.as_ref().unwrap();
    Ok(format!(
        "session progress: flags={} (own {}) orbs={} (own {}) death_count={} (own {}+{}) bones={}",
        set_keys(rd(FLAGS_MGR)).len(),
        s.own_flags.len(),
        set_keys(rd(ORBS_MGR)).len(),
        s.own_orbs.len(),
        rd(DEATH_COUNT),
        s.own_deaths,
        s.own_deaths_delta,
        s.bones.len(),
    ))
}

/// The session's `i`-th bones file: its virtual path ([`BONES_DIR`]) and contents.
pub fn bone(i: usize) -> Option<(String, Vec<u8>)> {
    let g = session();
    let (name, data) = g.as_ref()?.bones.get(i)?;
    Some((format!("{BONES_DIR}{name}"), data.clone()))
}

/// The number of the session's bones files.
pub fn bone_count() -> usize {
    session().as_ref().map_or(0, |s| s.bones.len())
}

/// This client's own player died in the session and the game-over stats changed the all-time
/// `death_count` by `delta` (`respawn::player_died_stats`, which keeps the session's value):
/// counted for this player's own save.
pub fn own_death_count_delta(delta: u32) {
    if let Some(s) = session().as_mut() {
        s.own_deaths_delta = s.own_deaths_delta.wrapping_add(delta);
    }
}

/// Leaving the session: this player's own progress again.
///
/// # Safety
/// Game thread.
pub unsafe fn deactivate() {
    if !ACTIVE.swap(false, Ordering::Relaxed) {
        return;
    }
    let g = session();
    let Some(s) = g.as_ref() else { return };
    set_assign(rd(FLAGS_MGR), &s.own_flags);
    set_assign(rd(ORBS_MGR), &s.own_orbs);
    *(DEATH_COUNT as *mut u32) = s.own_deaths.wrapping_add(s.own_deaths_delta);
}
