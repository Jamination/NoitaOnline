//! In-window readers of the engine camera, for every player's view.
//!
//! Inside the simulation window the engine camera (GameGlobal+0xc: float rect x, y, w, h)
//! is the simulation camera, i.e. the first live player. Vanilla systems that ask "how far
//! is this from the camera" or "is this on screen" therefore answered for player 1 only:
//! other players' sprite events (throws, kicks, AI jump triggers) never fired, their
//! enemies never started hunting, their verlet ropes stopped and vanished, their torches and
//! sprite emitters went out, their fog of war never cleared. The patches here answer as if
//! each player's own camera were the camera:
//! - distance-to-camera tests use the nearest view centre ([`views::nearest`]);
//! - "near the camera rect" / "inside the camera rect" tests pass if any view passes;
//! - per-view rects are the engine camera rect (its w, h) moved to the view centre, so the
//!   vanilla margins and distances keep their meaning; the one pure "on screen" test (pixel
//!   scenes popping in) uses [`views::rect`], the view's int rect;
//! - the fog of war runs its 17×17-cell update around every view, each fog pixel once.
//!
//! Every hook falls back to the vanilla code path outside the window or without views
//! ([`views::count`] == 0). Results depend only on simulation state and the views.
//!
//! Most hooks replace the GameGlobal getter (`call FUN_00439bb0`) in front of a camera read
//! with a stub returning a stand-in GameGlobal whose +0xc points at the chosen view's rect;
//! the original arithmetic then runs unchanged. All stubs share one shape ([`ctx_stub`]):
//! save every general and XMM register, call [`on_hook`] with the saved block (which it may
//! edit), restore, run a site-specific tail.
//!
//! Screen shake is split in two: the shared shake (world+0x58, decays in the world update
//! with two global-LCG draws per frame while above 0.01) becomes the strongest any player
//! would feel, and each client keeps its own shake for its own view, applied only to its
//! render camera ([`shake_render_enter`] / [`shake_render_leave`]).

use crate::patches::{expect, in_window, rd, rel32, write_code, Exec, PatchError, GAME_GLOBAL_PTR};
use crate::views;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

/// GameGlobal getter (cdecl, returns GameGlobal*, creating it on first use).
const GG_GETTER: u32 = 0x0043_9bb0;

// --- saved-register block --------------------------------------------------------------------

/// Indices of the pushad block (EDI lowest) after the 8 saved XMM registers.
const EDI: usize = 0;
const ESI: usize = 1;
const EBP: usize = 2;
const ESP: usize = 3;
const EBX: usize = 4;
const EDX: usize = 5;
const ECX: usize = 6;
const EAX: usize = 7;

/// The block [`ctx_stub`] hands to [`on_hook`]: XMM0..7 (16 bytes each), then pushad.
struct Ctx(*mut u32);

impl Ctx {
    unsafe fn xmm(&self, i: usize) -> f32 {
        f32::from_bits(*self.0.add(i * 4))
    }
    unsafe fn set_xmm(&self, i: usize, v: f32) {
        *self.0.add(i * 4) = v.to_bits();
    }
    unsafe fn reg(&self, r: usize) -> u32 {
        *self.0.add(32 + r)
    }
    unsafe fn set_reg(&self, r: usize, v: u32) {
        *self.0.add(32 + r) = v;
    }
    /// ESP at the patched instruction, for stubs entered by `call` from it.
    unsafe fn site_esp(&self) -> u32 {
        self.reg(ESP) + 4
    }
}

/// `pushad; sub esp,0x80; movups [esp+16i],xmm_i; push esp; push mode; call on_hook;
/// add esp,8; movups xmm_i,[esp+16i]; add esp,0x80; popad; <tail>`.
unsafe fn ctx_stub(ex: &mut Exec, mode: u32, tail: &[u8]) -> u32 {
    let cb = (on_hook as extern "C" fn(u32, *mut u32)) as u32;
    let mut s = vec![0x60, 0x81, 0xEC, 0x80, 0x00, 0x00, 0x00];
    for i in 0..8u8 {
        s.extend_from_slice(&[0x0F, 0x11, 0x44 | (i << 3), 0x24, i * 16]);
    }
    s.extend_from_slice(&[0x8B, 0xC4, 0x50, 0x68]);
    s.extend_from_slice(&mode.to_le_bytes());
    s.push(0xB8);
    s.extend_from_slice(&cb.to_le_bytes());
    s.extend_from_slice(&[0xFF, 0xD0, 0x83, 0xC4, 0x08]);
    for i in 0..8u8 {
        s.extend_from_slice(&[0x0F, 0x10, 0x44 | (i << 3), 0x24, i * 16]);
    }
    s.extend_from_slice(&[0x81, 0xC4, 0x80, 0x00, 0x00, 0x00, 0x61]);
    s.extend_from_slice(tail);
    ex.put(&s)
}

/// `call <to>` assembled at `site`, padded with NOPs to `len` bytes.
fn call_at(site: u32, to: u32, len: usize) -> Vec<u8> {
    let mut v = vec![0xE8];
    v.extend_from_slice(&rel32(site + 5, to));
    v.resize(len, 0x90);
    v
}

/// The bytes of `call <to>` at `site`.
fn call_bytes(site: u32, to: u32) -> [u8; 5] {
    let r = rel32(site + 5, to);
    [0xE8, r[0], r[1], r[2], r[3]]
}

/// Replace `call FUN_00439bb0` at `site` by a [`ctx_stub`] for `mode` (tail `ret`).
unsafe fn hook_getter(ex: &mut Exec, site: u32, mode: u32) -> Result<(), PatchError> {
    expect(site, &call_bytes(site, GG_GETTER))?;
    let stub = ctx_stub(ex, mode, &[0xC3]);
    write_code(site, &call_at(site, stub, 5))
}

/// `push <to>; ret`: a jump to an absolute address from anywhere in the stub page.
fn jump_abs(to: u32) -> Vec<u8> {
    let mut v = vec![0x68];
    v.extend_from_slice(&to.to_le_bytes());
    v.push(0xC3);
    v
}

/// `cmp dword [flag],0`.
fn cmp_flag(flag: &AtomicU32) -> Vec<u8> {
    let mut v = vec![0x83, 0x3D];
    v.extend_from_slice(&(flag.as_ptr() as u32).to_le_bytes());
    v.push(0x00);
    v
}

// --- views ------------------------------------------------------------------------------------

/// Whether the per-player semantics apply (inside the window, players published).
fn active() -> bool {
    in_window() && views::count() > 0
}

unsafe fn f32_at(a: u32) -> f32 {
    *(a as *const f32)
}

unsafe fn set_f32_at(a: u32, v: f32) {
    *(a as *mut f32) = v;
}

/// Entity position (Entity+0x70, +0x74).
unsafe fn entity_pos(entity: u32) -> (f32, f32) {
    (f32_at(entity + 0x70), f32_at(entity + 0x74))
}

/// The engine camera rect {x, y, w, h} (GameGlobal+0xc), if the world exists.
unsafe fn camera_rect() -> Option<[f32; 4]> {
    let gg = rd(GAME_GLOBAL_PTR);
    if gg == 0 {
        return None;
    }
    let cam = rd(gg + 0x0c);
    if cam == 0 {
        return None;
    }
    let r = cam as *const f32;
    Some([*r, *r.add(1), *r.add(2), *r.add(3)])
}

/// The engine camera rect moved so that its centre is view `i`'s centre.
fn view_camera_rect(i: usize, cam: [f32; 4]) -> [f32; 4] {
    let (cx, cy) = views::centre(i);
    [cx - cam[2] * 0.5, cy - cam[3] * 0.5, cam[2], cam[3]]
}

/// The engine camera centre in vanilla's operation order (`w * 0.5 + x`).
fn camera_centre(cam: [f32; 4]) -> (f32, f32) {
    (cam[2] * 0.5 + cam[0], cam[3] * 0.5 + cam[1])
}

