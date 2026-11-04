//! Patch `box2d_overlap` (performance): the Box2D step runs while this client renders, at a
//! point of vanilla's own timing range, instead of inline in the world update.
//!
//! Vanilla (`GridWorld::Update` FUN_0071a6b0) sets the b2World's "step in progress" flag
//! (`b2World+0x191d8`, `xchg` @0x0071a7d2) and submits the step to the job pool at the end of
//! the grid update (`call FUN_0084c1a0` @0x0071a7f2, lambda vtable 0x01005ce0, call
//! FUN_009a48d0: flag = 1, `b2World::Step` FUN_009c0d00, flag = 0). The step clears the flag
//! itself; the join at the next grid update (FUN_0045d040 on the job handle GridWorld+0x4f8,
//! @0x0071a708) only waits for the job. Meanwhile the entity systems run, and they read
//! bodies without any check (racy in vanilla). Everything that changes Box2D structure first
//! tests the flag (with `e_locked` and +0x191d9, both set only inside `b2World::Step`): 97
//! `mov al,[reg+0x191d8]; test al,al` sites ([`FLAG_READS`]). The Noita-side ones (physics
//! body / body2 / AI / projectile / telekinesis / player systems, Lua physics API, grid world
//! body code) wait until the flag clears (platform Sleep(0) loops, or FUN_0045d040 on the
//! handle); Box2D's own guards (CreateBody, DestroyBody, CreateFixture, SetTransform, ...)
//! refuse the call. The contact listener (GridWorld+0x538, `grid::PhysicsListener`, vtable
//! 0x01006f00) only records PreSolve contacts into its own array (no RNG, no game code); the
//! game thread processes them after the join (FUN_006f9210). The contact filter is Box2D's
//! default (category and group bits). The step's callee tree has no RNG global.
//!
//! `inline_async` used to run the step at the submit, i.e. as a machine whose worker finishes
//! it at once. Here the submit keeps vanilla's flag and records the step without running it
//! ([`defer_submit`]); it runs at the first of:
//! - a flag test on the game thread (the stub at every [`FLAG_READS`] site runs the step
//!   first, then the test reads the cleared flag): the timing of a worker that finishes the
//!   step right before that code looks;
//! - the start of the render (DeathMatch::Update's call of the particle render prep
//!   @0x006b3a97, [`start`]): it goes to a worker thread while this client renders, the
//!   timing of a worker that had not started it before the update ended. Not window exit:
//!   DeathMatch::Update still runs simulation code after it (the player spawn FUN_006b7e80
//!   and its `OnPlayerSpawned` at 0x006b3a25..0x006b3a57);
//! - the next `DeathMatch::Update` ([`join`]) at the latest, which waits for it.
//!
//! Every choice is a deterministic point of the simulation, so every client steps the same
//! bodies at the same moment. Until then the systems read the bodies as they were before the
//! step (vanilla allows any interleaving).

use crate::patches::{expect, rd, redirect_call, rel32, write_code, Exec, PatchError};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

extern "system" {
    fn GetCurrentThreadId() -> u32;
}

/// `GridWorld::Update`'s step submit: `mov ecx,[0x012051c4]; push eax; call FUN_0084c1a0`
/// (thiscall pool, `const std::function<void()>&`; returns the job handle, stored at
/// GridWorld+0x4f8).
const STEP_SUBMIT_CALL: u32 = 0x0071_a7f2;
const POOL_ASYNC_FN: u32 = 0x0084_c1a0;
/// The grid update's own join of the last step: `call FUN_0045d040` (fastcall, ECX = handle:
/// spins while handle+0x1c > 0), taken only if GridWorld+0x4f8 is set.
const STEP_JOIN_CALL: u32 = 0x0071_a708;
const JOB_JOIN_FN: u32 = 0x0045_d040;
/// The step lambda: `std::_Func_impl` vtable (captured GridWorld* at +4) and its `_Do_call`
/// FUN_009a48d0 (thiscall, no arguments).
const STEP_LAMBDA_VTBL: u32 = 0x0100_5ce0;
const STEP_LAMBDA_CALL: u32 = 0x009a_48d0;
/// `std::function` (MSVC 2013): the `_Impl` pointer after the 16-byte inline buffer.
const FUNCTION_IMPL: u32 = 0x10;
/// Job handle: pending count at +0x1c, which FUN_0045d040 spins on.
const HANDLE_SIZE: u32 = 0x28;
const HANDLE_PENDING: u32 = 0x1c;
/// DeathMatch::Update's vtable slot and function.
const DM_UPDATE_SLOT: u32 = 0x0100_cdec;
const DM_UPDATE_FN: u32 = 0x006b_26f0;
/// DeathMatch::Update's `call FUN_007148f0` (particle render prep, thiscall): the first call
/// of its render part, after the player spawn.
const RENDER_START_CALL: u32 = 0x006b_3a97;
const PARTICLE_RENDER_PREP_FN: u32 = 0x0071_48f0;
/// `mov al,byte ptr [reg+0x191d8]` (`8a <modrm> d8 91 01 00`), then `test al,al` (`84 c0`).
const FLAG_DISP: [u8; 4] = [0xD8, 0x91, 0x01, 0x00];

