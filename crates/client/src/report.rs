//! State reports are hashed off the game thread. Hashing a 512x512 chunk takes ~1.5 ms;
//! inline, that cost was part of the simulation step of every frame that hashed a chunk.
//! [`submit`] takes the report (chunk list, entity digest, positions) at the end of the
//! simulation window, and a worker thread hashes the chunks and sends the report while this
//! client renders. Cells only change inside the simulation window, so the worker sees
//! exactly the state the step left. [`join`] runs at the next window entry, before anything
//! can modify the world again.

use crate::net::Shared;
use crate::world;
use protocol::{ClientMsg, EntityPos, CH_RELIABLE};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// One state report whose chunk hashes are still to be computed.
pub struct Job {
    pub shared: Arc<Shared>,
    pub frame: u32,
    /// From [`world::loaded_chunks`].
    pub chunks: Vec<(u32, usize)>,
    pub materials_begin: usize,
    pub entities: Option<u64>,
    pub positions: Vec<EntityPos>,
}

static QUEUE: Mutex<Option<Sender<Job>>> = Mutex::new(None);
/// Submitted reports not yet sent.
static PENDING: Mutex<u32> = Mutex::new(0);
static SENT: Condvar = Condvar::new();
static LAST_HASH_US: AtomicU32 = AtomicU32::new(0);

/// Queue a report for the worker (started on first use; without a worker it runs here).
/// Reports without chunks (most frames of the staggered hash) are sent here directly.
///
/// # Safety
/// Game thread, at the end of the simulation window: the chunks must stay loaded and
/// unchanged until [`join`] returns.
pub unsafe fn submit(job: Job) {
    if job.chunks.is_empty() {
        join(); // reports go out in frame order
        return send(job, Vec::new());
    }
    *PENDING.lock().unwrap_or_else(|p| p.into_inner()) += 1;
    let mut queue = QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    if queue.is_none() {
        let (tx, rx) = channel::<Job>();
        let worker = std::thread::Builder::new().name("noita-online-hash".into()).spawn(move || {
            for job in rx {
                // SAFETY: see `submit`.
                unsafe { run(job) };
            }
        });
        if worker.is_ok() {
            *queue = Some(tx);
        }
    }
    match queue.as_ref() {
        Some(tx) => {
            if let Err(e) = tx.send(job) {
                run(e.0);
            }
        }
        None => run(job),
    }
}

unsafe fn run(job: Job) {
    let t0 = Instant::now();
    let chunks = world::hash_chunks(&job.chunks, job.materials_begin);
    LAST_HASH_US.store(t0.elapsed().as_micros().min(u32::MAX as u128) as u32, Ordering::Relaxed);
    send(job, chunks);
    *PENDING.lock().unwrap_or_else(|p| p.into_inner()) -= 1;
    SENT.notify_all();
}

fn send(job: Job, chunks: Vec<protocol::ChunkHash>) {
    let msg = ClientMsg::StateHash { frame: job.frame, entities: job.entities, chunks, positions: job.positions };
    job.shared.send(CH_RELIABLE, &msg);
}

/// Wait until every submitted report has been hashed and sent.
pub fn join() {
    let mut pending = PENDING.lock().unwrap_or_else(|p| p.into_inner());
    while *pending > 0 {
        pending = SENT.wait(pending).unwrap_or_else(|p| p.into_inner());
    }
}

/// Duration of the last chunk hash (µs).
pub fn last_hash_us() -> u32 {
    LAST_HASH_US.load(Ordering::Relaxed)
}