/// GameGlobal, as the getter returns it.
unsafe fn real_gg() -> u32 {
    let gg = rd(GAME_GLOBAL_PTR);
    if gg != 0 {
        return gg;
    }
    let get: extern "C" fn() -> u32 = std::mem::transmute(GG_GETTER as usize);
    get()
}

/// A stand-in GameGlobal: only +0xc (the camera object, whose rect is at +0) is meaningful,
/// which is all the hooked sites read from it before they fetch GameGlobal again.
#[repr(C)]
struct FakeGg {
    gg: [Cell<u32>; 4],
    rect: [Cell<f32>; 4],
}

/// A stand-in Entity for the fog-of-war player lookup: only the position (+0x70, +0x74) is
/// read.
#[repr(C)]
struct FakeEntity {
    words: [Cell<u32>; 30],
}

thread_local! {
    static FAKE_GG: FakeGg = const {
        FakeGg { gg: [const { Cell::new(0) }; 4], rect: [const { Cell::new(0.0) }; 4] }
    };
    static FAKE_ENTITY: FakeEntity = const { FakeEntity { words: [const { Cell::new(0) }; 30] } };
}

fn fake_gg(rect: [f32; 4]) -> u32 {
    FAKE_GG.with(|f| {
        for (slot, v) in f.rect.iter().zip(rect) {
            slot.set(v);
        }
        f.gg[3].set(f.rect.as_ptr() as usize as u32);
        f as *const FakeGg as usize as u32
    })
}

fn fake_entity(x: f32, y: f32) -> u32 {
    FAKE_ENTITY.with(|e| {
        e.words[0x70 / 4].set(x.to_bits());
        e.words[0x74 / 4].set(y.to_bits());
        e as *const FakeEntity as usize as u32
    })
}

/// GameGlobal for a camera-centre distance test from (x, y): a stand-in whose camera is
/// centred on the nearest view; the real one when inactive.
unsafe fn gg_nearest(x: f32, y: f32) -> u32 {
    if !active() {
        return real_gg();
    }
    match (camera_rect(), views::nearest(x, y)) {
        (Some(cam), Some((i, _))) => fake_gg(view_camera_rect(i, cam)),
        _ => real_gg(),
    }
}

// --- hook dispatch ----------------------------------------------------------------------------

const M_SPRITE_EVENT: u32 = 1;
const M_SPRITE_FINISHED: u32 = 2;
const M_HUNT: u32 = 3;
const M_HELP: u32 = 4;
const M_PF_JUMP: u32 = 5;
const M_CHARACTER: u32 = 6;
const M_TELEPATHY: u32 = 7;
const M_PIXEL_SCENE: u32 = 8;
const M_WORM: u32 = 9;
const M_DRAGON: u32 = 10;
const M_LASER: u32 = 11;
const M_SHAKE: u32 = 12;
const M_EDGE: u32 = 13;
const M_VERLET: u32 = 14;
const M_EMITTER: u32 = 15;
const M_FOG1_START: u32 = 16;
const M_FOG1_END: u32 = 17;
const M_FOG2_PLAYER: u32 = 18;
const M_FOG2_END: u32 = 19;

extern "C" fn on_hook(mode: u32, ctx: *mut u32) {
    let c = Ctx(ctx);
    unsafe {
        match mode {
            M_SPRITE_EVENT => {
                let (x, y) = entity_pos(rd(c.site_esp() + 0x1c));
                c.set_reg(EAX, gg_nearest(x, y));
            }
            M_SPRITE_FINISHED => {
                let (x, y) = entity_pos(rd(c.site_esp() + 0x10));
                c.set_reg(EAX, gg_nearest(x, y));
            }
            M_HUNT | M_HELP => {
                let (x, y) = entity_pos(c.reg(EDI));
                c.set_reg(EAX, gg_nearest(x, y));
            }
            M_PF_JUMP => {
                let s = c.site_esp();
                c.set_reg(EAX, gg_nearest(f32_at(s + 0x24), f32_at(s + 0x28)));
            }
            M_CHARACTER => {
                let s = c.site_esp();
                c.set_reg(EAX, gg_nearest(f32_at(s + 0x54), f32_at(s + 0x58)));
            }
            M_TELEPATHY => {
                let (x, y) = entity_pos(c.reg(EBX));
                c.set_reg(EAX, gg_nearest(x, y));
            }
            M_PIXEL_SCENE => pixel_scene(&c),
            M_WORM => {
                let s = c.site_esp();
                c.set_reg(EAX, gg_nearest(f32_at(s + 0x40), f32_at(s + 0x60)));
            }
            M_DRAGON => {
                let s = c.site_esp();
                c.set_reg(EAX, gg_nearest(f32_at(s + 0x7c), f32_at(s + 0x50)));
            }
            M_LASER => laser(&c),
            M_SHAKE => shake(&c),
            M_EDGE => edge(&c),
            M_VERLET => verlet(&c),
            M_EMITTER => emitter(&c),
            M_FOG1_START => fog1_start(&c),
            M_FOG1_END => fog_end(&c, &FOG1, LOOP1_IMAGE_SLOT),
            M_FOG2_PLAYER => fog2_player(&c),
            M_FOG2_END => fog_end(&c, &FOG2, 0),
            _ => {}
        }
    }
}

// --- sprite animation events ------------------------------------------------------------------

/// SpriteSystem per-component update (FUN_00c1ab60, inside the window): when the animation
/// frame changes it sends Message_NamedEvent for every `<Event on_finished="0">` of the new
/// frame whose `max_distance` (default 500 in the data) reaches the camera centre:
/// `call FUN_00439bb0; mov esi,[edi+0x6c]; mov edi,[edi+0x70]; mov eax,[eax+0xc]` ..
/// `mov eax,[esp+0x1c]` (the entity) `subss xmm2,[eax+0x70]; subss xmm1,[eax+0x74]`.
/// The receivers are gameplay: PlatformShooterPlayerSystem (`throw_release` throws the held
/// item, `sprite_invisible`/`sprite_visible`), AnimalAISystem (`jump_start` in the jump
/// state), AudioComponent (sounds).
const SPRITE_EVENT_CALL: u32 = 0x00c1_ae49;
/// SpriteAnimatorSystem (FUN_00c1b820): the same for `on_finished="1"` events when an
/// animation ends; entity at [esp+0x10].
const SPRITE_FINISHED_CALL: u32 = 0x00c1_b96d;

/// Sprite animation events fire within `max_distance` of the nearest player's view centre.
pub(crate) unsafe fn patch_sprite_events(ex: &mut Exec) -> Result<(), PatchError> {
    hook_getter(ex, SPRITE_EVENT_CALL, M_SPRITE_EVENT)?;
    hook_getter(ex, SPRITE_FINISHED_CALL, M_SPRITE_FINISHED)
}

// --- animal AI --------------------------------------------------------------------------------

/// AnimalAI JobDefault (FUN_00a25940): until the animal has once been closer than
/// `max_distance_to_cam_to_start_hunting` (AnimalAIComponent+0x90) to the camera centre
/// (flag +0x336), it idles. `call FUN_00439bb0; mov eax,[eax+0xc]` .. `subss xmm2,[edi+0x70]`
/// (EDI = entity).
const HUNT_CALL: u32 = 0x00a2_5990;
/// AnimalAI JobHelpOtherEntity (FUN_00a25f00): the path target (PathFinding+0xfc/+0x100) is
/// the camera centre. EDI = entity.
const HELP_CALL: u32 = 0x00a2_5f41;

/// Animals start hunting near any player; helpers head for the nearest player's view.
pub(crate) unsafe fn patch_animal_ai(ex: &mut Exec) -> Result<(), PatchError> {
    hook_getter(ex, HUNT_CALL, M_HUNT)?;
    hook_getter(ex, HELP_CALL, M_HELP)
}