/// Every read of the "step in progress" flag in noita.exe (all of the form above; the other
/// four references are the submit's and the lambda's `xchg` writes and the b2World
/// constructor). Found by scanning `.text` for the displacement; no jump targets the
/// `test`.
const FLAG_READS: [u32; 97] = [
    0x004a_d7a3, 0x0065_b7b7, 0x0070_e5cd, 0x0070_e62b, 0x0070_fea9, 0x0071_7b49, 0x0071_9799, 0x0071_c623,
    0x0071_c948, 0x0071_cac3, 0x0071_cb59, 0x0071_d2da, 0x0071_e226, 0x0071_ed68, 0x0072_0984, 0x0072_b628,
    0x0073_efcc, 0x007c_fed3, 0x007d_1213, 0x007d_182e, 0x007d_1dc4, 0x007d_220c, 0x007d_2c93, 0x0081_1465,
    0x0083_d4d4, 0x0083_d9f7, 0x0083_e3a3, 0x0084_21fd, 0x0091_eaa3, 0x0091_ec98, 0x0091_f663, 0x0091_f853,
    0x009b_c5ec, 0x009b_c78c, 0x009b_c8a4, 0x009b_cc75, 0x009b_f143, 0x009b_f232, 0x009b_f3f3, 0x009b_f54f,
    0x00a4_4d63, 0x00b9_9ae5, 0x00ba_67b6, 0x00bc_e28c, 0x00bc_eca9, 0x00bc_f12a, 0x00bc_f8c9, 0x00bc_fa19,
    0x00bd_8396, 0x00bd_8919, 0x00bd_8ef9, 0x00bd_a6b9, 0x00bd_a7a9, 0x00bd_ab69, 0x00bd_ac1d, 0x00bd_b1c9,
    0x00bd_b4e9, 0x00bd_b669, 0x00bd_b81a, 0x00bd_bba7, 0x00bd_c319, 0x00bd_c3cb, 0x00bd_c50a, 0x00bd_ca39,
    0x00bd_d8b9, 0x00bd_d968, 0x00bd_dac9, 0x00bd_e409, 0x00bd_e4f9, 0x00bd_ec59, 0x00bd_f0a9, 0x00bd_f279,
    0x00bd_f32b, 0x00bd_f4a9, 0x00bd_f65a, 0x00bd_f879, 0x00bd_f928, 0x00be_00a9, 0x00be_0ba8, 0x00be_0df3,
    0x00be_1d53, 0x00be_1e33, 0x00be_1f34, 0x00be_630e, 0x00be_65be, 0x00be_689e, 0x00be_69fc, 0x00be_6bf5,
    0x00be_9293, 0x00bf_2d69, 0x00bf_af29, 0x00bf_b3a9, 0x00bf_b659, 0x00c0_6856, 0x00c2_ed07, 0x00c3_71e6,
    0x00c4_0087,
];

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Idle,
    /// Submitted, not started (GridWorld*).
    Deferred(u32),
    /// On the worker or being run by the game thread.
    Running,
}

struct Ctl {
    step: Step,
    worker: Option<std::sync::mpsc::Sender<(u32, u16, u32)>>,
}

