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
players alive (1000 max HP, healed to full below half), refills their spells' uses before
the bomb phase and gives them a potion before the throw phase when their first item slot
is empty. Its potions hold a fixed rotation of liquids (no polymorphines) and its water
pours are bounded, so the players stay on the terrain they dig instead of floating up a
growing sea. `bot f=...` lines in `client_<id>.log` (every 300 frames per player) show each
player's HP, position, held item, inventory, spell uses, nearby projectiles and its last
Fire/Throw/Kick/Interact frames.

```sh
harness/e2e.sh --pin --players 4 --frames 36000 --out run/e2e/full          # 10 minutes
harness/e2e.sh --pin --players 4 --frames 36000 --netsim loss=0.05,latency=100 --out run/e2e/netsim
harness/e2e.sh --pin --players 1 --frames 3600 --seed 98765432 --out run/e2e/fps_mp1
harness/e2e.sh --pin --solo --players 1 --frames 3600 --seed 98765432 --out run/e2e/fps_solo1
harness/e2e.sh --pin --solo --players 4 --frames 36000 --out run/e2e/solo4   # informational
harness/evaluate.py --run run/e2e/full --netsim-run run/e2e/netsim \
  --fps-run run/e2e/fps_mp1 --baseline run/e2e/fps_solo1 --info-baseline run/e2e/solo4
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
mismatches at every 60-frame checkpoint, the mod's FPS overhead (a 1-player session vs the
vanilla single-player baseline, same scenario, seed and pinning; average and 1 % low within
5 %), bandwidth below 100 KB/s per client, server CPU below one core, and the
same session guarantees under the network conditions. Every lockstep client simulates every
player, so an N-player session carries N players' workload; its FPS against N solo
instances is printed for information only.

### Profiling

* `NOITA_ONLINE_PROF=1`: `prof_<id>.txt` (time of every job-pool submit call site, i.e. of the
  jobs that run inline) and `sections_<id>.txt` (main-thread time per frame of
  DeathMatch::Update and its children, the Lua dispatches, the world draw and the buffer
  swap; also in `--solo` runs). `harness/prof_sections.py DIR...` compares runs side by side.
* `NOITA_ONLINE_SAMPLE=1` (with `PROF`): samples the game thread's EIP and EBP chain every
  ~1 ms into `samples_<id>.txt` (with a module map for non-game addresses);
  `harness/prof_samples.py DIR` aggregates them per function through the Ghidra query server.