// --- pathfinding jumps ------------------------------------------------------------------------

/// PathFindingSystem (FUN_00bca030): when a search starts, `can_jump` only counts within
/// `max_jump_distance_from_camera` (component+0x7c) of the camera centre.
/// `call FUN_00439bb0` .. `mov ecx,[eax+0xc]`, entity position at [esp+0x24]/[esp+0x28].
const PF_JUMP_CALL: u32 = 0x00bc_a385;

/// Pathfinding may plan jumps near any player.
pub(crate) unsafe fn patch_pathfinding_jump(ex: &mut Exec) -> Result<(), PatchError> {
    hook_getter(ex, PF_JUMP_CALL, M_PF_JUMP)
}

// --- character platforming effects ------------------------------------------------------------

/// CharacterPlatformingSystem per-component lambda (FUN_00a7ea90): characters within 700 px
/// (dist^2 < 490000) of the camera centre that fly or are knocked back are queued; the queue
/// (FUN_00a809f0) emits the jetpack particles + `jetpack` named event (FUN_00a81440) and the
/// liquid push (FUN_00a819f0, two global-LCG draws per cell). Position at [esp+0x54/0x58].
const CHARACTER_CALL: u32 = 0x00a8_031c;

/// Jetpack/knockback effects happen near any player.
pub(crate) unsafe fn patch_character_effects(ex: &mut Exec) -> Result<(), PatchError> {
    hook_getter(ex, CHARACTER_CALL, M_CHARACTER)
}

// --- worm and dragon ground bursts ------------------------------------------------------------

/// WormSystem (FUN_00c44480): when a worm crosses the ground surface within 128 px of the
/// camera centre (dist^2 < 16384), 20 grid particles burst out (2 global-LCG draws each).
/// `mov eax,[eax+0xc]; mov edi,[esp+0x6c]` (the target of `jnz` at 0x00c454b3), then the
/// distance to [esp+0x40] (x), [esp+0x60] (y).
const WORM_SITE: u32 = 0x00c4_54ed;
const WORM_ORIG: [u8; 7] = [0x8B, 0x40, 0x0C, 0x8B, 0x7C, 0x24, 0x6C];
/// BossDragonSystem (FUN_00a53610): the same burst; `mov eax,[eax+0xc]; movss xmm2,[0.5]`
/// (target of `jnz` at 0x00a5472e), x at [esp+0x7c], y at [esp+0x50].
const DRAGON_SITE: u32 = 0x00a5_4768;
const DRAGON_ORIG: [u8; 11] = [0x8B, 0x40, 0x0C, 0xF3, 0x0F, 0x10, 0x15, 0x1C, 0x36, 0x05, 0x01];

/// Worm and dragon ground bursts happen near any player.
pub(crate) unsafe fn patch_worm_bursts(ex: &mut Exec) -> Result<(), PatchError> {
    expect(WORM_SITE, &WORM_ORIG)?;
    expect(DRAGON_SITE, &DRAGON_ORIG)?;
    // tail: mov eax,[eax+0xc]; mov edi,[esp+0x70]; ret
    let worm = ctx_stub(ex, M_WORM, &[0x8B, 0x40, 0x0C, 0x8B, 0x7C, 0x24, 0x70, 0xC3]);
    write_code(WORM_SITE, &call_at(WORM_SITE, worm, WORM_ORIG.len()))?;
    // tail: mov eax,[eax+0xc]; movss xmm2,[0x0105361c]; ret
    let dragon = ctx_stub(ex, M_DRAGON, &[0x8B, 0x40, 0x0C, 0xF3, 0x0F, 0x10, 0x15, 0x1C, 0x36, 0x05, 0x01, 0xC3]);
    write_code(DRAGON_SITE, &call_at(DRAGON_SITE, dragon, DRAGON_ORIG.len()))
}

// --- telepathy --------------------------------------------------------------------------------

/// GameEffectSystem (FUN_00b015d0), effect 10 (TELEPATHY) on a `player_unit`: every
/// `mortal` entity in the camera rect ±32 that is lit gets an eye sprite (data/particles/
/// eye.xml) drawn over it. `call FUN_00439bb0; push; lea ecx; mov eax,[eax+0xc]`; EBX = the
/// effect's holder.
const TELEPATHY_CALL: u32 = 0x00b0_223f;

/// Telepathy marks creatures around its holder's view.
pub(crate) unsafe fn patch_telepathy(ex: &mut Exec) -> Result<(), PatchError> {
    hook_getter(ex, TELEPATHY_CALL, M_TELEPATHY)
}

// --- pixel scenes -----------------------------------------------------------------------------

/// Pending pixel-scene placement (FUN_0087ec40, from the chunk integration inside the grid
/// update): a scene that intersects the camera's int rect is not placed yet (no pop-in on
/// screen). `call FUN_00439bb0; mov ecx,[ebp-0x10]` (scene x) `; mov eax,[eax+0xc]`; scene y
/// in EBX, size {w, h} at [[esi+0xdc]]. The scene waits iff trunc(x+w) >= sx,
/// trunc(y+h) >= sy, trunc(x) <= sx+sw and trunc(y) <= sy+sh.
const PIXEL_SCENE_CALL: u32 = 0x0087_ecad;

/// A pixel scene waits while it intersects any player's view.
pub(crate) unsafe fn patch_pixel_scenes(ex: &mut Exec) -> Result<(), PatchError> {
    hook_getter(ex, PIXEL_SCENE_CALL, M_PIXEL_SCENE)
}

unsafe fn pixel_scene(c: &Ctx) {
    if !active() {
        c.set_reg(EAX, real_gg());
        return;
    }
    let sx = rd(c.reg(EBP).wrapping_sub(0x10)) as i32;
    let sy = c.reg(EBX) as i32;
    let size = rd(c.reg(ESI) + 0xdc);
    let (sw, sh) = (rd(size) as i32, rd(size + 4) as i32);
    let hit = |r: &[i32; 4]| r[2] >= sx && r[3] >= sy && r[0] <= sx.wrapping_add(sw) && r[1] <= sy.wrapping_add(sh);
    let r = (0..views::count()).map(views::rect).find(hit).unwrap_or_else(|| views::rect(0));
    c.set_reg(EAX, fake_gg([r[0] as f32, r[1] as f32, (r[2] - r[0]) as f32, (r[3] - r[1]) as f32]));
}

// --- camera-edge distance: SpriteParticleEmitter camera_bound, Torch --------------------------

/// FUN_00a5c610 (fastcall ECX = entity, XMM1 = r; returns AL): whether the entity is within
/// r of the camera rect's edge. Callers: SpriteParticleEmitterSystem (FUN_00c1fde0,
/// `camera_bound`/`camera_distance`; the emission draws the global LCG) and TorchSystem
/// (FUN_00c34d80; ignition, suffocation, global LCG). Prologue `push ebp; mov ebp,esp;
/// push -1` replaced by a jump.
const EDGE_FN: u32 = 0x00a5_c610;
const EDGE_PROLOGUE: [u8; 5] = [0x55, 0x8B, 0xEC, 0x6A, 0xFF];
const EDGE_RESUME: u32 = EDGE_FN + 5;

/// Camera-bound sprite emitters and torches run near any player's view.
pub(crate) unsafe fn patch_camera_edge(ex: &mut Exec) -> Result<(), PatchError> {
    expect(EDGE_FN, &EDGE_PROLOGUE)?;
    // test edx,edx; jz vanilla; ret; vanilla: push ebp; mov ebp,esp; push -1; push RESUME; ret
    let mut tail = vec![0x85, 0xD2, 0x74, 0x01, 0xC3];
    tail.extend_from_slice(&EDGE_PROLOGUE);
    tail.extend_from_slice(&jump_abs(EDGE_RESUME));
    let stub = ctx_stub(ex, M_EDGE, &tail);
    let mut jmp = vec![0xE9];
    jmp.extend_from_slice(&rel32(EDGE_FN + 5, stub));
    write_code(EDGE_FN, &jmp)
}