static CTL: Mutex<Ctl> = Mutex::new(Ctl { step: Step::Idle, worker: None });
static DONE: Condvar = Condvar::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static GAME_THREAD: AtomicU32 = AtomicU32::new(0);
/// Byte the flag stubs test: a step is deferred or running.
static PENDING: AtomicU32 = AtomicU32::new(0);
/// The fake job handle returned to the submit (GridWorld+0x4f8).
static HANDLE: AtomicU32 = AtomicU32::new(0);
/// A step finished since the slow-step check ([`check_step_time`]) last ran.
static UNCHECKED: AtomicBool = AtomicBool::new(false);

fn ctl() -> MutexGuard<'static, Ctl> {
    CTL.lock().unwrap_or_else(|p| p.into_inner())
}

fn on_game_thread() -> bool {
    unsafe { GetCurrentThreadId() == GAME_THREAD.load(Ordering::Relaxed) }
}

/// Whether the patch is installed.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Install `box2d_overlap`.
///
/// # Safety
/// Patch installation (game thread, before the world exists).
pub(crate) unsafe fn patch(ex: &mut Exec) -> Result<(), PatchError> {
    // mov ecx,[0x012051c4]; push eax; call FUN_0084c1a0
    expect(STEP_SUBMIT_CALL - 7, &[0x8B, 0x0D, 0xC4, 0x51, 0x20, 0x01, 0x50])?;
    for (site, target) in [(STEP_SUBMIT_CALL, POOL_ASYNC_FN), (STEP_JOIN_CALL, JOB_JOIN_FN), (RENDER_START_CALL, PARTICLE_RENDER_PREP_FN)] {
        let rel = rel32(site + 5, target);
        expect(site, &[0xE8, rel[0], rel[1], rel[2], rel[3]])?;
    }
    expect(DM_UPDATE_SLOT, &DM_UPDATE_FN.to_le_bytes())?;
    let mut regs = Vec::with_capacity(FLAG_READS.len());
    for site in FLAG_READS {
        let modrm = *((site + 1) as *const u8);
        // [eax] [ecx] [edx] [esi] [edi] + disp32, destination AL
        if !matches!(modrm, 0x80 | 0x81 | 0x82 | 0x86 | 0x87) {
            return Err(PatchError(format!("flag read at {site:08x}: modrm {modrm:02x}")));
        }
        expect(site, &[0x8A, modrm, FLAG_DISP[0], FLAG_DISP[1], FLAG_DISP[2], FLAG_DISP[3], 0x84, 0xC0])?;
        regs.push(modrm);
    }
    GAME_THREAD.store(GetCurrentThreadId(), Ordering::Relaxed);
    let pending = ex.alloc_data(4);
    PENDING.store(pending, Ordering::Relaxed);
    HANDLE.store(ex.alloc_data(HANDLE_SIZE), Ordering::Relaxed);

    // One stub per base register: if the flag is set and a step is pending, run
    // `flag_observed(return address)` first; then the original `mov al,[reg+0x191d8];
    // test al,al` (flags are the caller's result; `ret` keeps them).
    let mut stubs = [0u32; 8];
    let observed = ((flag_observed as extern "C" fn(u32)) as u32).to_le_bytes();
    let p = pending.to_le_bytes();
    for modrm in [0x80u8, 0x81, 0x82, 0x86, 0x87] {
        let base = ex.alloc_data(256);
        // cmp byte [reg+0x191d8],0 (80 /7: modrm 0xb8 | reg)
        let mut c = vec![0x80, 0xB8 | (modrm & 7), FLAG_DISP[0], FLAG_DISP[1], FLAG_DISP[2], FLAG_DISP[3], 0x00];
        let jz1 = c.len();
        c.extend_from_slice(&[0x74, 0x00]);
        // cmp byte [pending],0
        c.extend_from_slice(&[0x80, 0x3D, p[0], p[1], p[2], p[3], 0x00]);
        let jz2 = c.len();
        c.extend_from_slice(&[0x74, 0x00]);
        c.push(0x60); // pushad
        c.extend(crate::cosmetic::xmm_save());
        // push dword [esp+0xa0] (the return address); mov eax,flag_observed; call eax; add esp,4
        c.extend_from_slice(&[0xFF, 0xB4, 0x24, 0xA0, 0x00, 0x00, 0x00]);
        c.extend_from_slice(&[0xB8, observed[0], observed[1], observed[2], observed[3], 0xFF, 0xD0, 0x83, 0xC4, 0x04]);
        c.extend(crate::cosmetic::xmm_restore());
        c.push(0x61); // popad
        let fast = c.len();
        c[jz1 + 1] = (fast - (jz1 + 2)) as u8;
        c[jz2 + 1] = (fast - (jz2 + 2)) as u8;
        // mov al,[reg+0x191d8]; test al,al; ret
        c.extend_from_slice(&[0x8A, modrm, FLAG_DISP[0], FLAG_DISP[1], FLAG_DISP[2], FLAG_DISP[3], 0x84, 0xC0, 0xC3]);
        assert!(c.len() <= 256 && fast - (jz1 + 2) < 0x80);
        std::ptr::copy_nonoverlapping(c.as_ptr(), base as *mut u8, c.len());
        stubs[(modrm & 7) as usize] = base;
    }

    // DeathMatch::Update starts by waiting for the step; the render start sends it to the
    // worker: pushad; mov eax,cb; call eax; popad; jmp target.
    let hook = |ex: &mut Exec, cb: extern "C" fn(), target: u32| {
        let at = ex.alloc_data(32);
        let f = (cb as u32).to_le_bytes();
        let mut c = vec![0x60, 0xB8, f[0], f[1], f[2], f[3], 0xFF, 0xD0, 0x61];
        crate::cosmetic::jump(&mut c, at, &[0xE9], target);
        std::ptr::copy_nonoverlapping(c.as_ptr(), at as *mut u8, c.len());
        at
    };
    let dm = hook(ex, frame_join, DM_UPDATE_FN);
    let render = hook(ex, render_start, PARTICLE_RENDER_PREP_FN);

    for (site, modrm) in FLAG_READS.into_iter().zip(regs) {
        let mut call = vec![0xE8];
        call.extend_from_slice(&rel32(site + 5, stubs[(modrm & 7) as usize]));
        call.extend_from_slice(&[0x90; 3]);
        write_code(site, &call)?;
    }
    let submit = (defer_submit as extern "thiscall" fn(u32, u32) -> u32) as u32;
    redirect_call(STEP_SUBMIT_CALL, POOL_ASYNC_FN, submit)?;
    redirect_call(STEP_JOIN_CALL, JOB_JOIN_FN, (grid_join as extern "fastcall" fn(u32)) as u32)?;
    redirect_call(RENDER_START_CALL, PARTICLE_RENDER_PREP_FN, render)?;
    write_code(DM_UPDATE_SLOT, &dm.to_le_bytes())?;
    ACTIVE.store(true, Ordering::Relaxed);
    Ok(())
}

