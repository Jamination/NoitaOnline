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
- **Camera:** the camera is the shared *simulation camera* (the first live player's view,
  see below), set with force, and the engine's camera lock is then cleared on every client,
  as vanilla has it at that point of a frame. Outside the window, each client's render
  camera is the camera Noita computed in the window for its own player (`mDesiredCameraPos`
  of its `PlatformShooterPlayerComponent`, the only one with `center_camera_on_this_entity`).
- **Views:** every player's view is the camera its own client rendered. Each input carries
  the position of the client's render camera (the camera it drew the frame before the input
  was captured, rounded to whole px) in a trailer the client runtime adds (`no_send_input`)
  and strips (`no_wait_tick`) before Lua sees the input: one flags byte (bit 0: Box2D slow
  step, `box2d_freeze`; bits 1–2 / 3–4: x / y as unchanged, i8 or i16 delta from the
  previous input's, or i32 absolute), then the fields in front of it. A still camera costs 1
  byte, a moving one usually 3. Every client decodes every player's camera from the ticks
  in frame order, so the views are identical everywhere (`views.rs`). A view lags its
  client's screen by the input delay plus one frame (2 frames on a LAN). Before a player's
  first camera arrives (the first frames) the view is centred on the player; a player who
  left or died keeps the last camera it sent. A view's rect is vanilla's camera int rect
  (GridWorld+0x498: 431×242, what the render snapshots): the simulation camera's own rect
  for view 0, moved by the view centre's offset for the others. The camera-dependent
  patches below (`camera_loads`, `focus_freeze`, `particle_views`, `camera_bound`,
  `cell_emitters`, `rain_views`, `pathfinding_refresh`, `grid_cap`, the camera readers) all
  use these views.
- **Chunk loading** runs vanilla's camera loads for every player's view, and **body
  freezing** follows every player's view, rather than the local camera. Chunk eviction
  measures from the players (the *foci*), as below.

## Patch table

| Name | Root cause | Fix |
|---|---|---|
| `rand` | msvcr120 `rand` keeps per-thread state, and `srand(time)` seeds it. | Imports are replaced with one process-wide LCG; `srand` is ignored. |
| `streamclock` | The streaming throttle runs on wall-clock time. | Uses game time (`frame / 60`). |
| `chunkwait` | Generated chunks are integrated as soon as their worker job finishes, which depends on timing. | Gate integrated with a delay of 8 frames: a job is integrated exactly 8 frames after submission; the frame waits for it if needed. The wait happens at the start of the frame, outside any engine lock (`chunk_prewait`, judged by the frame the world update will run as, f + 1). A bounded in-lock wait inside the integration only catches jobs nothing pre-waited. If such a job is still unfinished after 5 s, it is integrated whenever it finishes; the state report counts these cases (`timed out`). |
| `threaded_chunkgen` | (performance) Chunk generation costs 17–36 ms per chunk. | Stays on worker threads through an unpatched copy of the async submit. Its global-LCG write (the cell constructor's store) is removed; its cell RNG draws are `chunk_rng`'s. |
| `chunk_rng` | Chunk jobs (`FUN_0073b040`: generation, or loading a chunk from the save) build cells with the real GridWorld, and the gas, fire and lifetime-liquid constructors (`FUN_0070a4a0`, `FUN_00709ef0`, `FUN_0070ae10`) draw the GridWorld's RNG (+4) for flags and lifetimes, on a worker thread, while the simulation draws the same RNG on the game thread. Which values each side got depended on thread timing, so reloaded gas, fire and liquid cells got client-specific flags and lifetimes (invisible to the material hash until they moved or expired differently) and the simulation's cells got client-specific values (seed 98765432 apart scene: a player returning home at f=2700 reloaded its chunks; first bad chunk at f=2893). | The chunk job runs through a wrapper that gives the thread its own LCG stream for the job, seeded from the session seed and the chunk's coordinates; the four constructor draws use it while a chunk job runs on their thread and the world's RNG everywhere else (game thread, grid tiles with their own RNG). |
| `chunk_frame` | In the same chunk jobs, the fire and lifetime-liquid constructors (`FUN_00709ef0` @0x00709f6e, `FUN_0070ae10` @0x0070ae99) read the creation frame through the world's vtable+0x10, i.e. the GridWorld frame counter (+0x4fc) that the game thread increments at the start of every grid update. On a worker thread that is whatever frame the job happened to reach the cell in, so a generated or reloaded fire cell's age and a liquid's lifetime start (hence when it burns out or expires) depended on thread timing. Latent: no failing run traced to it. | The submit stub records the grid frame of each chunk job at submission (game thread, `CHUNKGEN_SUBMIT_CALL`); the job wrapper hands it to its thread, and the two constructor reads return it while a chunk job runs on their thread, the world's counter everywhere else. |
| `threaded_grid` | (performance) | Falling-sand tiles run in parallel deterministically: each tile has its own RNG, queues its world edits (cells to create or remove, particles, explosions, …) in tile-local vectors that the game thread replays tile by tile in active-list order, and the checkerboard passes never let tiles race. The explosion path did not hold to this (`tile_explosions`). They keep the pool. |
| `tile_explosions` | Burning cells (`FUN_0070fb10`), burning physics bodies (CSolidCell update `FUN_0070e060`) and reactions (`FUN_0070ebf0`) explode only while the world's explosion count (vtable+0x84) is 0. A tile's count (`FUN_0072b430`) is one process-wide counter (`0x01207e7c`) that every tile's queued explosion (vtable+0x80, `FUN_0072b3d0`, `lock xadd`) increments and the flush resets, so whether a cell exploded depended on whether a tile running at the same time on another thread had queued one first; the burning check also draws the tile RNG only when the count is 0, so the tile's later cells diverged as well. The sites also write the explosion's radius and friends into the material's (reaction's) shared `ConfigExplosion` before queuing a copy of it (two tiles could copy each other's values; every explosion inherited what the last one, in thread order, left there), and a burning body picks its explosion cells with the calling thread's Park–Miller RNG (`FUN_0063d1c0`, thread-local, state = whatever that worker ran before). Seed 837996934 with 4 players (bomb phase of cycle 5) diverged inside the grid update in 4 of 5 runs, first mismatch at f=19800–20807 and a different client each time: at its exit either every RNG and the Box2D bodies differed with six chunks over ~1500 px at once (an explosion on some clients only), or one chunk differed with every RNG equal (a cell on the other branch). | A tile's count is the explosions queued in the frame's earlier passes, sampled by the game thread at the head of each checkerboard pass (`0x00728180`, no tile running), plus the tile's own queue: no tile sees a tile of its own pass. On a tile, the three sites' config pointer loads return a thread-local copy of the config, so they write and queue their own copy and the shared config is never written by a tile job. On a tile, the body explosion's RNG is a Park–Miller state seeded from the tile's RNG state and the burning cell's position. Game-thread callers (the real GridWorld) are unchanged. |
| `threaded_render` | (performance) Grid rendering costs 7–10 ms per frame inline. | Renders on worker threads outside the window. It is joined before the RNG banks swap, so it never overlaps the simulation. |
| `threaded_pathfinding` | (performance) The pathfinding update's per-tile jobs (`FUN_00864590`, about 1.4 ms per frame inline). | They stay on worker threads. Each job recomputes the node flags of its own grid tile from that tile's cell summary only, with no RNG and no allocation, and `FUN_00864590` joins them all before anything else runs. |
| `inline_group`, `inline_async` | Other pool jobs (particles, render jobs, pathfinding search, entity jobs) race on shared RNG state and overlap the next frame's simulation. | Run synchronously at submission (the Box2D step: `box2d_overlap`). |
| `box2d_overlap` | (performance) Vanilla submits the Box2D step (`FUN_009a48d0` → `b2World::Step` `FUN_009c0d00`) to a worker at the end of the grid update, with the "step in progress" flag `b2World+0x191d8` set; the step clears the flag, the next grid update joins the job (GridWorld+0x4f8). Meanwhile the entity systems read bodies unchecked (racy), and every structural Box2D change first tests the flag: 97 `mov al,[reg+0x191d8]; test al,al` sites, Noita's waiting until it clears, Box2D's own refusing the call. Run inline at the submit (as `inline_async` did) it cost 0.3–0.6 ms per frame on the game thread. | The submit keeps vanilla's flag and records the step without running it. The step runs at the first of: a flag test on the game thread (a stub at all 97 sites runs it first: a worker finishing right before that code looks), the render start (DeathMatch::Update's particle render prep call @0x006b3a97, after the player spawn: it goes to a worker thread while the client renders: a worker that had not started before the update ended), the next DeathMatch::Update (joined). All three are deterministic points; the step draws no RNG (contact listener only records PreSolve contacts; default contact filter), checked with `NOITA_ONLINE_B2CHECK`. `box2d_freeze` measures the step when it finishes. |
| `particle_init` | Particle structs keep uninitialised heap bits, which select the collision world. | Cleared fully. |
| `camera_loads` | Chunks load around each client's camera and look-ahead (`FUN_00720130`: whenever the camera's int rect changes, the rect snapped toward zero to 64 px and grown by 64 px, plus a copy moved by 4 × the snapped motion, ±512 px, requested through RequestLoad @0x00720210 / @0x00720290); the engine evicts chunks on wall-clock time around the camera. | The camera's RequestLoad calls go through a stub: before the first simulation window (world creation's start camera, identical everywhere) they stay vanilla, from then on they are skipped. Inside the window the mod makes the same loads for every player's view (`no_stream_views`, after the views are set): view 0 is the simulation camera (player 1's view), view k its rect moved to player k's view, each with its own last rect (all starting from the one world creation left), so each player's look-ahead follows its own camera's motion. Chunks are evicted with vanilla's rule per player on a frame schedule (`no_evict_chunks`): every STREAMING_FREQUENCY s, a player owning (nearest focus centre) more than STREAMING_CHUNK_TARGET loaded chunks loses its farthest one outside the keep-alive list and its focus rect (all above three times the target; the interval halves at twice the target). Until perf8 the loads were ±400×300 px focus rects around the players without the start camera's loads: a 1-player session then kept a different chunk set than vanilla for the whole bot run on every seed (chunk (1,1), underground, instead of (-2,-1)), up to twice vanilla's active grid tiles. |
| `fixed_dt` | `DeathMatch::Update` feeds wall-clock `dt` into the simulation. | `dt = 1/60`. |
| `focus_freeze` | Far physics bodies freeze by distance to the local camera. | Distance to the nearest view centre, on a deterministic schedule. |
| `particle_views` | Vanilla simulates view-bound particles only on screen: creation with the view-bound flag returns early outside the camera rect (GridWorld+0x498 origin, render snapshot size) in `FUN_007168c0` (cell→particle), `FUN_00716af0` and `FUN_00716ce0`; cosmetic particles (+0x6a bit 1) collide with the render's material snapshot of the screen (`FUN_00712390`) and land when they leave it. Inside the window the camera is player 1's, and the snapshot belongs to each client's own render (outside the window), so particles landed differently per client (re_camleak P14) and only player 1's view got view-bound particles. | "On screen" means inside some player's view (`views.rs`: the view's 431×242 int rect). The three creation culls test that; a cosmetic particle's lookup reads the world as the snapshot shows it (material id, −1 for no cell or a cell the render does not draw, vtbl+0x10 == 4) inside a view and takes vanilla's leave-the-screen path outside every view. Until perf9 a view was the player's position ± 216+64 × 121+64 px (a 64 px margin for the camera's look-ahead): twice vanilla's area. |
| `camera_bound` | CameraBoundSystem tests entities against the one camera (player 1 inside the window): farther than `distance` → frozen/killed (`FUN_00a5bd10`), `max_count` cull keeps those nearest the camera (`FUN_00a5bbe0` → `std::sort` `FUN_00d68150`), frozen records respawn only in the 5×5 cells around it and off its screen (`FUN_00a5be10` → `FUN_00a590f0`); `FUN_00a5c730` respawns around the render camera outside the window. | Distances are to the nearest view centre; the respawn grid runs around every view centre (slot order, each cell once); the sort is the engine's for ≤1 view and an exact MSVC-sort port with the nearest-view key for more; with 2+ views entities in unloaded chunks freeze and records never respawn into one; `FUN_00a5c730` is skipped once views exist (`cambound.rs`). |
| `clock` | Weather re-initialises on hour and day changes; snow, holidays and `GameGetDateAndTime*` read the local date. | `GetLocalTime`, `GetSystemTime`, `_time64` and `_localtime64` return the server's session start time plus game time, in UTC. |
| `early_window` | Vanilla `data/scripts/init.lua` emits rain in its `OnWorldPreUpdate`, which runs before the mod's, with the local camera and the render RNG. | The window opens natively before any mod's `OnWorldPreUpdate`. |
| `freecam_static` | The free camera moves itself (keyboard, easing) inside the window. | Removed; only the mod moves the camera. |
| `cell_flash` | The flash flag on a cell (`+0xc`) keeps its tile active, but only the render job cleared it, and only for on-screen cells. | The liquid update expires the flag itself; the render only reads it. |
| `cell_emitters` | Material particle effects (`render_on_grid` ParticleEffects: sparkles and bubbles over magic liquids, acid, poison, radioactive liquid, cursed rock, …) are emitted from the on-screen surface cells the grid render job finds (`FUN_00724580` queue → `FUN_00724480` → `FUN_007171d0`), outside the window, in the order of this client's view; the particles then live in the shared GridParticles lists and draw the simulation particle RNG (seed 2046634918, f=1385). | The render no longer queues emitters. At window exit a worker thread scans the union of every player's view with the render's rule (drawn cell, material+0x28c set, snapshot pixel above empty) while the client renders (cells only change inside the window); at the next window entry (simulation RNG bank loaded) the game thread joins it and calls `FUN_007171d0` for each hit in row order (`cosmetic.rs`), i.e. what each player's render queued at the end of the previous frame. |
| `rain_views` | `GameEmitRainParticles` (vanilla `init.lua` weather) emits rain/snow in a band around the camera (`FUN_00c0e9f0`: GridWorld+0x498/+0x49c/+0x4a0), i.e. around player 1 inside the window. | With 2+ views the band is emitted around every player (the camera's int view rect moved by the player's offset), skipping a player whose view an earlier emitted band already covers from the same height or above (vanilla density where views overlap). |
| `box2d_freeze` | With the magic number `BOX2D_FREEZE_STUCK_BODIES` (on by default), the body→grid update (`FUN_00718d50`) freezes the most-stuck awake body (`FUN_00717aa0`) whenever the previous Box2D step took longer than `BOX2D_THREAD_MAX_WAIT_IN_MS` (12 ms) of wall-clock time (`b2World+0x19270`): a safety net for physics too heavy for the machine. Which frames cross 12 ms depends on machine load (seed 1717171717, f=1382). | The time test reads a flag set from the tick: each client measures its own step at window exit and reports a slow one in bit 0 of its next input's trailer (`no_send_input`, see *Views*; `no_wait_tick` strips the trailer before Lua sees the input). A tick in which any player reported a slow step freezes the most stuck body on every client, one input delay after the slow step. |
| `stuck_wake_aabb` | PlayerCollisionSystem (`FUN_00bfa850`): a player stuck in the ground for more than 10 frames gets a collision body, and every 10 frames the bodies in an AABB around it are woken (Box2D QueryAABB `FUN_009d0d40` @0x00bfb2f5, then `SetAwake(true)`). The code stores only one corner, `pos + collision_aabb_max + 3` (Box2D units), into lowerBound; upperBound (16 bytes) is never written and holds whatever earlier calls left on the game thread's stack. That leftover differed between clients (it depends on each client's own code paths, e.g. its local input on the proxy: human sessions only), so a chest rolling into the players at the spawn woke every sleeping body below and right of them on one client and none on the other (seed 98765432: pd_human2 f=1443, pd_human3 f=2382, pf4_human3 f=18976, px_human3 f=980; equal RNGs, slightly different body states). | A stub at the call gives the query the box the code evidently meant: lowerBound `pos + collision_aabb_min - 3`, upperBound the stored corner, i.e. the player's collision box grown by 3 px, with the engine's float/double arithmetic. |
| `free_solids_order` | Bodies that lost cells in a frame (`mFreeSolids`, a `std::map<b2Body*, …>`) are rebuilt in `b2Body*` heap-address order (`FUN_0071d020`). Rebuilding removes the old body from the body vector and appends the new ones, so after an explosion broke several bodies the new bodies sat at client-specific vector positions. The next frame's body→grid pass places bodies in vector order, so displaced cells became particles in a different order (seed 1717171717, f=1382). | Rebuilt in body-vector order (the entry whose body has the lowest vector index goes first). |
| `pathfinding_refresh` | With `PATHFINDING_DISTANCE_FIELD_ENABLED` (on by default), the pathfinding job (`FUN_00861a50`) keeps refreshing the node clearance field of the 2×2 pathfinding chunks around the camera centre: one chunk every two frames, then a new cycle (`FUN_008605e0` → `FUN_00860150`). The search reads this clearance as the room a mover has, and nothing else computes it. The job runs before the simulation window, so it used each client's render camera. Clients refreshed different chunks at different frames, and an enemy searching in between got a different path (seed 1404389217, f=1554: a rat's next path node). | The cycle centres on every player's view, using the view centres (cameras) from the previous window. With one player it is vanilla's single cycle. With more, every player runs its own cycle each frame in slot order (own copy of the coroutine state at `0x01208520` and, mid-chunk, of `FUN_00860150`'s chunk scratch), so every player's surroundings are refreshed at vanilla's rate. |
| `ragdoll_stains` | When a creature dies, its ragdoll images are blended with the creature's stains (`FUN_00abb7d0` → `FUN_00be8240`, overlay at `0x00abc082`). The overlay was the SpriteStains image at +0x20: the stain colours (+0x00) blurred with the left neighbour, written only by the stains texture upload (`FUN_00c21020`) from the render-side queue, and only for sprites drawn last frame. So each client blended in its own last *drawn* stains, all zero for a creature it never saw, and small ragdoll parts (one box fixture per run of equal-colour pixels, `FUN_009817d0`) got different fixtures and mass (seed 1786459272, f=5345). | The site passes a scratch image holding the upload's blur of the *current* stain colours, bit for bit as `FUN_00c21020` computes it (per channel (c + c_left) / (1 + a_left) in f32, ×255 truncated and clamped, `FUN_0044abc0`): vanilla's look from simulation state only (`ragdoll.rs`). It differs from vanilla only where vanilla's overlay lagged the stains (upload queue) or was empty (creature died off-screen). |
| `platform_frame` | The platform frame counter (PlatformDesktop+0x20, its vtable+0x60 getter) counts main-loop frames from before the world exists (62 of them in a `-gamemode 0` start), so it differs between clients by the time each spent before its session started, e.g. in menus. About 60 sites read it, simulation ones among them: the SpriteStains update runs for an entity when `(frame ^ entity_id) & 15 == 0` (a player: every 3rd frame) and draws the global LCG there; PathFindingComponent (`FUN_00bca030`) and animal AI (`FUN_00b750b0`) schedule work by it. | The counter is set to GameGlobal's frame + 2^20 by `no_fix_seeds` and at every window entry. The base is above any pre-session count, so the counter only jumps forward. |
| `analog_fly_rng` | With the option `gamepad_analog_flying` and a gamepad, the ControlsSystem (`FUN_00a8d7f0`) dithers the fly button from the analog trigger with one global-LCG step per frame. It runs only for an enabled ControlsComponent, i.e. the local input proxy, inside the window, so that client's simulation LCG ran ahead of the others'. | The dither uses its own LCG state (@0x00a8e8c1, 0x00a8e8d0); the dithered Fly bit is captured and sent as input. |
| `early_input` | (latency) The ControlsSystem writes the input it reads for the player during the world update, and the mod captured the input proxy in `OnWorldPreUpdate`, i.e. the input polled for the previous frame: every input waited one frame more than the network needs. | The system's component update calls (`FUN_00a8d7f0` from the update loop @0x00c523f7 and from its parallel job @0x00d8b863) go through a hook that skips the input proxy's component when it was already updated for this update; the `early_window` hook runs that update for the proxy right before the `OnWorldPreUpdate` dispatch, before the window opens (render camera, own RNG), with the frame counter at the coming update's number for its press stamps. Local input only; it never touches the simulation. |
| `inventory_gui` | Noita's inventory GUI (`InventoryGuiComponent`, update `FUN_00b7d8d0`) reads this client's keyboard and mouse and edits the inventory at once, inside the window: enabled for a client's own player only, it changed the simulation on that client alone (seed 1834914516: number key 2 equipped wand 2 on one client only). Its "inventory open" flag (`0x01222510`) is read by the simulation (item pickup refuses with the screen open). A full-inventory pickup (`FUN_00b906a0` → `FUN_00b811b0`) sets GameGlobal pause bit 2, after which every client runs only the "replace which item" screen (`FUN_00b813a0`) from its own input. | The engine's own GUI update is skipped for every player; the mod runs it for its own player after the window (`no_gui_update`), with the co-op player list (`[0x01204bc0]+0x58`) pointing at that player (the HUD is laid out per list index). While it runs, the GUI's simulation calls are recorded instead of executed: set active item `FUN_00b4ffa0`, take/put item `FUN_00b5eb00`/`FUN_00b5eb90`, `SendMessage<ThrowItem>` `FUN_00d0e670`, `SendMessage<ItemAboutToBeEaten>` `FUN_00d20850`, pick up `FUN_00b905f0`. The ops (entity ids, slots, message fields) travel in the input; every client replays them in the tick with the same engine calls (`no_gui_apply`). The open flag is cleared between GUI calls. `FUN_00b811b0` never pauses; the picker's own client runs the replace screen after the window (`no_gui_swap_update`), recorded the same way. |
| `grid_cap` | The grid update (`FUN_00726340`) simulates at most `GRID_MAX_UPDATES_PER_FRAME` (128) active tiles. Above that it keeps the 128 nearest to the GetBorders centre (`std::sort` `FUN_0095d050`, comparator `FUN_00725e60`) and defers the rest into a `rand`-shuffled refill list. Inside the window that centre is the simulation camera (player 1), so in busy frames every other player's sand, liquids and fire stopped updating. | The cap is 128 per view (the caller's `[ebp-0x64]`). Each tile goes to its nearest view, each view's tiles are ordered by vanilla's key around that view (MSVC-sort port; view 0's centre is exactly vanilla's, the others get the same 64 px snapping shift), and the views are interleaved rank-major, so each player keeps its nearest min(n, 128) tiles. With ≤1 view the update is unchanged. |
| `sprite_events` | Sprite animation events (`<Event>`, `max_distance` 500) fire only near the camera (SpriteSystem `FUN_00c1ab60`, SpriteAnimator `FUN_00c1b820`). Inside the window that is player 1, so other players' `throw_release` (throwing), `sprite_visible`, and their enemies' `jump_start` never fired. | Distance to the nearest view centre. |
| `ai_camera` | AnimalAI JobDefault (`FUN_00a25940`) idles until the animal has once been within `max_distance_to_cam_to_start_hunting` of the camera; JobHelpOtherEntity (`FUN_00a25f00`) paths to the camera centre. | Nearest view centre. |
| `pf_jump` | PathFinding (`FUN_00bca030`) allows jumps only within `max_jump_distance_from_camera`. | Nearest view centre. |
| `char_effects` | CharacterPlatforming (`FUN_00a7ea90`): jetpack/knockback effects (particles, the `jetpack` event, a liquid push with global-LCG draws) only within 700 px of the camera. | Nearest view centre. |
| `worm_bursts` | Worm (`FUN_00c44480`) and dragon (`FUN_00a53610`) ground-crossing particle bursts (global-LCG draws) only within 128 px of the camera. | Nearest view centre. |
| `camera_edge` | Camera-bound sprite emitters and torches (`FUN_00a5c610`, global-LCG draws) only run near the camera rect. | Near any view's camera-sized rect. |
| `verlet_cull` | Verlet objects (ropes, vines, cloth; `FUN_00c3a610`) outside the camera rect ±128/256 stop simulating and hide. | Inside any view-centred rect. |
| `emitter_views` | Non-cosmetic particle emitters (`FUN_00bc8bb0`) are culled by distance to the camera AABB (`FUN_00d13f10`). | Minimum distance over the view AABBs, same maths. |
| `pixel_scenes` | Pending pixel scenes (`FUN_0087ec40`) wait while intersecting the camera (no pop-in), i.e. only player 1's screen. | Wait while intersecting any view rect. |
| `telepathy` | The TELEPATHY effect (`FUN_00b015d0`) draws eyes over creatures in the camera rect. | The rect of the view nearest the effect's holder. |
| `screen_shake` | Shake strength (`Camera::AddShake` `FUN_006f2db0`, Lua `GameScreenshake`, laser hits `FUN_00b946a0`) is attenuated by distance from the camera rect, so inside the window every player got player 1's shake. The shake (camera+0x58) decays in the world update with two global-LCG draws per frame. | The shared shake is the strongest any view would feel (same RNG use everywhere). Each client also keeps its own shake, attenuated against its own view (`no_local_view`), and the render camera uses its offsets: they are swapped in at window exit and the shared ones back at the next window entry. |
| `fog_of_war` | The fog-of-war image (`FUN_00c42d30`, GameGlobal+0x2c) is stepped toward the light only around the camera, and cleared by the biome amount only around the first `player_unit`. | Both loops run once per view; a pixel a previous view already stepped this frame is restored after the next view's pass, so each pixel steps once per frame. |
| `lua_camera_bounds` | Vanilla `is_in_camera_bounds` (`data/scripts/lib/utilities.lua`, `GameGetCameraBounds` `FUN_007aea80`) gates gravity fields, the moon, physics fungi, swirling and scattering particles and portal effects on the camera, i.e. on player 1 inside the window. | The mod appends `files/camera_bounds.lua` to utilities.lua: `is_in_camera_bounds(x, y, padding)` calls `GameGetCameraBounds(x, y, padding)`, which the patch answers natively (component scripts' Lua states have no FFI): inside any view's rect (the camera's int rect moved to the view, as `rain_views`), vanilla's test outside the window. The radar perk tests its holder's view only (`GameGetCameraBounds(px, py, x, y, padding)`). |
| `polymorph_slots` | Vanilla makes a polymorphed player's creature "the player" only for players-vector entries (`FUN_006b6d60` from the effect apply @0x00b05a3a; back with `FUN_006b7590` @0x00b019ed): tag `polymorphed_player`, AI off, ControlsComponent with `polymorph_hax`, `enabled` and Run, fog-of-war radius. Slots are not in the vector (it holds only the proxy), so a polymorphed slot became an AI creature nobody followed. The creature's attacks (ControlsSystem `FUN_00a8d7f0` @0x00a8e919, global-LCG pick of an AI attack) run only for an enabled ControlsComponent. | For a slot the swap runs with the slot standing in the vector's first entry (restored after the call), then the creature's `enabled` is cleared again (else the input system would write this client's input into it); the slot registry (`slots.rs`) follows the entity, and the mod drives the creature like a slot. A disabled ControlsComponent with `polymorph_hax` runs the attack block (code caves at 0x00a8d860/0x00a8d884 and its exit 0x00a8ec1a), from the applied input, on every client. |
| `sound_rng` | Sound events (`FUN_0047b990`) draw two global-LCG steps for pitch and volume (@0x0047be26, @0x0047be82), after early returns that depend on this client's audio state (event lookup, voice stealing). | Own LCG. Sounds no longer touch the simulation's RNG, so player-scoped ones can play on one client only (`local_presentation`). |
| `local_presentation` | (presentation, not determinism) Every slot has `player_unit`, so engine presentation gated by "is player" (`FUN_0063e880`) ran for every player on every client: log messages (`$log_teleported`, `$log_polymorph`, `$log_pickedup`, `$log_equipped`, ...), wand and breathing sounds, the out-of-air screen darkening; the low-health indicator read the players vector (the proxy); gamepad camera look-ahead, reticle and rumble (`FUN_00a8d5f0`) never ran for a slot. The damage flash (camera +0x5c amount, +0x60 fade rate; raised by the PlatformShooterPlayerSystem's damage handler for every slot, `FUN_006f2f10` @0x00bf66be, which draws the global LCG, on some paths only while the amount is under a limit; faded by `FUN_006f1d40` in the world update) flashed every client for every player's damage. | The gates keep the engine's answer (the simulation in those blocks is unchanged) and record whether the subject is this client's slot; the presentation calls in the block are skipped otherwise (`slots.rs` `GATED_PRESENTATION`). The low-health indicator and the gamepad users take this client's slot (its gamepad via the proxy). Damage flash: inside the window the camera keeps the shared flash (every player's hits, as before, so the LCG draws stay identical on every client); this client's own player's hits also raise a local copy (the same setter on a scratch object, with the LCG put back), which fades with the same dt and rule (hook at the fade call @0x006b32ae) and is what the render shows: swapped in at window exit, the shared one back at the next entry. |
| camera lock (window code, no patch) | GameGlobal::SetCameraPos ignores unforced requests while the camera lock (GameGlobal+0x3c) is set. A forced request sets the lock only if it actually moves the camera, and the engine clears it only at the end of each frame. So the forced simulation camera locked the camera only when this client's render camera had been elsewhere. Inside the window, unforced requests (any mod's or script's `GameSetCameraPos`) took effect on some clients and frames but not others, and the PlatformShooterPlayerSystem, which computes the player's camera target only while the lock is clear (`FUN_00bf2040` @0x00bf4caf), ran it on some frames. After the window, the render camera's request was ignored on every frame in which the lock was set. On a client whose player is not the first, the view alternated between the two players every frame while the first player stood still, and stayed on the first player while it moved. | `no_set_sim_camera` and the early window entry clear the lock after the forced move, on every client (vanilla's state at that point); a forced request inside the window (a teleporting player) sets it as in vanilla; `no_sim_leave` clears it. |

Seeds are set at world init and at frame 1 by `no_fix_seeds`:
- the platform RNGs, the global LCG and PM, the particle RNG and the `rand` state;
- the simulation RNG bank.

All of them are derived from the session seed.

## Mod-level rules

- The engine's own player entity is an inert input proxy. Each player's character is
  driven only by lockstep ticks (`files/players.lua`).
- Every simulation-affecting Lua call happens inside the window, in the same order on
  every client. Examples: world events in the bot scenario, inventory GUI ops
  (`no_gui_apply`), `GameSetCameraPos` for the simulation camera.
- Rendering-only Lua (the local camera, Noita's inventory GUI of the local player) runs
  outside the window.
- Pausing is not supported in a session. The inventory screen and the full-inventory
  replace screen stay open without pausing; the player's input is neutral meanwhile.
- All clients must run the same mods (mod hash) and game build; the server checks both.
- Noita's inventory GUI runs for this client's player only, after the window
  (`Players.gui_update`, patch `inventory_gui`). It reads this client's buttons
  (Inventory key; the mouse comes from the platform) through the player's
  ControlsComponent, set from the proxy for the call and restored. Its direct writes of
  item `inventory_slot` and `uses_remaining` (slot assignment, tooltip uses) and of
  `Inventory2Component.mForceRefresh` (it consumes the refresh request) are undone after
  the call; the mod assigns quick-inventory, spell-inventory and wand-card slots
  identically on every client each tick. When the screen opens the GUI sorts the
  containers' children by slot (`FUN_00b4ed70`); that sort is recorded as an op too
  (seed 1287715911 without it: the quick inventory's child order differed between clients
  from the first checkpoint after opening). The container capacity / free-slot queries
  (`FUN_00b5d7d0`, `FUN_00b5d940`) read the GUI's globals (its player's inventories), and
  the simulation calls them when a player picks up a spell, so on the other clients a
  pickup was refused (seed 1052469498: spells picked up on one client only); they run
  with the globals set to the container owner's. The entity digest includes every
  player's inventory (items in child order, containers, slots, uses, active item).
- Press stamps: the world update after `OnWorldPreUpdate(f)` runs as frame `f + 1`, and
  "just pressed" checks (item switching, throw, kick, interact) compare `mButtonFrame*`
  with that number, so applied inputs are stamped `f + 1` (`Input.press_frame`). The input
  proxy's ControlsSystem update runs early for that same update (`early_input`), so its
  stamps are `f + 1` too, and the capture counts a press stamped `f + 1` as pressed.
- Noita's ControlsSystem (`FUN_00a8d7f0`) derives fields of an *enabled* ControlsComponent
  every frame and leaves a disabled one (a slot's) alone; `Input.apply` derives them the
  same way from the tick input and the slot's state: `mFlyingTargetY = y - 10` (the fly
  model flies toward it; without it a slot could not levitate above y = 10, i.e. anywhere
  before the mines), the aim vector from the mouse position to the hitbox centre and its
  normalisation against 100 px (throw strength), the `input_latency_frames` delay lines and
  `mButtonLastFrameFire`. With a gamepad (input flag bit 31) the stick aim Noita computed
  is sent instead.
- The input proxy keeps only its ControlsComponent: its PlatformShooterPlayerComponent ran
  crouching and cell eating from this client's own input (Down held at the spawn point would
  have eaten cells on this client only).
- A polymorphed player's slot follows its creature and back (`polymorph_slots`; the mod
  reads the slot registry at the start of each tick): the creature gets the slot's setup
  (driven ControlsComponent, no death by itself, a dead slot's body stays a spectator, which
  is also never polymorphed), identically on every client. `Input.apply` leaves the buttons
  the ControlsSystem never writes (Action, Jump, Run, Dig, HolsterItem, Eat) alone, so the
  Run the polymorph holds for the creature survives. A dead player's spectator body is
  `polymorphable_NOT`, so the effect apply refuses it.
