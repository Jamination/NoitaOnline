# Determinism patches

Noita Online keeps clients in sync by deterministic lockstep: every client runs the same
simulation from the same seed and the same per-frame inputs. Vanilla Noita is not
deterministic across machines. Its simulation depends on wall-clock time, thread timing,
the local camera, uninitialised memory and the date. The client DLL patches the running
`noita.exe` in memory (build `Jan 25 2025`, PE timestamp `0x6794ee3c`) to remove each of
those dependencies. Nothing on disk is modified.

All patches are listed in `PATCHES` in `crates/client/src/patches.rs`. Each patch checks
the original bytes before it writes and fails closed if they differ. For experiments only,
`NOITA_ONLINE_PATCH_OFF=name,name` skips patches by name; normal play needs every patch.

## The simulation window

The mod opens a *simulation window* around the engine's world update:
- on entry, `no_sim_enter` / the `early_window` hook;
- on exit, `no_sim_leave` in `OnWorldPostUpdate`.

Inside the window:
- **RNG:** every engine RNG (global LCG, platform RNGs, particle RNG, `rand`) runs on the
  *simulation bank*. Outside it, a separate *render bank* is used, so per-client rendering
  can consume randomness freely.
- **Camera:** the camera is the shared *simulation camera* (the first live player), set
  with force. Outside the window, each client's render camera follows its own player.
- **Chunk loading and body freezing** follow every player (the *foci*) rather than the
  local camera.

## Patch table

| Name | Root cause | Fix |
|---|---|---|
| `rand` | msvcr120 `rand` keeps per-thread state, and `srand(time)` seeds it. | Imports are replaced with one process-wide LCG; `srand` is ignored. |
| `streamclock` | The streaming throttle runs on wall-clock time. | Uses game time (`frame / 60`). |
| `chunkwait` | Generated chunks are integrated as soon as their worker job finishes, which depends on timing. | Gate integrated with a delay of 8 frames: a job is integrated exactly 8 frames after submission; the frame waits for it if needed. |
| `threaded_chunkgen` | (performance) Chunk generation costs 17–36 ms per chunk. | Stays on worker threads through an unpatched copy of the async submit. Its only shared-RNG write (the cell constructor's LCG store) is removed. |
| `threaded_grid` | (performance) | Falling-sand tiles run in parallel deterministically: each tile has its own RNG, and the checkerboard passes never let tiles race. They keep the pool. |
| `threaded_render` | (performance) Grid rendering costs 7–10 ms per frame inline. | Renders on worker threads outside the window. It is joined before the RNG banks swap, so it never overlaps the simulation. |
| `inline_group`, `inline_async` | Other pool jobs (particles, Box2D step, pathfinding, entity jobs) race on shared RNG state and overlap the next frame's simulation. | Run synchronously at submission. |
| `particle_init` | Particle structs keep uninitialised heap bits, which select the collision world. | Cleared fully. |
| `camera_loads` | Chunks load around each client's camera and look-ahead. | Camera-driven loads are removed. The mod streams chunks around every player (`no_stream_focus`) and unloads far chunks deterministically (`no_unload_far`). |
| `fixed_dt` | `DeathMatch::Update` feeds wall-clock `dt` into the simulation. | `dt = 1/60`. |
| `focus_freeze` | Far physics bodies freeze by distance to the local camera. | Distance to the nearest player, on a deterministic schedule. |
| `cosmetic_particles` | Cosmetic particles collide with a snapshot of the grid around the local camera, then land as real cells. | They collide with the real grid. |
| `clock` | Weather re-initialises on hour and day changes; snow, holidays and `GameGetDateAndTime*` read the local date. | `GetLocalTime`, `GetSystemTime`, `_time64` and `_localtime64` return the server's session start time plus game time, in UTC. |
| `early_window` | Vanilla `data/scripts/init.lua` emits rain in its `OnWorldPreUpdate`, which runs before the mod's, with the local camera and the render RNG. | The window opens natively before any mod's `OnWorldPreUpdate`. |
| `freecam_static` | The free camera moves itself (keyboard, easing) inside the window. | Removed; only the mod moves the camera. |
| `cell_flash` | The flash flag on a cell (`+0xc`) keeps its tile active, but only the render job cleared it, and only for on-screen cells. | The liquid update expires the flag itself; the render only reads it. |
| `cell_emitters` | Material particle effects (sparkles over magic liquids, acid, lava) are emitted from on-screen cells found by the render job, then use the simulation particle RNG. | No longer emitted. These effects are purely cosmetic and are not drawn. |
| `box2d_freeze` | With the magic number `BOX2D_FREEZE_STUCK_BODIES` (on by default), the body→grid update (`FUN_00718d50`) makes the most-stuck awake body static whenever the previous Box2D step took longer than `BOX2D_THREAD_MAX_WAIT_IN_MS` (12 ms) of wall-clock time (`b2World+0x19270`). Which frames cross 12 ms depends on machine load. | The time test is skipped, so the load-dependent freeze never runs. (Preventive: step times stayed under 11 ms in the runs measured so far.) |
| `free_solids_order` | Bodies that lost cells in a frame (`mFreeSolids`, a `std::map<b2Body*, …>`) are rebuilt in `b2Body*` heap-address order (`FUN_0071d020`). Rebuilding removes the old body from the body vector and appends the new ones, so after an explosion broke several bodies the new bodies sat at client-specific vector positions. The next frame's body→grid pass places bodies in vector order, so displaced cells became particles in a different order (seed 1717171717, f=1382). | Rebuilt in body-vector order (the entry whose body has the lowest vector index goes first). |
| `pathfinding_refresh` | With `PATHFINDING_DISTANCE_FIELD_ENABLED` (on by default), the pathfinding job (`FUN_00861a50`) keeps refreshing the node clearance field of the 2×2 pathfinding chunks around the camera centre: one chunk every two frames, then a new cycle. The search reads this clearance as the room a mover has, and nothing else computes it. The job runs before the simulation window, so it used each client's render camera. Clients refreshed different chunks at different frames, and an enemy searching in between got a different path (seed 1404389217, f=1554: a rat's next path node). | Each refresh cycle centres on a player in turn (slot order), using the focus centres from the previous window. |