/// The step submit (thiscall `pool`, `func` = the `std::function`): record the step and return
/// the fake handle (pending). Anything but the step lambda on the game thread goes to the
/// engine's submit.
extern "thiscall" fn defer_submit(pool: u32, func: u32) -> u32 {
    unsafe {
        let imp = rd(func + FUNCTION_IMPL);
        if !on_game_thread() || imp == 0 || rd(imp) != STEP_LAMBDA_VTBL {
            let submit: extern "thiscall" fn(u32, u32) -> u32 = std::mem::transmute(POOL_ASYNC_FN as usize);
            return submit(pool, func);
        }
        let grid_world = rd(imp + 4);
        // A step still deferred here would be lost: the world update's own join (start of the
        // grid update) comes before the next submit, so this never happens; run it to be safe.
        join();
        let handle = HANDLE.load(Ordering::Relaxed);
        *((handle + HANDLE_PENDING) as *mut i32) = 1;
        *(PENDING.load(Ordering::Relaxed) as *mut u8) = 1;
        ctl().step = Step::Deferred(grid_world);
        stats(|s| s.deferred += 1);
        handle
    }
}

/// `std::function` call of the step lambda (FUN_009a48d0: flag = 1, step, flag = 0).
unsafe fn run_step(grid_world: u32) {
    let functor = [STEP_LAMBDA_VTBL, grid_world];
    let call: extern "thiscall" fn(*const u32) = std::mem::transmute(STEP_LAMBDA_CALL as usize);
    call(functor.as_ptr());
}

