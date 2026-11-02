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
  with force, and the engine's camera lock is then cleared on every client, as vanilla has
  it at that point of a frame. Outside the window, each client's render camera is the
  camera Noita computed in the window for its own player (`mDesiredCameraPos` of its
  `PlatformShooterPlayerComponent`, the only one with `center_camera_on_this_entity`).
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
| `threaded_pathfinding` | (performance) The pathfinding update's per-tile jobs (`FUN_00864590`, about 1.4 ms per frame inline). | They stay on worker threads. Each job recomputes the node flags of its own grid tile from that tile's cell summary only, with no RNG and no allocation, and `FUN_00864590` joins them all before anything else runs. |
| `inline_group`, `inline_async` | Other pool jobs (particles, Box2D step, pathfinding search, entity jobs) race on shared RNG state and overlap the next frame's simulation. | Run synchronously at submission. |
| `particle_init` | Particle structs keep uninitialised heap bits, which select the collision world. | Cleared fully. |
| `camera_loads` | Chunks load around each client's camera and look-ahead. | Camera-driven loads are removed. The mod streams chunks around every player (`no_stream_focus`) and unloads far chunks deterministically (`no_unload_far`). |
| `fixed_dt` | `DeathMatch::Update` feeds wall-clock `dt` into the simulation. | `dt = 1/60`. |
| `focus_freeze` | Far physics bodies freeze by distance to the local camera. | Distance to the nearest player, on a deterministic schedule. |
| `particle_views` | Vanilla simulates view-bound particles only on screen: creation with the view-bound flag returns early outside the camera rect (GridWorld+0x498 origin, render snapshot size) in `FUN_007168c0` (cell→particle), `FUN_00716af0` and `FUN_00716ce0`; cosmetic particles (+0x6a bit 1) collide with the render's material snapshot of the screen (`FUN_00712390`) and land when they leave it. Inside the window the camera is player 1's, and the snapshot belongs to each client's own render (outside the window), so particles landed differently per client (re_camleak P14) and only player 1's view got view-bound particles. | "On screen" means inside some player's view (`views.rs`: focus centre ± 216+64 × 121+64 px). The three creation culls test that; a cosmetic particle's lookup reads the world as the snapshot shows it (material id, −1 for no cell or a cell the render does not draw, vtbl+0x10 == 4) inside a view and takes vanilla's leave-the-screen path outside every view. |
| `camera_bound` | CameraBoundSystem tests entities against the one camera (player 1 inside the window): farther than `distance` → frozen/killed (`FUN_00a5bd10`), `max_count` cull keeps those nearest the camera (`FUN_00a5bbe0` → `std::sort` `FUN_00d68150`), frozen records respawn only in the 5×5 cells around it and off its screen (`FUN_00a5be10` → `FUN_00a590f0`); `FUN_00a5c730` respawns around the render camera outside the window. | Distances are to the nearest view centre; the respawn grid runs around every view centre (slot order, each cell once); the sort is the engine's for ≤1 view and an exact MSVC-sort port with the nearest-view key for more; with 2+ views entities in unloaded chunks freeze and records never respawn into one; `FUN_00a5c730` is skipped once views exist (`cambound.rs`). |
| `clock` | Weather re-initialises on hour and day changes; snow, holidays and `GameGetDateAndTime*` read the local date. | `GetLocalTime`, `GetSystemTime`, `_time64` and `_localtime64` return the server's session start time plus game time, in UTC. |
| `early_window` | Vanilla `data/scripts/init.lua` emits rain in its `OnWorldPreUpdate`, which runs before the mod's, with the local camera and the render RNG. | The window opens natively before any mod's `OnWorldPreUpdate`. |
| `freecam_static` | The free camera moves itself (keyboard, easing) inside the window. | Removed; only the mod moves the camera. |
| `cell_flash` | The flash flag on a cell (`+0xc`) keeps its tile active, but only the render job cleared it, and only for on-screen cells. | The liquid update expires the flag itself; the render only reads it. |
| `cell_emitters` | Material particle effects (`render_on_grid` ParticleEffects: sparkles and bubbles over magic liquids, acid, poison, radioactive liquid, cursed rock, …) are emitted from the on-screen surface cells the grid render job finds (`FUN_00724580` queue → `FUN_00724480` → `FUN_007171d0`), outside the window, in the order of this client's view; the particles then live in the shared GridParticles lists and draw the simulation particle RNG (seed 2046634918, f=1385). | The render no longer queues emitters. At window entry (simulation RNG bank loaded) the union of every player's view is scanned with the render's rule (drawn cell, material+0x28c set, snapshot pixel above empty) and `FUN_007171d0` is called for each hit in row order (`cosmetic.rs`), i.e. what each player's render queued at the end of the previous frame. |
| `rain_views` | `GameEmitRainParticles` (vanilla `init.lua` weather) emits rain/snow in a band around the camera (`FUN_00c0e9f0`: GridWorld+0x498/+0x49c/+0x4a0), i.e. around player 1 inside the window. | With 2+ views the band is emitted around every player (the camera's int view rect moved by the player's offset), skipping a player whose view an earlier emitted band already covers from the same height or above (vanilla density where views overlap). |
| `box2d_freeze` | With the magic number `BOX2D_FREEZE_STUCK_BODIES` (on by default), the body→grid update (`FUN_00718d50`) freezes the most-stuck awake body (`FUN_00717aa0`) whenever the previous Box2D step took longer than `BOX2D_THREAD_MAX_WAIT_IN_MS` (12 ms) of wall-clock time (`b2World+0x19270`): a safety net for physics too heavy for the machine. Which frames cross 12 ms depends on machine load (seed 1717171717, f=1382). | The time test reads a flag set from the tick: each client measures its own step at window exit and reports a slow one as a trailing byte of its next input (`no_send_input`; `no_wait_tick` strips it before Lua sees the input). A tick in which any player reported a slow step freezes the most stuck body on every client, one input delay after the slow step. |
| `free_solids_order` | Bodies that lost cells in a frame (`mFreeSolids`, a `std::map<b2Body*, …>`) are rebuilt in `b2Body*` heap-address order (`FUN_0071d020`). Rebuilding removes the old body from the body vector and appends the new ones, so after an explosion broke several bodies the new bodies sat at client-specific vector positions. The next frame's body→grid pass places bodies in vector order, so displaced cells became particles in a different order (seed 1717171717, f=1382). | Rebuilt in body-vector order (the entry whose body has the lowest vector index goes first). |
| `pathfinding_refresh` | With `PATHFINDING_DISTANCE_FIELD_ENABLED` (on by default), the pathfinding job (`FUN_00861a50`) keeps refreshing the node clearance field of the 2×2 pathfinding chunks around the camera centre: one chunk every two frames, then a new cycle (`FUN_008605e0` → `FUN_00860150`). The search reads this clearance as the room a mover has, and nothing else computes it. The job runs before the simulation window, so it used each client's render camera. Clients refreshed different chunks at different frames, and an enemy searching in between got a different path (seed 1404389217, f=1554: a rat's next path node). | The cycle centres on the players, using the focus centres from the previous window. With one player it is vanilla's single cycle. With more, every player runs its own cycle each frame in slot order (own copy of the coroutine state at `0x01208520` and, mid-chunk, of `FUN_00860150`'s chunk scratch), so every player's surroundings are refreshed at vanilla's rate. |
| `ragdoll_stains` | When a creature dies, its ragdoll images are blended with the creature's stains (`FUN_00abb7d0` → `FUN_00be8240`, overlay at `0x00abc082`). The overlay was the SpriteStains image at +0x20: the stain colours (+0x00) blurred with the left neighbour, written only by the stains texture upload (`FUN_00c21020`) from the render-side queue, and only for sprites drawn last frame. So each client blended in its own last *drawn* stains, all zero for a creature it never saw, and small ragdoll parts (one box fixture per run of equal-colour pixels, `FUN_009817d0`) got different fixtures and mass (seed 1786459272, f=5345). | The site passes a scratch image holding the upload's blur of the *current* stain colours, bit for bit as `FUN_00c21020` computes it (per channel (c + c_left) / (1 + a_left) in f32, ×255 truncated and clamped, `FUN_0044abc0`): vanilla's look from simulation state only (`ragdoll.rs`). It differs from vanilla only where vanilla's overlay lagged the stains (upload queue) or was empty (creature died off-screen). |
| `platform_frame` | The platform frame counter (PlatformDesktop+0x20, its vtable+0x60 getter) counts main-loop frames from before the world exists (62 of them in a `-gamemode 0` start), so it differs between clients by the time each spent before its session started, e.g. in menus. About 60 sites read it, simulation ones among them: the SpriteStains update runs for an entity when `(frame ^ entity_id) & 15 == 0` (a player: every 3rd frame) and draws the global LCG there; PathFindingComponent (`FUN_00bca030`) and animal AI (`FUN_00b750b0`) schedule work by it. | The counter is set to GameGlobal's frame + 2^20 by `no_fix_seeds` and at every window entry. The base is above any pre-session count, so the counter only jumps forward. |
| `analog_fly_rng` | With the option `gamepad_analog_flying` and a gamepad, the ControlsSystem (`FUN_00a8d7f0`) dithers the fly button from the analog trigger with one global-LCG step per frame. It runs only for an enabled ControlsComponent, i.e. the local input proxy, inside the window, so that client's simulation LCG ran ahead of the others'. | The dither uses its own LCG state (@0x00a8e8c1, 0x00a8e8d0); the dithered Fly bit is captured and sent as input. |
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
| `local_presentation` | (presentation, not determinism) Every slot has `player_unit`, so engine presentation gated by "is player" (`FUN_0063e880`) ran for every player on every client: log messages (`$log_teleported`, `$log_polymorph`, `$log_pickedup`, `$log_equipped`, ...), wand and breathing sounds, the out-of-air screen darkening; the low-health indicator read the players vector (the proxy); gamepad camera look-ahead, reticle and rumble (`FUN_00a8d5f0`) never ran for a slot. | The gates keep the engine's answer (the simulation in those blocks is unchanged) and record whether the subject is this client's slot; the presentation calls in the block are skipped otherwise (`slots.rs` `GATED_PRESENTATION`). The low-health indicator and the gamepad users take this client's slot (its gamepad via the proxy). |
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
  with that number, so applied inputs are stamped `f + 1` (`Input.press_frame`).
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
  Run the polymorph holds for the creature survives.
- The engine maps the mouse to world coordinates (`ControlsComponent.mMousePosition`)
  inside the window, through the simulation camera. That is another player's view on every
  client but the first. The mod maps the mouse again through the render camera
  (`DEBUG_GetMouseWorld` after the render camera is set) and sends that position.

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
  centre and state (`pfc`, `pfs`), the platform frame counter (`pf`) and, per window edge, a
  `V` line: the shared screen shake, this client's own shake and view, and per view the sum
  of the fog-of-war pixels around it.
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
- `NOITA_ONLINE_PROF` / `NOITA_ONLINE_SAMPLE`: profilers.