/// FUN_00a5c610's test, in its operation order: the edge distance per axis is
/// `max(p - hi, 0)` if that exceeds `lo - p`, else `lo - p`; near iff r*r > dx*dx + dy*dy.
fn edge_near(px: f32, py: f32, rect: [f32; 4], r: f32) -> bool {
    let [x0, y0, w, h] = rect;
    let (y1, x1) = (h + y0, w + x0);
    let (a, b) = (y0 - py, py - y1);
    let bz = if b > 0.0 { b } else { 0.0 };
    let dy = if bz > a { bz } else { a };
    let (c, d) = (x0 - px, px - x1);
    let dz = if d > 0.0 { d } else { 0.0 };
    let dx = if dz > c { dz } else { c };
    r * r > dx * dx + dy * dy
}

/// EDX = 1: answered (EAX = 0/1); EDX = 0: run the vanilla body (EDX is scratch for a
/// fastcall callee).
unsafe fn edge(c: &Ctx) {
    c.set_reg(EDX, 0);
    if !active() {
        return;
    }
    let Some(cam) = camera_rect() else { return };
    let (x, y) = entity_pos(c.reg(ECX));
    let r = c.xmm(1);
    let near = (0..views::count()).any(|i| edge_near(x, y, view_camera_rect(i, cam), r));
    c.set_reg(EAX, near as u32);
    c.set_reg(EDX, 1);
}

// --- verlet culling ---------------------------------------------------------------------------

/// VerletPhysicsSystem (FUN_00c3a3d0) culls each verlet object (ropes, vines, cloth) whose
/// middle is outside the camera rect grown by 128 (x) / 256 (y): no simulation, child
/// sprites hidden. Test in the per-component lambda FUN_00c3a610: midpoint XMM2 (x),
/// XMM1 (y), lo {x0, y0} at [ESI], hi {x1, y1} at [EAX]; inside iff x >= x0 && x1 > x &&
/// y >= y0 && y1 > y → EAX = 1/0, then `test al,al` at 0x00c3a8e6. The block also reloads
/// ESI = [esp+0x18] and EDI = [esp+0x28] (the component, already in EDI).
const VERLET_SITE: u32 = 0x00c3_a8b4;
const VERLET_ORIG: [u8; 50] = [
    0x0F, 0x2F, 0x16, 0x8B, 0x74, 0x24, 0x18, 0x72, 0x27, 0xF3, 0x0F, 0x10, 0x00, 0x0F, 0x2F, 0xC2, 0x76, 0x1E, 0x8B, 0x7A,
    0x14, 0x0F, 0x2F, 0x4F, 0x04, 0x8B, 0x7C, 0x24, 0x28, 0x72, 0x11, 0xF3, 0x0F, 0x10, 0x40, 0x04, 0x0F, 0x2F, 0xC1, 0x76,
    0x07, 0xB8, 0x01, 0x00, 0x00, 0x00, 0xEB, 0x02, 0x33, 0xC0,
];

/// Verlet objects simulate and show inside any player's (grown) view.
pub(crate) unsafe fn patch_verlet_cull(ex: &mut Exec) -> Result<(), PatchError> {
    expect(VERLET_SITE, &VERLET_ORIG)?;
    let stub = ctx_stub(ex, M_VERLET, &[0xC3]);
    // call stub; jmp short VERLET_SITE + 50
    let mut code = call_at(VERLET_SITE, stub, 5);
    code.extend_from_slice(&[0xEB, (VERLET_ORIG.len() - 7) as u8]);
    code.resize(VERLET_ORIG.len(), 0x90);
    write_code(VERLET_SITE, &code)
}

unsafe fn verlet(c: &Ctx) {
    let s = c.site_esp();
    let (lo, hi) = (c.reg(ESI), c.reg(EAX));
    let (x, y) = (c.xmm(2), c.xmm(1));
    let rect = [f32_at(lo), f32_at(lo + 4), f32_at(hi), f32_at(hi + 4)];
    let inside = |r: [f32; 4]| x >= r[0] && r[2] > x && y >= r[1] && r[3] > y;
    let hit = match camera_rect() {
        Some(cam) if active() => {
            let (ccx, ccy) = camera_centre(cam);
            (0..views::count()).any(|i| {
                let (cx, cy) = views::centre(i);
                let (dx, dy) = (cx - ccx, cy - ccy);
                inside([rect[0] + dx, rect[1] + dy, rect[2] + dx, rect[3] + dy])
            })
        }
        _ => inside(rect),
    };
    c.set_reg(EAX, hit as u32);
    c.set_reg(ESI, rd(s + 0x18));
    c.set_reg(EDI, rd(s + 0x28));
}

// --- particle emitter culling -----------------------------------------------------------------

/// ParticleEmitterSystem (FUN_00bc8bb0): an emitter that is neither cosmetic nor
/// render-on-grid (+0x60, +0x61) only emits while its entity is within
/// max(+0x98 * k, c) of the camera rect: `call FUN_00d13f10` (point-to-AABB dist^2: ECX =
/// &point, EDX = &min, [esp] = &max; result XMM0; plain `ret`). The emission draws the global
/// LCG and creates grid particles.
const EMITTER_DIST_CALL: u32 = 0x00bc_8d55;
const AABB_DIST_FN: u32 = 0x00d1_3f10;

/// Particle emitters emit near any player's view.
pub(crate) unsafe fn patch_emitter_views(ex: &mut Exec) -> Result<(), PatchError> {
    expect(EMITTER_DIST_CALL, &call_bytes(EMITTER_DIST_CALL, AABB_DIST_FN))?;
    let stub = ctx_stub(ex, M_EMITTER, &[0xC3]);
    write_code(EMITTER_DIST_CALL, &call_at(EMITTER_DIST_CALL, stub, 5))
}

/// FUN_00d13f10 in its operation order.
fn aabb_dist2(px: f32, py: f32, min: (f32, f32), max: (f32, f32)) -> f32 {
    let (dya, dyb) = (py - max.1, min.1 - py);
    let t = if dya > 0.0 { dya } else { 0.0 };
    let dy = if t > dyb { t } else { dyb };
    let (dxa, dxb) = (px - max.0, min.0 - px);
    let t = if dxa > 0.0 { dxa } else { 0.0 };
    let dx = if t > dxb { t } else { dxb };
    dx * dx + dy * dy
}

unsafe fn emitter(c: &Ctx) {
    let (p, min, max) = (c.reg(ECX), c.reg(EDX), rd(c.site_esp()));
    let (px, py) = (f32_at(p), f32_at(p + 4));
    let d = match camera_rect() {
        Some(cam) if active() => (0..views::count())
            .map(|i| {
                let r = view_camera_rect(i, cam);
                aabb_dist2(px, py, (r[0], r[1]), (r[2] + r[0], r[3] + r[1]))
            })
            .fold(f32::INFINITY, |a, b| if b < a { b } else { a }),
        _ => aabb_dist2(px, py, (f32_at(min), f32_at(min + 4)), (f32_at(max), f32_at(max + 4))),
    };
    c.set_xmm(0, d);
}

// --- fog of war ---------------------------------------------------------------------------------