/// The step has returned (its flag is clear): the handle's job is done.
fn finish(mut c: MutexGuard<'static, Ctl>) {
    unsafe {
        *((HANDLE.load(Ordering::Relaxed) + HANDLE_PENDING) as *mut i32) = 0;
        *(PENDING.load(Ordering::Relaxed) as *mut u8) = 0;
    }
    c.step = Step::Idle;
    UNCHECKED.store(true, Ordering::Relaxed);
    DONE.notify_all();
}

/// Run a deferred step here (game thread), or wait for the running one.
fn complete(mut c: MutexGuard<'static, Ctl>) -> bool {
    let mut ran = false;
    loop {
        match c.step {
            Step::Idle => break,
            Step::Running => c = DONE.wait(c).unwrap_or_else(|p| p.into_inner()),
            Step::Deferred(grid_world) => {
                c.step = Step::Running;
                drop(c);
                unsafe { run_step(grid_world) };
                finish(ctl());
                ran = true;
                break;
            }
        }
    }
    ran
}

/// A flag stub found the flag set while a step is pending (`ret` = the site's return
/// address). On the game thread the step runs (or is waited for) now; other threads keep
/// vanilla's own wait.
extern "C" fn flag_observed(ret: u32) {
    if !on_game_thread() {
        stats(|s| s.other_thread += 1);
        return;
    }
    let t0 = std::time::Instant::now();
    let ran = complete(ctl());
    let us = t0.elapsed().as_micros() as u64;
    let inside = crate::patches::in_window();
    stats(|s| {
        let e = s.sites.entry(ret.wrapping_sub(5)).or_default();
        if ran {
            if inside {
                s.forced += 1;
                s.forced_us += us;
                e.0 += 1;
            } else {
                s.forced_outside += 1;
                e.1 += 1;
            }
        } else {
            s.waited_observe += 1;
            s.wait_us += us;
            e.2 += 1;
        }
    });
    check_step_time();
}

/// Render start (game thread, DeathMatch::Update before the particle render prep): a step
/// nothing forced goes to the worker thread while this client renders.
extern "C" fn render_start() {
    start();
}

fn start() {
    if !active() || !on_game_thread() {
        return;
    }
    let mut c = ctl();
    let Step::Deferred(grid_world) = c.step else { return };
    if check_mode() {
        drop(c);
        unsafe { check_step(grid_world) };
        return;
    }
    if inline_mode() {
        complete(c);
        check_step_time();
        return;
    }
    let (mut cw, mut csr) = (0u16, 0u32);
    // SAFETY: stores the game thread's x87 control word and MXCSR; the worker loads them.
    unsafe { std::arch::asm!("fnstcw word ptr [{0}]", "stmxcsr dword ptr [{1}]", in(reg) &mut cw, in(reg) &mut csr) };
    if c.worker.is_none() {
        let (tx, rx) = std::sync::mpsc::channel::<(u32, u16, u32)>();
        let spawned = std::thread::Builder::new().name("noita-online-box2d".into()).spawn(move || {
            for (grid_world, cw, csr) in rx {
                // SAFETY: the game thread does not touch Box2D until it joins (flag stubs and
                // DeathMatch::Update wait); same floating-point state as the game thread.
                unsafe {
                    std::arch::asm!("fldcw word ptr [{0}]", "ldmxcsr dword ptr [{1}]", in(reg) &cw, in(reg) &csr);
                    run_step(grid_world);
                }
                finish(ctl());
            }
        });
        if spawned.is_ok() {
            c.worker = Some(tx);
        }
    }
    let sent = c.worker.as_ref().map(|tx| tx.send((grid_world, cw, csr)).is_ok()).unwrap_or(false);
    if sent {
        c.step = Step::Running;
        stats(|s| s.worker += 1);
    } else {
        // No worker: run it here, as `inline_async` did.
        complete(c);
        check_step_time();
    }
}

