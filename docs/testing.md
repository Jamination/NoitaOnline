# Testing

## Unit and integration tests

```sh
cargo test --release --workspace
```

Covers the reliable-UDP core (in-order exactly-once delivery under 30 % loss, reordering
and duplication; fragmentation of messages up to 1 MiB; MTU compliance; retransmission and
recovery after a total outage; independent channel ordering; sequence wraparound past
65 536 packets/messages; RTT estimation; timeouts; malformed packets), the UDP endpoints
over localhost (4 clients + denial of a 5th, disconnects, timeouts, 5 % loss + 100 ms each
way on both ends), the game protocol codecs, the lockstep relay and the verifier.

## Headless load test

```sh
harness/sim_test.sh 4 1800 loss=0.05,latency=100
```

Server plus `simclient` bots running the full lockstep loop at 60 Hz, with each bot's state
derived from the ticks it consumed (relay bugs show up as verification mismatches).
`RUNS=3 harness/sim_test.sh 3 600` plays three runs: every run but the last ends with each
bot leaving it (`NextRun`) at frame 600 + 60 · its index while the others keep playing it.

## End-to-end with real Noita clients

`harness/e2e.sh` must run inside a desktop session (it launches GUI processes). Each
client runs in its own Proton prefix under `run/prefix<id>` (cloned from `run/prefix0`,
Steam-free, config forced by `harness/prep_prefix.sh`, save wiped per launch), so the
user's Steam installation and saves are never touched. The server passes
`--scenario bot`: every client drives its player with the scripted bot
(`mod/noita_online/files/bot.lua`: walking, levitating, spell casts, bomb digging, potion
throws, kicks, item pickups, item switching, fights, in a 4800-frame cycle of eight
phases) and the simulation spawns enemies, items, liquids and explosives around the
players at fixed frames and polymorphs the last player during each cycle's long walk. So
that every cycle exercises all of that for the whole session, the scenario keeps the bot
players alive (1000 max HP, healed to full below half), makes them keep their items when
another player kicks them (`is_immune_to_kicks`: the bots press Kick at the same frames, and
a kicked player otherwise throws its whole inventory, see the README's limitations), refills
their spells' uses before the bomb phase and gives them a potion before the throw phase when
their first item slot is empty. Its potions hold a fixed rotation of liquids (no
polymorphines) and its water pours are bounded, so the players stay on the terrain they dig
instead of floating up a growing sea. `bot f=...` lines in `client_<id>.log` (every 300
frames per player) show each player's HP, position, held item, inventory, spell uses,
nearby projectiles and its last Fire/Throw/Kick/Interact frames.

```sh
harness/e2e.sh --pin --players 4 --frames 36000 --out run/e2e/full          # 10 minutes
harness/e2e.sh --pin --players 4 --frames 36000 --netsim loss=0.05,latency=100 --out run/e2e/netsim
harness/fps_pairs.sh --pairs 3 --seed 98765432 --frames 3600 --out run/e2e/fps   # MP/solo pairs
harness/e2e.sh --pin --solo --players 4 --frames 36000 --out run/e2e/solo4   # informational
harness/evaluate.py --run run/e2e/full --netsim-run run/e2e/netsim \
  --fps-run run/e2e/fps/mp_* --baseline run/e2e/fps/solo_* --info-baseline run/e2e/solo4
```

**Next run** (`--scenario restart`, `files/nextrun.lua`): the bot scenario, and in the first
run every player counts as dead at frame 1200 (`NextRun.all_dead` inside the window,
vanilla's `GameTriggerGameOver` outside it, a stand-in for the real all-dead path); each bot
presses vanilla's New game 120 + 180 · its player id frames later (`no_press_new_game`), so
the clients leave the old world at different frames while the others keep simulating it;
the new run (new seed, same players) then plays the bot scenario. `--frames` (and
`NOITA_ONLINE_DURATION`) counts frames per run: a client finishes when its current run
reaches it. `summary.json` lists each run's frames and verification totals under `runs`;
`hashes.jsonl` lines carry the run.

```sh
harness/e2e.sh --players 2 --scenario restart --frames 2700 --seed 98765432 --timeout 1500 --out run/nextrun/2p
```

* `--pin` gives client *k* its own physical core (CPUs `B+k` and `B+6+k`, `B` =
  `NOITA_PIN_BASE`, default 2), so concurrent clients on one machine behave like separate
  machines; the baseline uses the same pinning. Concurrent harness runs must use disjoint
  bases (e.g. 2 and 4 with two clients each). Without `--pin`, `NOITA_CPUSET` (e.g.
  `0,1,6,7`) confines every client of the run to those CPUs instead.
* `--solo` is the single-player baseline: the same scripted scenario without networking
  or engine patches, one player per instance. As in a session, a player at 0 HP (not
  reached in the bot scenario) becomes a spectator instead of dying, so the scenario's
  world events keep running.
* `--netsim` applies 5 % loss and 100 ms latency to every datagram the server sends *and*
  receives (i.e. on both directions of every client link).

`evaluate.py` checks: builds (Linux server, Windows server, Windows client DLL), the test
suite, a complete 4-client session with no disconnects, zero chunk-hash and zero position
mismatches at every 60-frame checkpoint, the mod's FPS overhead (1-player sessions vs the
vanilla single-player baseline, same scenario, seed and pinning, median over alternating
runs, see below; average and 1 % low within 5 %), bandwidth below 100 KB/s per client,
server CPU below one core, and the same session guarantees under the network conditions.
Every lockstep client simulates every player, so an N-player session carries N players'
workload; its FPS against N solo instances is printed for information only.

### Measuring the FPS overhead

A single MP run against a single solo run is not a usable comparison:

* Vanilla's simulation is not deterministic (job threads race on shared RNGs, wall-clock
  budgets), so identical `--solo` runs leave the scripted trajectory after a few hundred
  frames: the bot ends up in different places, digging through different material, and
  the frame cost of the rest of the run differs (seed 98765432, 3600 frames: 41 to 50 fps
  average). A lockstep session is deterministic and replays one trajectory; its runs agree
  to about 0.5 %.