/// WorldStateSystem (FUN_00c42d30) updates the fog-of-war image (GameGlobal+0x2c: w +8,
/// h +0xc, stride +0x30, bytes +0x48; one pixel per 32 world px, centred on the origin) in
/// two loops over the (2n+1)^2 cells (n = [esp+0x30] = ceil(427/64)+1 = 8) around a centre:
/// 1. around the camera centre ([esp+0x68], [esp+0x6c]): each pixel steps ±8 toward the
///    light at its cell (sky light map, solid cells dark);
/// 2. around the first `player_unit` (FUN_0063e920): each pixel gains the biome's
///    fog-of-war clearing amount.
///
/// Each loop now runs once per view, each pixel stepped once per loop (pixels a previous
/// view already stepped are saved before and restored after the next pass).
///
/// Loop 1 head (`mov eax,[esp+0x30]; mov ecx,eax`, entered from 0x00c433ee and 0x00c43402).
const FOG1_START: u32 = 0x00c4_3412;
const FOG1_START_ORIG: [u8; 6] = [0x8B, 0x44, 0x24, 0x30, 0x8B, 0xC8];
/// Loop 1 exit (`cmp dword [0x0122374c],0`, also reached when the fog is off).
const FOG1_END: u32 = 0x00c4_3721;
const FOG1_END_ORIG: [u8; 7] = [0x83, 0x3D, 0x4C, 0x37, 0x22, 0x01, 0x00];
/// Loop 1 re-entry (reloads XMM1 = 32, XMM6 = 16; needs ESI = image).
const FOG1_AGAIN: u32 = 0x00c4_3402;
/// Loop 2: `call FUN_0063e920` (first `player_unit`; fastcall ECX = EntityManager).
const FOG2_PLAYER_CALL: u32 = 0x00c4_376c;
const FIRST_PLAYER_FN: u32 = 0x0063_e920;
/// Loop 2 re-entry (`mov ecx,[EntityManager]` before that call; needs ESI = image).
const FOG2_AGAIN: u32 = 0x00c4_3766;
/// Loop 2 exit (`mov esi,[esp+0x54]; cmp byte [esi+0x1a5],0`).
const FOG2_END: u32 = 0x00c4_396e;
const FOG2_END_ORIG: [u8; 11] = [0x8B, 0x74, 0x24, 0x54, 0x80, 0xBE, 0xA5, 0x01, 0x00, 0x00, 0x00];
/// [esp+0x38] holds the image during loop 1 (loop 2 reuses the slot).
const LOOP1_IMAGE_SLOT: u32 = 0x38;
const FOG_N_SLOT: u32 = 0x30;

/// Set by the fog hooks for their stub tails: 1 = run the loop again / answered.
static FOG_FLAG: AtomicU32 = AtomicU32::new(0);

struct FogPass {
    active: bool,
    frame: u32,
    view: usize,
    image: u32,
    saved: Vec<(u32, u8)>,
}

impl FogPass {
    const fn new() -> Self {
        Self { active: false, frame: 0, view: 0, image: 0, saved: Vec::new() }
    }
}

static FOG1: Mutex<FogPass> = Mutex::new(FogPass::new());
static FOG2: Mutex<FogPass> = Mutex::new(FogPass::new());

/// The fog of war clears around every player.
pub(crate) unsafe fn patch_fog_of_war(ex: &mut Exec) -> Result<(), PatchError> {
    expect(FOG1_START, &FOG1_START_ORIG)?;
    expect(FOG1_END, &FOG1_END_ORIG)?;
    expect(FOG2_PLAYER_CALL, &call_bytes(FOG2_PLAYER_CALL, FIRST_PLAYER_FN))?;
    expect(FOG2_END, &FOG2_END_ORIG)?;

    // mov eax,[esp+0x34]; mov ecx,eax; ret
    let start = ctx_stub(ex, M_FOG1_START, &[0x8B, 0x44, 0x24, 0x34, 0x8B, 0xC8, 0xC3]);
    write_code(FOG1_START, &call_at(FOG1_START, start, FOG1_START_ORIG.len()))?;

    // flag ? (add esp,4; jmp AGAIN) : (cmp dword [0x0122374c],0; ret)
    let again1 = jump_abs(FOG1_AGAIN);
    let mut tail = cmp_flag(&FOG_FLAG);
    tail.extend_from_slice(&[0x74, (3 + again1.len()) as u8, 0x83, 0xC4, 0x04]);
    tail.extend_from_slice(&again1);
    tail.extend_from_slice(&FOG1_END_ORIG);
    tail.push(0xC3);
    let end1 = ctx_stub(ex, M_FOG1_END, &tail);
    write_code(FOG1_END, &call_at(FOG1_END, end1, FOG1_END_ORIG.len()))?;

    // flag ? ret (EAX = stand-in player) : jmp FUN_0063e920 (tail call)
    let original = jump_abs(FIRST_PLAYER_FN);
    let mut tail = cmp_flag(&FOG_FLAG);
    tail.extend_from_slice(&[0x75, original.len() as u8]);
    tail.extend_from_slice(&original);
    tail.push(0xC3);
    let player = ctx_stub(ex, M_FOG2_PLAYER, &tail);
    write_code(FOG2_PLAYER_CALL, &call_at(FOG2_PLAYER_CALL, player, 5))?;

    // flag ? (add esp,4; jmp AGAIN) : (mov esi,[esp+0x58]; cmp byte [esi+0x1a5],0; ret)
    let again2 = jump_abs(FOG2_AGAIN);
    let mut tail = cmp_flag(&FOG_FLAG);
    tail.extend_from_slice(&[0x74, (3 + again2.len()) as u8, 0x83, 0xC4, 0x04]);
    tail.extend_from_slice(&again2);
    tail.extend_from_slice(&[0x8B, 0x74, 0x24, 0x58]);
    tail.extend_from_slice(&FOG2_END_ORIG[4..]);
    tail.push(0xC3);
    let end2 = ctx_stub(ex, M_FOG2_END, &tail);
    write_code(FOG2_END, &call_at(FOG2_END, end2, FOG2_END_ORIG.len()))
}

/// The fog pixel coordinate of cell offset `d` around world coordinate `c` (both loops, in
/// their operation order): trunc(half + (trunc(trunc((d*32 + c) / 32) * 32) + 16) / 32).
fn fog_coord(c: f32, d: i32, half: i32) -> i32 {
    const INV32: f32 = 0.03125;
    let a = ((d as f32 * 32.0 + c) * INV32) as i32;
    let b = (a as f32 * 32.0) as i32;
    (half as f32 + (b as f32 + 16.0) * INV32) as i32
}

/// Inclusive pixel block {x0, y0, x1, y1} a loop covers around (cx, cy).
fn fog_block((cx, cy): (f32, f32), n: i32, w: i32, h: i32) -> [i32; 4] {
    [fog_coord(cx, -n, w / 2), fog_coord(cy, -n, h / 2), fog_coord(cx, n, w / 2), fog_coord(cy, n, h / 2)]
}

/// Before view `k`'s pass: remember the pixels of its block that views 0..k already
/// stepped this frame (restored after the pass, so they are stepped once).
unsafe fn fog_save_overlap(pass: &mut FogPass, n: i32) {
    let img = pass.image;
    let (w, h, stride, data) = (rd(img + 8) as i32, rd(img + 0x0c) as i32, rd(img + 0x30) as i32, rd(img + 0x48));
    let k = pass.view;
    let (cx, cy) = views::centre(k);
    let stepped = |px: i32, py: i32| {
        (0..k).any(|j| {
            let b = fog_block(views::centre(j), n, w, h);
            px >= b[0] && px <= b[2] && py >= b[1] && py <= b[3]
        })
    };
    pass.saved.clear();
    for dy in -n..=n {
        let py = fog_coord(cy, dy, h / 2);
        for dx in -n..=n {
            let px = fog_coord(cx, dx, w / 2);
            if px < 0 || py < 0 || px >= w || py >= h {
                continue;
            }
            let p = data.wrapping_add(stride.wrapping_mul(py) as u32).wrapping_add(px as u32);
            if p != 0 && stepped(px, py) {
                pass.saved.push((p, *(p as *const u8)));
            }
        }
    }
}

unsafe fn game_frame() -> u32 {
    let gg = rd(GAME_GLOBAL_PTR);
    if gg == 0 {
        0
    } else {
        rd(gg)
    }
}