/// Wait for the step (DeathMatch::Update entry, window entry, shutdown, next submit); a step
/// still deferred runs here.
pub fn join() {
    if !active() || !on_game_thread() {
        return;
    }
    let t0 = std::time::Instant::now();
    let waited = ctl().step == Step::Running;
    let ran = complete(ctl());
    if waited || ran {
        let us = t0.elapsed().as_micros() as u64;
        stats(|s| {
            if ran {
                s.joined_inline += 1;
            } else {
                s.join_waits += 1;
                s.join_wait_us += us;
                s.join_wait_max = s.join_wait_max.max(us);
            }
        });
    }
    check_step_time();
}

/// New run (`no_next_run`): the old world's last step was joined at its teardown
/// (`world_teardown`); its slow-step report is dropped.
pub fn reset_run() {
    UNCHECKED.store(false, Ordering::Relaxed);
    *CHECK_BODIES.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// b2World (Box2D 2.2, built with doubles): the broad phase (+0x191dc) starts with its dynamic
/// tree {root, nodes, node count, capacity, free list, path, insertion count}, then the proxy
/// count (+0x1c), the move buffer, capacity and count (+0x20, +0x24, +0x28) and the pair
/// buffer, capacity and count (+0x2c, +0x30, +0x34) (`UpdatePairs` FUN_009d09d0). Tree nodes are
/// 0x38 bytes: next / parent +0x24, height +0x30 (`AllocateNode` FUN_009b7590).
const W_BROADPHASE: u32 = 0x191dc;
const BP_PROXY_COUNT: u32 = 0x1c;
const BP_MOVE_COUNT: u32 = 0x28;
const BP_PAIR_COUNT: u32 = 0x34;
const TREE_NODE_SIZE: u32 = 0x38;
const NODE_NEXT: u32 = 0x24;
const NODE_HEIGHT: u32 = 0x30;
/// b2World: contact count (contact manager), body list (next body +0xb8, uid +0x110), joint
/// count, `m_inv_dt0` (double).
const W_CONTACT_COUNT: u32 = 0x1921c;
const W_BODY_LIST: u32 = 0x1922c;
const W_JOINT_COUNT: u32 = 0x19238;
const W_INV_DT0: u32 = 0x19260;
const BODY_NEXT: u32 = 0xb8;
const BODY_UID: u32 = 0x110;
/// The 64-bit uid counter that `CreateBody` (FUN_009bf120) and `CreateJoint` (FUN_009bf3d0)
/// give each new body / joint without one (1 in a fresh process).
const B2_UID_COUNTER: u32 = 0x0115_3308;

/// New run: the b2World is not rebuilt by a new game (the GridWorld keeps it; the game start
/// `FUN_006afaa0` only fetches it again into `0x01207414`), and neither is the static body at
/// (0, 0) that world creation makes in a fresh process (the first body, uid 1, its proxy the
/// tree's node 0 and root). The broad phase's dynamic tree keeps every other node of the old
/// world on its free list, in the order each client's last frames freed them. Proxy ids come
/// from that list, the broad phase creates contacts in the order of their proxy-id pairs
/// (`UpdatePairs` sorts them), which is the order the solver meets them, and tree queries
/// (`QueryAABB`) walk the tree the ids shape: run 1 of a restart differed in body motion from
/// the first contacts on (seed 98765432: frame 17). With no joint or contact left, the free
/// nodes are relinked in index order (a fresh tree's free list after the live nodes, whatever
/// capacity it grew to), the move and pair buffers are emptied, `m_inv_dt0` is 0 as after
/// construction, and the uid counter continues after the live bodies' uids, as in a fresh
/// process after world creation. Returns what was done (client log).
///
/// # Safety
/// Game thread, between the old world's teardown and the new world's creation; `world` is the
/// b2World.
pub unsafe fn reset_world(world: u32) -> String {
    if world == 0 {
        return "box2d: no world".into();
    }
    let bp = world + W_BROADPHASE;
    let (joints, contacts) = (rd(world + W_JOINT_COUNT), rd(world + W_CONTACT_COUNT));
    if joints != 0 || contacts != 0 {
        return format!("box2d: NOT reset, {joints} joints and {contacts} contacts left");
    }
    let nodes = rd(bp + 4);
    let live = rd(bp + 8) as i32;
    let capacity = (rd(bp + 0xc) as i32).max(0);
    let next_of = |i: i32| (nodes + i as u32 * TREE_NODE_SIZE + NODE_NEXT) as *mut i32;
    let mut free = vec![false; capacity as usize];
    let mut n = 0;
    let mut i = rd(bp + 0x10) as i32;
    while i != -1 {
        if i < 0 || i >= capacity || free[i as usize] {
            return format!("box2d: NOT reset, bad free list at node {i}");
        }
        free[i as usize] = true;
        n += 1;
        i = *next_of(i);
    }
    if n + live != capacity {
        return format!("box2d: NOT reset, {n} free + {live} live nodes != capacity {capacity}");
    }
    let mut head = -1;
    for i in (0..capacity).rev() {
        if free[i as usize] {
            *next_of(i) = head;
            *((nodes + i as u32 * TREE_NODE_SIZE + NODE_HEIGHT) as *mut i32) = -1;
            head = i;
        }
    }
    *((bp + 0x10) as *mut i32) = head;
    // m_path and m_insertionCount (only incremented by this build's InsertLeaf): as after
    // inserting the live proxies into a fresh tree.
    let proxies = rd(bp + BP_PROXY_COUNT);
    *((bp + 0x14) as *mut u32) = 0;
    *((bp + 0x18) as *mut u32) = proxies;
    *((bp + BP_MOVE_COUNT) as *mut u32) = 0;
    *((bp + BP_PAIR_COUNT) as *mut u32) = 0;
    *((world + W_INV_DT0) as *mut f64) = 0.0;
    let mut uid_next = 1u64;
    let mut body = rd(world + W_BODY_LIST);
    let mut bodies = 0;
    while body != 0 && bodies < 1 << 20 {
        let uid = rd(body + BODY_UID) as u64 | (rd(body + BODY_UID + 4) as u64) << 32;
        uid_next = uid_next.max(uid + 1);
        bodies += 1;
        body = rd(body + BODY_NEXT);
    }
    let uid_old = rd(B2_UID_COUNTER) as u64 | (rd(B2_UID_COUNTER + 4) as u64) << 32;
    *(B2_UID_COUNTER as *mut u32) = uid_next as u32;
    *((B2_UID_COUNTER + 4) as *mut u32) = (uid_next >> 32) as u32;
    format!(
        "box2d: reset ({bodies} bodies and {live} tree nodes kept, {n} free of {capacity}; uid counter {uid_old} -> {uid_next})"
    )
}

/// The grid update's join of the last step (ECX = GridWorld+0x4f8): normally DeathMatch::Update
/// joined it already.
extern "fastcall" fn grid_join(handle: u32) {
    join();
    let f: extern "fastcall" fn(u32) = unsafe { std::mem::transmute(JOB_JOIN_FN as usize) };
    f(handle);
}

extern "C" fn frame_join() {
    join();
    unsafe { check_bodies_unchanged() };
    stats_frame();
}

/// `box2d_freeze`: report a slow step with the next input (game thread, after a step).
fn check_step_time() {
    if UNCHECKED.swap(false, Ordering::Relaxed) && unsafe { crate::patches::box2d_step_slow() } {
        crate::BOX2D_STEP_SLOW.store(true, Ordering::Relaxed);
    }
}

// --- diagnostics ---------------------------------------------------------------------------

/// `NOITA_ONLINE_B2CHECK=1`: the step runs at the render start on the game thread, and the run
/// checks that it changes no RNG (render bank: nothing may draw from any bank) and that
/// nothing changes a body between the render start and the next DeathMatch::Update (what
/// the worker's step overlaps).
fn check_mode() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("NOITA_ONLINE_B2CHECK").is_some());
    *ON
}