- Vanilla Lua scripts' player-scoped messages (pickups of wands, hearts, chests, orbs,
  essences and the greed curse; perks; kantele/ocarina songs; fungal shifts; the
  adventurer perk; dice and the worm deflector "near a player") run in callbacks that
  every client runs for every player. `files/local_prints.lua` rewrites those print
  statements (same line) to `if <subject is this client's player> then ... end`; the
  player is the mod setting `noita_online.local_entity`, set by `players.lua` (follows a
  polymorph; 0 when dead). Only the prints (and the "all orbs" achievement) depend on it.
- The engine maps the mouse to world coordinates (`ControlsComponent.mMousePosition`)
  through the camera of the moment. Inside the window that is the simulation camera,
  another player's view on every client but the first. The proxy's update runs before the
  window (`early_input`), i.e. through this client's render camera, the view the player
  aimed in; in a frame without that update the mod maps the mouse again through the render
  camera (`DEBUG_GetMouseWorld` after the render camera is set) and sends that position.
- Input delay changes during the session (`InputDelay`, see architecture.md) only change
  which frame a client's captured input is sent for; every client simulates the server's
  ticks, so they cannot desync.

## Verification

Each client hashes every loaded chunk's material cells once every 60 frames, staggered by
chunk index. It also reports player and enemy positions plus an entity digest. The server
compares all clients' reports per frame (`hashes.jsonl`). Setting
`NOITA_ONLINE_HASH=full` reports full snapshots every 60 frames.