/// Start a pass sequence on its first entry this frame; false when inactive.
fn fog_begin(pass: &mut FogPass, image: u32) -> bool {
    if !active() {
        pass.active = false;
        return false;
    }
    let frame = unsafe { game_frame() };
    if !pass.active || pass.frame != frame {
        pass.active = true;
        pass.frame = frame;
        pass.view = 0;
        pass.image = image;
        pass.saved.clear();
    }
    true
}

unsafe fn fog1_start(c: &Ctx) {
    let s = c.site_esp();
    let mut pass = FOG1.lock().unwrap_or_else(|p| p.into_inner());
    if fog_begin(&mut pass, rd(s + LOOP1_IMAGE_SLOT)) {
        let (x, y) = views::centre(pass.view);
        set_f32_at(s + 0x68, x);
        set_f32_at(s + 0x6c, y);
    }
}

unsafe fn fog2_player(c: &Ctx) {
    FOG_FLAG.store(0, Ordering::Relaxed);
    let mut pass = FOG2.lock().unwrap_or_else(|p| p.into_inner());
    if fog_begin(&mut pass, c.reg(ESI)) {
        let (x, y) = views::centre(pass.view);
        c.set_reg(EAX, fake_entity(x, y));
        FOG_FLAG.store(1, Ordering::Relaxed);
    }
}

/// After a pass: undo its steps on the pixels earlier views already stepped, then go on
/// with the next view (ESI = image, flag set) or finish.
unsafe fn fog_end(c: &Ctx, which: &Mutex<FogPass>, image_slot: u32) {
    FOG_FLAG.store(0, Ordering::Relaxed);
    let mut pass = which.lock().unwrap_or_else(|p| p.into_inner());
    if !pass.active {
        return;
    }
    if pass.frame != game_frame() {
        // Left over from a frame whose update did not finish: start afresh next time.
        pass.active = false;
        pass.saved.clear();
        return;
    }
    for &(p, v) in &pass.saved {
        *(p as *mut u8) = v;
    }
    pass.saved.clear();
    pass.view += 1;
    if pass.view >= views::count() || !in_window() {
        pass.active = false;
        return;
    }
    let s = c.site_esp();
    if image_slot != 0 {
        pass.image = rd(s + image_slot);
    }
    fog_save_overlap(&mut pass, rd(s + FOG_N_SLOT) as i32);
    c.set_reg(ESI, pass.image);
    FOG_FLAG.store(1, Ordering::Relaxed);
}

// --- screen shake -----------------------------------------------------------------------------

/// Camera::AddShake (FUN_006f2db0; thiscall ECX = camera object, [ebp+8] = &pos, XMM1 =
/// strength): unless the current shake (+0x58) is already stronger, the strength is
/// attenuated by the position's distance from the camera rect (full inside it) and becomes
/// the new shake if stronger. From `movaps xmm2,xmm1` through `mov esi,[ebp+8]`; the hook
/// returns the result in XMM2 (also [ebp-0x14]), XMM4 = current, ESI = &pos and jumps to the
/// `comiss xmm2,xmm4` at 0x006f2ea2.
const SHAKE_SITE: u32 = 0x006f_2dd6;
const SHAKE_ORIG: [u8; 25] = [
    0x0F, 0x28, 0xD1, 0xF3, 0x0F, 0x11, 0x4D, 0xEC, 0xF3, 0x0F, 0x10, 0x61, 0x58, 0x0F, 0x2F, 0xE2, 0x0F, 0x87, 0x0E, 0x01,
    0x00, 0x00, 0x8B, 0x75, 0x08,
];
const SHAKE_COMPARE: u32 = 0x006f_2ea2;
/// Lua GameScreenshake (FUN_007a5a50): `cmp esi,3; jl` — fewer than 3 arguments shake at
/// the camera centre, i.e. "at each player's own camera".
const SCREENSHAKE_ARGC: u32 = 0x007a_5d32;
const SCREENSHAKE_ARGC_ORIG: [u8; 5] = [0x83, 0xFE, 0x03, 0x7C, 0x1D];
const SCREENSHAKE_ADD: u32 = 0x007a_5d54;
/// LaserEmitterSystem (FUN_00b946a0): the laser hit shakes with `1 - min(d / 100, 1)` (if
/// above 0.1), d = distance from the hit ([esp+0x28]) to the camera centre computed earlier at
/// [esp+0x6c]/[esp+0x70] (also the beam sound's listener point). Hooked at its first use,
/// `movss xmm1,[esp+0x28]`.
const LASER_SITE: u32 = 0x00b9_588f;
const LASER_ORIG: [u8; 6] = [0xF3, 0x0F, 0x10, 0x4C, 0x24, 0x28];
/// Engine constants the shake code uses (expected, then used as literals).
const SHAKE_CONSTANTS: [(u32, f32); 6] = [
    (0x0105_361c, 0.5),
    (0x0105_39f8, 2.0),
    (0x0105_3780, 1.0),
    (0x0105_3464, SHAKE_MIN),
    (0x0105_3730, SHAKE_DECAY),
    (0x0105_3fa4, RAND_DIV),
];
/// Shake below this is dropped; above it decays by [`SHAKE_DECAY`] per frame.
const SHAKE_MIN: f32 = 0.01;
const SHAKE_DECAY: f32 = 0.9;
const RAND_DIV: f32 = 32767.0;
/// Shake state on the camera object: offsets (render) and strength (simulation).
const CAM_SHAKE_X: u32 = 0x50;
const CAM_SHAKE_Y: u32 = 0x54;
const CAM_SHAKE: u32 = 0x58;

/// Set by GameScreenshake without a position, consumed by the next AddShake.
static SHAKE_AT_CAMERA: AtomicU32 = AtomicU32::new(0);
/// This client's own shake strength (f32 bits) and the view it follows.
static LOCAL_SHAKE: AtomicU32 = AtomicU32::new(0);
static LOCAL_VIEW: AtomicU32 = AtomicU32::new(u32::MAX);
static LOCAL_RNG: AtomicU32 = AtomicU32::new(0x2545_f491);
static SHAKE_ON: AtomicBool = AtomicBool::new(false);
/// Simulation offsets parked while the render camera uses this client's own.
static PARKED: Mutex<Option<(u32, u32)>> = Mutex::new(None);

/// Screen shake for every player: the shared shake is the strongest any player would feel;
/// each client renders its own.
pub(crate) unsafe fn patch_screen_shake(ex: &mut Exec) -> Result<(), PatchError> {
    expect(SHAKE_SITE, &SHAKE_ORIG)?;
    expect(SCREENSHAKE_ARGC, &SCREENSHAKE_ARGC_ORIG)?;
    expect(LASER_SITE, &LASER_ORIG)?;
    for (addr, v) in SHAKE_CONSTANTS {
        expect(addr, &v.to_le_bytes())?;
    }
    let stub = ctx_stub(ex, M_SHAKE, &[0xC3]);
    let mut code = call_at(SHAKE_SITE, stub, 5);
    code.push(0xE9);
    code.extend_from_slice(&rel32(SHAKE_SITE + 10, SHAKE_COMPARE));
    write_code(SHAKE_SITE, &code)?;

    // cmp esi,3; jl +1; ret; mov dword [flag],1; add esp,4; jmp SCREENSHAKE_ADD
    let mut argc = vec![0x83, 0xFE, 0x03, 0x7C, 0x01, 0xC3, 0xC7, 0x05];
    argc.extend_from_slice(&(SHAKE_AT_CAMERA.as_ptr() as u32).to_le_bytes());
    argc.extend_from_slice(&1u32.to_le_bytes());
    argc.extend_from_slice(&[0x83, 0xC4, 0x04]);
    argc.extend_from_slice(&jump_abs(SCREENSHAKE_ADD));
    let argc = ex.put(&argc);
    write_code(SCREENSHAKE_ARGC, &call_at(SCREENSHAKE_ARGC, argc, 5))?;

    // tail: movss xmm1,[esp+0x2c]; ret
    let laser = ctx_stub(ex, M_LASER, &[0xF3, 0x0F, 0x10, 0x4C, 0x24, 0x2C, 0xC3]);
    write_code(LASER_SITE, &call_at(LASER_SITE, laser, LASER_ORIG.len()))?;
    SHAKE_ON.store(true, Ordering::Relaxed);
    Ok(())
}