/// `NOITA_ONLINE_B2INLINE=1`: the step runs at the render start on the game thread, without
/// checks: the same simulation as the worker (same trajectory), for measuring the overlap.
fn inline_mode() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("NOITA_ONLINE_B2INLINE").is_some());
    *ON
}

/// Body hash right after the render-start step ([`check_mode`]).
static CHECK_BODIES: Mutex<Option<(u32, u64)>> = Mutex::new(None);

unsafe fn check_step(grid_world: u32) {
    let before = crate::patches::rng_fingerprint();
    complete(ctl());
    check_step_time();
    let after = crate::patches::rng_fingerprint();
    let bodies = crate::patches::bodies_fingerprint(grid_world);
    *CHECK_BODIES.lock().unwrap_or_else(|p| p.into_inner()) = Some((grid_world, bodies));
    stats(|s| {
        s.checked += 1;
        if before != after {
            s.rng_changed += 1;
            s.notes.push(format!("rng f={}", frame()));
        }
    });
}

unsafe fn check_bodies_unchanged() {
    let Some((grid_world, bodies)) = CHECK_BODIES.lock().unwrap_or_else(|p| p.into_inner()).take() else { return };
    if crate::patches::bodies_fingerprint(grid_world) != bodies {
        stats(|s| {
            s.bodies_changed += 1;
            s.notes.push(format!("bodies f={}", frame()));
        });
    }
}