Seeds are set at world init and at frame 1 by `no_fix_seeds`:
- the platform RNGs, the global LCG and PM, the particle RNG and the `rand` state;
- the simulation RNG bank.

All of them are derived from the session seed.

## Mod-level rules

- The engine's own player entity is an inert input proxy. Each player's character is
  driven only by lockstep ticks (`files/players.lua`).
- Every simulation-affecting Lua call happens inside the window, in the same order on
  every client. Examples: world events in the bot scenario, inventory and wand edits
  (`files/inventory.lua`), `GameSetCameraPos` for the simulation camera.
- Rendering-only Lua (the local camera, the editor UI) runs outside the window.
- Pausing is not supported in a session. The vanilla inventory key is not transmitted;
  the lockstep wand editor replaces it.
- All clients must run the same mods (mod hash) and game build; the server checks both.

## Verification

Each client hashes every loaded chunk's material cells once every 60 frames, staggered by
chunk index. It also reports player and enemy positions plus an entity digest. The server
compares all clients' reports per frame (`hashes.jsonl`). Setting
`NOITA_ONLINE_HASH=full` reports full snapshots every 60 frames.

Diagnostics used to find the root causes above, enabled by environment variables:
- `NOITA_ONLINE_DENSE=a-b`: per-frame entity dumps. With `NOITA_ONLINE_ENTLOG=id,...` it
  logs those entities' transforms each dense frame; `NOITA_ONLINE_ENTLOG_COMPS=1` adds every
  component's scalar members and the children.
- `NOITA_ONLINE_RNGLOG`: RNG states and cameras at window entry and exit, plus the last
  Box2D step's wall time (`b2ms`) and the pathfinding clearance-refresh centre and state
  (`pfc`, `pfs`).
- `NOITA_ONLINE_PTRACE`: call chains of particle creation; also every body→grid placement
  (body pose, velocity, vector key) and particle construction (position, velocity).
- `NOITA_ONLINE_DUMP`: chunk and cell dumps. With `NOITA_ONLINE_DUMP_RECT` it adds
  particles, the GridParticles side lists, the render material snapshot hash and the
  physics bodies; `NOITA_ONLINE_DUMP_ENTRY=1` also dumps at window entry
  (`dump_<inst>_<frame>e.*`).
- `NOITA_ONLINE_PROF` / `NOITA_ONLINE_SAMPLE`: profilers.