/// AddShake's attenuation, in its operation order.
fn attenuate(rect: [f32; 4], px: f32, py: f32, s: f32) -> f32 {
    let [x, y, w, h] = rect;
    if px >= x && x + w > px && py >= y && y + h > py {
        return s;
    }
    let hh = h * 0.5;
    let hw = w * 0.5;
    let w2 = w * 2.0;
    let dy = (hh + y) - py;
    let dx = (hw + x) - px;
    let mut t = dx * dx + dy * dy;
    t -= hw * hw;
    if 0.0 > t {
        t = 0.0;
    }
    t /= w2 * w2;
    if t > 1.0 {
        t = 1.0;
    }
    s - t * t * s
}

unsafe fn shake(c: &Ctx) {
    let this = c.reg(ECX);
    let pos = rd(c.reg(EBP) + 8);
    let s = c.xmm(1);
    let at_camera = SHAKE_AT_CAMERA.swap(0, Ordering::Relaxed) != 0;
    let current = f32_at(this + CAM_SHAKE);
    let (px, py) = (f32_at(pos), f32_at(pos + 4));
    let r = this as *const f32;
    let cam = [*r, *r.add(1), *r.add(2), *r.add(3)];
    let views_live = views::count() > 0;
    let shared = if !views_live {
        attenuate(cam, px, py, s)
    } else if !in_window() {
        // Outside the window the shared shake is simulation state: leave it.
        current
    } else if at_camera {
        s
    } else {
        (0..views::count()).map(|i| attenuate(view_camera_rect(i, cam), px, py, s)).fold(0.0f32, |a, b| if b > a { b } else { a })
    };
    if views_live {
        let local = if !in_window() {
            Some(attenuate(cam, px, py, s))
        } else if at_camera {
            Some(s)
        } else {
            match LOCAL_VIEW.load(Ordering::Relaxed) {
                v if (v as usize) < views::count() => Some(attenuate(view_camera_rect(v as usize, cam), px, py, s)),
                _ => None,
            }
        };
        if let Some(l) = local {
            if l > f32::from_bits(LOCAL_SHAKE.load(Ordering::Relaxed)) {
                LOCAL_SHAKE.store(l.to_bits(), Ordering::Relaxed);
            }
        }
    }
    c.set_xmm(2, shared);
    c.set_xmm(4, current);
    c.set_reg(ESI, pos);
    set_f32_at(c.reg(EBP).wrapping_sub(0x14), shared);
}

unsafe fn laser(c: &Ctx) {
    if !active() {
        return;
    }
    let s = c.site_esp();
    if let Some((i, _)) = views::nearest(f32_at(s + 0x28), f32_at(s + 0x2c)) {
        let (x, y) = views::centre(i);
        set_f32_at(s + 0x6c, x);
        set_f32_at(s + 0x70, y);
    }
}

/// The view (index into [`views::centre`]) of this client's player, whose shake this client
/// renders; None (spectating, before the first window) renders the shared shake.
pub fn set_local_view(view: Option<usize>) {
    LOCAL_VIEW.store(view.map_or(u32::MAX, |v| v as u32), Ordering::Relaxed);
}

/// After the window closes, before the render camera is set: park the simulation's shake
/// offsets and put this client's own on the camera object, advancing its own shake as the
/// world update does (offsets uniform in [-s, s], then s *= 0.9; dropped below 0.01), with a
/// client-local RNG. Game thread.
///
/// # Safety
/// World initialised; pair with [`shake_render_leave`] before the next window.
pub unsafe fn shake_render_enter() {
    if !SHAKE_ON.load(Ordering::Relaxed) || views::count() == 0 || LOCAL_VIEW.load(Ordering::Relaxed) == u32::MAX {
        return;
    }
    let gg = rd(GAME_GLOBAL_PTR);
    if gg == 0 {
        return;
    }
    let cam = rd(gg + 0x0c);
    let mut parked = PARKED.lock().unwrap_or_else(|p| p.into_inner());
    if parked.is_none() {
        *parked = Some((rd(cam + CAM_SHAKE_X), rd(cam + CAM_SHAKE_Y)));
    }
    let s = f32::from_bits(LOCAL_SHAKE.load(Ordering::Relaxed));
    let (ox, oy) = if s <= SHAKE_MIN {
        LOCAL_SHAKE.store(0, Ordering::Relaxed);
        (0.0, 0.0)
    } else {
        let mut x = LOCAL_RNG.load(Ordering::Relaxed);
        let mut draw = || {
            x = x.wrapping_mul(0x343fd).wrapping_add(0x269ec3);
            ((x >> 16) & 0x7fff) as f32 / RAND_DIV
        };
        let (ry, rx) = (draw(), draw());
        LOCAL_RNG.store(x, Ordering::Relaxed);
        LOCAL_SHAKE.store((s * SHAKE_DECAY).to_bits(), Ordering::Relaxed);
        let lo = -s;
        (rx * (s - lo) + lo, ry * (s - lo) + lo)
    };
    set_f32_at(cam + CAM_SHAKE_X, ox);
    set_f32_at(cam + CAM_SHAKE_Y, oy);
}

/// Before the next window sets the simulation camera: restore the simulation's shake
/// offsets parked by [`shake_render_enter`]. Game thread.
///
/// # Safety
/// World initialised.
pub unsafe fn shake_render_leave() {
    let Some((x, y)) = PARKED.lock().unwrap_or_else(|p| p.into_inner()).take() else { return };
    let gg = rd(GAME_GLOBAL_PTR);
    if gg == 0 {
        return;
    }
    let cam = rd(gg + 0x0c);
    *((cam + CAM_SHAKE_X) as *mut u32) = x;
    *((cam + CAM_SHAKE_Y) as *mut u32) = y;
}

// --- Lua: is_in_camera_bounds -----------------------------------------------------------------

/// Lua `GameGetCameraBounds()` (FUN_007aea80, a lua_CFunction registered in every Lua state):
/// pushes the camera's int view rect (GridWorld+0x498 {x, y, w, h}). Vanilla
/// `is_in_camera_bounds(x, y, padding)` (data/scripts/lib/utilities.lua) tests a point against
/// it grown by `padding`, in Lua doubles. Its gameplay users (gravity fields, the moon,
/// physics fungi, swirling and scattering particles, portal effects) act only near the
/// camera, i.e. near player 1 inside the window.
///
/// The mod's `camera_bounds.lua` (appended to utilities.lua) answers through this function
/// called with arguments, which vanilla never does (it warns and ignores them):
/// `GameGetCameraBounds(x, y, padding)` returns whether the point is inside some view's
/// rect, `GameGetCameraBounds(px, py, x, y, padding)` whether it is inside the rect of the view
/// nearest to (px, py) (a script acting for the player there: the radar perk). View `k`'s rect
/// is the engine's int rect moved by the view centre's offset from the camera centre (as
/// `rain_views`; view 0's is the engine's own). Outside the window or without views the test
/// is vanilla's, against the engine's rect. Component scripts' Lua states have no FFI, hence a
/// native entry point every state already has.
const CAMERA_BOUNDS_FN: u32 = 0x007a_ea80;
/// push ebp; mov ebp,esp; and esp,-64
const CAMERA_BOUNDS_PROLOGUE: [u8; 6] = [0x55, 0x8B, 0xEC, 0x83, 0xE4, 0xC0];