Diagnostics used to find the root causes above, enabled by environment variables:
- `NOITA_ONLINE_DENSE=a-b`: per-frame entity dumps. With `NOITA_ONLINE_ENTLOG=id,...` it
  logs those entities' transforms each dense frame; `NOITA_ONLINE_ENTLOG_COMPS=1` adds every
  component's scalar members and the children.
- `NOITA_ONLINE_RNGLOG`: RNG states and cameras at window entry and exit, plus the camera
  lock (`lock`), the last Box2D step's wall time (`b2ms`), the pathfinding clearance-refresh
  centre and state (`pfc`, `pfs`), the platform frame counter (`pf`), the game thread's
  floating-point control state (`fp`: x87 control word / MXCSR) and, per window edge, a
  `V` line: the shared screen shake, this client's own shake and view, and per view the sum
  of the fog-of-war pixels around it. `gw` (the GridWorld RNG) is not banked: a value at
  window entry that differs from the previous exit means something drew it outside the
  window (a worker thread before `chunk_rng`). The `B` line (Box2D body motion hash) also
  carries `sleep=`, a hash of every body's awake/active flags and sleep time;
  `NOITA_ONLINE_RNGLOG_BODIES=1` adds one `b` line per body (hash, position, type `t`,
  flags `f`, sleep time `s`). The `G` line summarises the falling-sand state: the active
  tile list (count, deferred count, order hash), a hash of every tile's RNG, the GridWorld
  frame counter, the pending chunk jobs, and per GridParticles list its length and a hash
  of the particles' positions and velocities. `run/fork9/rngcmp.py` finds the first
  differing window edge and field.