* Background load on the desktop (audio, browser, other agents) moves every run by a few
  percent over minutes, and runs started side by side share the X server, which slows the
  faster (solo) run more.

`harness/fps_pairs.sh` therefore runs MP and solo one at a time on the same pinned core,
alternating which goes first (MP solo, solo MP, ...), and `evaluate.py` compares the
median average FPS and the median 1 % low of each side over all runs (frames 600 on).
A session replays one trajectory per seed, and which one depends on details such as the
session's start hour (weather); a single trajectory can be several percent heavier or
lighter than the solo median. So that both sides sample the scenario alike, MP run k
gets one extra frame of "Right" for the bot at frame 400 + 50·(k−1) (`nudge<N>` below),
a distinct deterministic trajectory per run, as solo runs diverge on their own around
frame 750. `--same-trajectory` replays one trajectory (for A/B tests of a change).
Use at least 4 pairs per seed and several seeds; more pairs narrow both medians.

### Profiling

* `NOITA_ONLINE_PROF=1`: `prof_<id>.txt` (time of every job-pool submit call site, i.e. of the
  jobs that run inline) and `sections_<id>.txt` (main-thread time per frame of
  DeathMatch::Update and its children, the Lua dispatches, the world draw and the buffer
  swap; also in `--solo` runs), one block per `NOITA_ONLINE_PROF_WINDOW` frames (default 600).
  Each block has a `counters:` line (window means, sampled every 10th frame, every frame for
  windows under 10 frames): DrawGrid rects, camera size and centre, particle list lengths,
  active/deferred grid tiles, Box2D bodies, loaded chunks, live entities and those tagged
  `enemy` / `projectile` / `item_pickup`, the first `player_unit`'s position; and a
  `chunkset:` line (the loaded chunks at the window's end), to compare the workload of a
  session and the baseline, not just their time.
  `harness/prof_sections.py [-r FROM:TO] DIR...` compares runs side by side (windows 2.. by
  default; with `-r` the windows inside that frame range).
* `NOITA_ONLINE_SAMPLE=1` (with `PROF`): samples the game thread's EIP and EBP chain every
  ~1 ms into `samples_<id>.txt` (with a module map for non-game addresses);
  `harness/prof_samples.py DIR` aggregates them per function through the Ghidra query server.
  The sampler itself costs about 2 ms per frame: compare sampled runs only with sampled runs.

### Cost ladder (diagnostics)

What a 1-player session costs over the solo baseline is measured piece by piece, in a
window of frames where both runs follow the same trajectory (seed 98765432: frames 300 to
about 750; up to 1500 when only session runs are compared, which replay one trajectory), with
`PROF` sections and repeated, alternating runs. These switches are diagnostics only and never
set in normal play:

* `NOITA_ONLINE_LADDER=flag,...` (`init.lua`):
  * session runs: `noreport` (no state reports: no entity digest, no chunk hashes),
    `nolocal` (no presentation input for the own player: raw mouse for the camera, reticle,
    render-camera mouse), `norendercam` (the view stays at the simulation camera);
    `nudge<N>` (one extra frame of "Right" for the bot at frame N: a different, still
    deterministic trajectory, to sample session runs over several trajectories the way solo
    runs spread on their own);
  * `--solo` runs: `proxy` (an inert input proxy entity as in a session), `freecam` (the
    session's free camera, set every frame to the player's camera target).
* `NOITA_ONLINE_PATCH_OFF=name,...` removes engine patches (a 1-player session cannot
  desync; the trajectory may change).