/// lua51.dll entry points: (lua_gettop, lua_tonumber, lua_pushboolean).
struct LuaApi {
    gettop: extern "C" fn(u32) -> i32,
    tonumber: extern "C" fn(u32, i32) -> f64,
    pushboolean: extern "C" fn(u32, i32),
}

static LUA_API: std::sync::OnceLock<LuaApi> = std::sync::OnceLock::new();

extern "system" {
    fn GetModuleHandleA(name: *const u8) -> isize;
    fn GetProcAddress(module: isize, name: *const u8) -> usize;
}

/// Every player's view for `is_in_camera_bounds` (see [`CAMERA_BOUNDS_FN`]).
pub(crate) unsafe fn patch_lua_camera_bounds(ex: &mut Exec) -> Result<(), PatchError> {
    expect(CAMERA_BOUNDS_FN, &CAMERA_BOUNDS_PROLOGUE)?;
    let lua = GetModuleHandleA(c"lua51.dll".as_ptr() as *const u8);
    if lua == 0 {
        return Err(PatchError("lua51.dll not loaded".into()));
    }
    let proc = |name: &std::ffi::CStr| {
        let p = GetProcAddress(lua, name.as_ptr() as *const u8);
        if p == 0 {
            Err(PatchError(format!("lua51.dll has no {name:?}")))
        } else {
            Ok(p)
        }
    };
    let api = LuaApi {
        gettop: std::mem::transmute::<usize, extern "C" fn(u32) -> i32>(proc(c"lua_gettop")?),
        tonumber: std::mem::transmute::<usize, extern "C" fn(u32, i32) -> f64>(proc(c"lua_tonumber")?),
        pushboolean: std::mem::transmute::<usize, extern "C" fn(u32, i32)>(proc(c"lua_pushboolean")?),
    };
    let _ = LUA_API.set(api);
    // push [esp+4] (L); mov eax,handler; call eax; add esp,4; test eax,eax; js vanilla; ret;
    // vanilla: push ebp; mov ebp,esp; and esp,-64; push FN+6; ret
    let f = ((lua_camera_bounds as extern "C" fn(u32) -> i32) as u32).to_le_bytes();
    let mut s = vec![0xFF, 0x74, 0x24, 0x04, 0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x83, 0xC4, 0x04, 0x85, 0xC0, 0x78, 0x01, 0xC3];
    s.extend_from_slice(&CAMERA_BOUNDS_PROLOGUE);
    s.extend_from_slice(&jump_abs(CAMERA_BOUNDS_FN + CAMERA_BOUNDS_PROLOGUE.len() as u32));
    let stub = ex.put(&s);
    let mut jmp = vec![0xE9];
    jmp.extend_from_slice(&rel32(CAMERA_BOUNDS_FN + 5, stub));
    jmp.push(0x90);
    write_code(CAMERA_BOUNDS_FN, &jmp)
}

/// GameGetCameraBounds with 3 or 5 arguments: pushes the boolean, returns 1; otherwise -1
/// (vanilla runs).
extern "C" fn lua_camera_bounds(l: u32) -> i32 {
    let Some(api) = LUA_API.get() else { return -1 };
    let (near, first) = match (api.gettop)(l) {
        3 => (None, 1),
        5 => (Some(((api.tonumber)(l, 1), (api.tonumber)(l, 2))), 3),
        _ => return -1,
    };
    let (x, y, pad) = ((api.tonumber)(l, first), (api.tonumber)(l, first + 1), (api.tonumber)(l, first + 2));
    let hit = unsafe { in_camera_bounds(near, x, y, pad) };
    (api.pushboolean)(l, hit as i32);
    1
}

/// `is_in_camera_bounds` for every player's view (see [`CAMERA_BOUNDS_FN`]).
unsafe fn in_camera_bounds(near: Option<(f64, f64)>, x: f64, y: f64, pad: f64) -> bool {
    let world = rd(real_gg() + 0x0c);
    let grid_world = if world != 0 { rd(world + 0x44) } else { 0 };
    if grid_world == 0 {
        return false;
    }
    let gi = |o: u32| rd(grid_world + o) as i32;
    let (ox, oy, w, h) = (gi(0x498), gi(0x49c), gi(0x4a0), gi(0x4a4));
    let inside = |left: i32, up: i32| {
        let (left, up, w, h) = (left as f64, up as f64, w as f64, h as f64);
        x >= left - pad && y >= up - pad && x <= left + w + pad && y <= up + h + pad
    };
    let Some(cam) = camera_rect().filter(|_| active()) else { return inside(ox, oy) };
    let (ccx, ccy) = camera_centre(cam);
    let view = |k: usize| {
        let (kx, ky) = views::centre(k);
        inside(
            ox.wrapping_add(crate::cosmetic::floor_px(kx - ccx + 0.5)),
            oy.wrapping_add(crate::cosmetic::floor_px(ky - ccy + 0.5)),
        )
    };
    match near {
        Some((px, py)) => views::nearest(px as f32, py as f32).is_some_and(|(k, _)| view(k)),
        None => (0..views::count()).any(view),
    }
}

/// Diagnostics for `NOITA_ONLINE_RNGLOG`: the shared shake, this client's own shake and view,
/// and per view the sum of the fog-of-war pixels of its 17×17 update block.
///
/// # Safety
/// Game thread, world initialised.
pub unsafe fn debug_line() -> String {
    use std::fmt::Write;
    let gg = rd(GAME_GLOBAL_PTR);
    let cam = rd(gg + 0x0c);
    let mut s = format!(
        "shake={} lshake={} lview={}",
        f32_at(cam + CAM_SHAKE),
        f32::from_bits(LOCAL_SHAKE.load(Ordering::Relaxed)),
        LOCAL_VIEW.load(Ordering::Relaxed) as i32
    );
    let img = rd(gg + 0x2c);
    if img != 0 {
        let (w, h, stride, data) = (rd(img + 8) as i32, rd(img + 0x0c) as i32, rd(img + 0x30) as i32, rd(img + 0x48));
        for k in 0..views::count() {
            let b = fog_block(views::centre(k), 8, w, h);
            let mut sum = 0u32;
            for py in b[1].max(0)..=b[3].min(h - 1) {
                for px in b[0].max(0)..=b[2].min(w - 1) {
                    sum += *((data + (stride * py + px) as u32) as *const u8) as u32;
                }
            }
            let _ = write!(s, " fog{k}={sum}");
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attenuation_is_full_inside_and_fades_outside() {
        let cam = [0.0, 0.0, 427.0, 242.0];
        assert_eq!(attenuate(cam, 100.0, 100.0, 50.0), 50.0);
        let near = attenuate(cam, 500.0, 121.0, 50.0);
        let far = attenuate(cam, 900.0, 121.0, 50.0);
        assert!(near > far && far >= 0.0 && near < 50.0);
        assert_eq!(attenuate(cam, 5000.0, 121.0, 50.0), 0.0);
    }

    #[test]
    fn edge_distance_matches_the_rect() {
        let r = [0.0, 0.0, 100.0, 50.0];
        assert!(edge_near(50.0, 25.0, r, 1.0));
        assert!(edge_near(110.0, 25.0, r, 11.0));
        assert!(!edge_near(110.0, 25.0, r, 9.0));
        assert!(!edge_near(50.0, -20.0, r, 19.0));
        assert_eq!(aabb_dist2(110.0, 25.0, (0.0, 0.0), (100.0, 50.0)), 100.0);
        assert_eq!(aabb_dist2(50.0, 25.0, (0.0, 0.0), (100.0, 50.0)), 0.0);
    }

    #[test]
    fn fog_block_is_one_pixel_per_cell() {
        let b = fog_block((1000.0, -300.0), 8, 2048, 2048);
        assert_eq!(b[2] - b[0], 16);
        assert_eq!(b[3] - b[1], 16);
        for d in -8..8 {
            assert_eq!(fog_coord(1000.0, d + 1, 1024) - fog_coord(1000.0, d, 1024), 1);
        }
    }
}