fn frame() -> u32 {
    unsafe {
        let gg = rd(crate::patches::GAME_GLOBAL_PTR);
        if gg != 0 {
            rd(gg)
        } else {
            0
        }
    }
}

/// `NOITA_ONLINE_B2STATS=1` (or [`check_mode`]): counters, written to
/// `$NOITA_ONLINE_OUT/box2d_<instance>.txt` every 600 frames.
#[derive(Default)]
struct Stats {
    deferred: u64,
    /// Steps a flag test inside the window ran (and their µs).
    forced: u64,
    forced_us: u64,
    /// Steps a flag test outside the window ran (no window exit since the submit).
    forced_outside: u64,
    /// Steps sent to the worker.
    worker: u64,
    /// Flag tests on the game thread that waited for the worker.
    waited_observe: u64,
    wait_us: u64,
    /// Flag tests on other threads while a step was pending.
    other_thread: u64,
    /// DeathMatch::Update / window-entry joins that waited, and deferred steps they ran.
    join_waits: u64,
    join_wait_us: u64,
    join_wait_max: u64,
    joined_inline: u64,
    checked: u64,
    rng_changed: u64,
    bodies_changed: u64,
    /// Per site: (forced in window, forced outside, waits).
    sites: std::collections::BTreeMap<u32, (u32, u32, u32)>,
    notes: Vec<String>,
}

static STATS: Mutex<Option<Stats>> = Mutex::new(None);

fn stats_on() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("NOITA_ONLINE_B2STATS").is_some() || check_mode());
    *ON
}

fn stats(f: impl FnOnce(&mut Stats)) {
    if !stats_on() {
        return;
    }
    let mut g = STATS.lock().unwrap_or_else(|p| p.into_inner());
    f(g.get_or_insert_with(Stats::default));
}

fn stats_frame() {
    if !stats_on() {
        return;
    }
    let f = frame();
    if f == 0 || f % 600 != 0 {
        return;
    }
    let g = STATS.lock().unwrap_or_else(|p| p.into_inner());
    let Some(s) = g.as_ref() else { return };
    let Ok(out) = std::env::var("NOITA_ONLINE_OUT") else { return };
    let inst = std::env::var("NOITA_ONLINE_INSTANCE").unwrap_or_default();
    let mut line = format!(
        "f={f} deferred={} worker={} forced={} forced_us={} forced_outside={} waited_observe={} wait_us={} other_thread={} join_waits={} join_wait_us={} join_wait_max={} joined_inline={} checked={} rng_changed={} bodies_changed={} sites=",
        s.deferred,
        s.worker,
        s.forced,
        s.forced_us,
        s.forced_outside,
        s.waited_observe,
        s.wait_us,
        s.other_thread,
        s.join_waits,
        s.join_wait_us,
        s.join_wait_max,
        s.joined_inline,
        s.checked,
        s.rng_changed,
        s.bodies_changed
    );
    for (site, (a, b, c)) in &s.sites {
        line.push_str(&format!(" {site:08x}:{a}/{b}/{c}"));
    }
    if !s.notes.is_empty() {
        line.push_str(&format!(" notes={}", s.notes.join(",")));
    }
    use std::io::Write;
    if let Ok(mut file) =
        std::fs::OpenOptions::new().create(true).append(true).open(std::path::Path::new(&out).join(format!("box2d_{inst}.txt")))
    {
        let _ = writeln!(file, "{line}");
    }
}
