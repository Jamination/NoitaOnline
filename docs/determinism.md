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
- `NOITA_ONLINE_DENSE=a-b`: per-frame entity dumps.
- `NOITA_ONLINE_RNGLOG`: RNG states and cameras at window entry and exit.
- `NOITA_ONLINE_PTRACE`: call chains of particle creation.
- `NOITA_ONLINE_DUMP`: chunk and cell dumps.
- `NOITA_ONLINE_PROF` / `NOITA_ONLINE_SAMPLE`: profilers.