- `NOITA_ONLINE_WAKEWATCH=a-b`: hardware write watchpoints on the awake flag of up to four
  sleeping dynamic bodies (the same bodies on every client); every change is logged with the
  writing code's return-address chain and whether it happened inside the window, to
  `wake_<inst>.txt`, plus each `stuck_wake_aabb` query (the stale stack value it replaced).
- `NOITA_ONLINE_STEPLOG=a-b`: per frame, the RNG states, GridWorld RNG and Box2D body
  count / motion / sleep hashes at every stage of the step (`step_<inst>.txt`): entry and exit
  of the frame sections (the profiler's wrappers), after every entity system's update
  (`s<i>`, RTTI names in `N` lines) and at window entry/exit. `NOITA_ONLINE_STEPLOG_TILES=1`
  adds, at window entry and around the grid update, the active tile list and every tile's
  RNG, material hash, rects and wake flags (`T` lines). `run/fork7/stepcmp.py` and
  `tilecmp.py` find the first differing stage. With `NOITA_ONLINE_STEPLOG_LITE=1` each
  frame is one `L` line of `point:hash` pairs (RNGs, GridWorld RNG, body count, particle
  list lengths; no body motion hash), cheap enough for a whole run (`run/fork9/litecmp.py`).
  Points outside the window hash this client's render RNG bank, so differences there are
  expected.
- `NOITA_ONLINE_RNGWATCH=a-b`: hardware write watchpoints on the global LCG, particle RNG,
  GridWorld RNG and `rand` state in every thread but the game thread while the window is
  open; each write is logged with its code chain (`rngwatch_<inst>.txt`, with a positive
  control at the first armed frame).
- `NOITA_ONLINE_EXPLOG=a-b`: every explosion a grid tile job queues (tile vtable+0x80), with
  the frame, the tile, the position, the radius and the engine's process-wide count before it
  (`explog_<inst>.txt`, thread order: sort within a frame to compare clients). Works with and
  without `tile_explosions`.
- `NOITA_ONLINE_PCLOG=1`: every slot's PlayerCollisionComponent and CharacterDataComponent
  members, every frame (client log). `NOITA_ONLINE_SCENE=chests` (human scenario `items`):
  a chest is put on the spawn point every 300 frames from 600, so it rolls through the
  players standing there.
- `NOITA_ONLINE_PTRACE`: call chains of particle creation; also every body→grid placement
  (body pose, velocity, vector key), particle construction (position, velocity), fixture
  creation, ragdoll loads (stain colour and overlay image hashes) and the images turned
  into body shapes.
- `NOITA_ONLINE_DUMP`: chunk and cell dumps. With `NOITA_ONLINE_DUMP_RECT` it adds
  particles, the GridParticles side lists, the render material snapshot hash and the
  physics bodies; `NOITA_ONLINE_DUMP_ENTRY=1` also dumps at window entry
  (`dump_<inst>_<frame>e.*`).
- `NOITA_ONLINE_PF_SKEW=<instance>:<n>`: adds n to that instance's platform frame counter
  once, at session start, as a stand-in for time spent in menus (`platform_frame` overrides
  it).
- `NOITA_ONLINE_CHUNKJOB_DELAY=<instance>:<ms>`: delays the start of every chunk job on that
  instance, as a stand-in for a slower or busier machine, so a race between chunk jobs and
  the simulation shows up on every run instead of now and then.
- `NOITA_ONLINE_TILE_REVERSE=<instance>,...`: on those instances, a checkerboard pass's
  pooled tiles are not submitted to the workers but run on the game thread at the end of
  the pass's dispatch, in reverse order. Any dependence of a tile's result on the other
  tiles of its pass then diverges from the other clients on every run.
- `NOITA_ONLINE_HEAP_PAD=<instance>,...`: on those instances every `malloc` and `operator
  new` allocates a pseudo-random 0–112 bytes more, so the relative order of heap addresses
  differs from the other clients' far more often; simulation code ordered by pointer then
  diverges on most runs.
- The client log's `state report` lines carry the chunk gate's statistics: frames whose
  prewait waited, in-lock fallback waits (longest wait, frame of the last one, and how many
  were still unfinished after the 5 s bound, i.e. integrated at a timing-dependent frame).
- `NOITA_ONLINE_PROF` / `NOITA_ONLINE_SAMPLE`: profilers.
- `NOITA_ONLINE_B2STATS=1` (`box2d_overlap`): every 600 frames, `box2d_<inst>.txt` counts the
  steps sent to the worker, the steps a flag test forced (per site: in the window, outside it,
  waits) and the joins that waited. `NOITA_ONLINE_B2CHECK=1` runs the step at the render
  start on the game thread and counts steps that changed any RNG and frames in which a body
  changed between the render start and the next DeathMatch::Update (both must stay 0).
  `NOITA_ONLINE_B2INLINE=1` runs it there without checks (same simulation, no overlap: for
  measuring the overlap on an identical trajectory).
- `NOITA_ONLINE_LATPROBE=1`: input-latency probe (`files/latprobe.lua`, `lat_<inst>.txt`;
  also on vanilla play without a server); `harness/latency.py` counts the frames from the
  input poll to the character's reaction.
