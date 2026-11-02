-- Noita Online: deterministic-lockstep multiplayer client.
--
-- At game start (init.lua runs when a new game begins) the mod connects to the headless
-- server, waits for the session to start, and takes the world seed from it. Every frame it
-- sends this client's input for frame f + input_delay, waits for the server's tick for
-- frame f (all players' inputs), and applies it before the simulation runs. In-memory
-- patches (native runtime) make the engine's simulation bit-deterministic, so every client
-- computes the same world. Every 60 frames each client reports world/entity hashes, which
-- the server cross-checks.

local ROOT = "mods/noita_online/files/"
local Config = dofile_once(ROOT .. "config.lua")
local cfg = Config.load()

local logf = nil
if cfg.log_path then logf = io.open(cfg.log_path, "w") end
local function log(msg)
  local line = "[noita_online] " .. msg
  print(line)
  if logf then logf:write(line, "\n") logf:flush() end
end

if not cfg.enabled then
  log("disabled (no server configured)")
  return
end

local NO = dofile_once(ROOT .. "native.lua")
local Input = dofile_once(ROOT .. "input.lua")
local Players = dofile_once(ROOT .. "players.lua")
local State = dofile_once(ROOT .. "state.lua")
local Bot = dofile_once(ROOT .. "bot.lua")

local session = nil   -- { seed, input_delay, player, players, scenario }
local solo = cfg.solo
local finished = false

local function drain_events()
  for _, e in ipairs(NO.poll_events()) do log("net: " .. e) end
end

local function quit_game(reason)
  log("finishing: " .. reason)
  if cfg.perf_path then
    local n = NO.lib.no_perf_dump(cfg.perf_path)
    log("perf frames dumped: " .. tostring(n))
  end
  NO.lib.no_shutdown()
  if logf then logf:close() logf = nil end
  if cfg.exit_on_finish then os.exit(0) end
end

-- 1. Session setup (before the world exists).
if solo then
  session = { seed = cfg.solo_seed, input_delay = 0, player = 0, players = { 0 }, scenario = cfg.bot and "bot" or "" }
  log("solo baseline mode, seed " .. session.seed)
else
  local mask = NO.lib.no_apply_patches()
  if mask < 0 then
    log("FATAL: determinism patches failed: " .. NO.last_error())
    error("noita_online: " .. NO.last_error())
  end
  log(string.format("determinism patches applied (mask %x)", mask))
  local rc = NO.lib.no_connect(cfg.server, cfg.name, Config.GAME_BUILD, 0, cfg.netsim or "")
  if rc ~= 0 then error("noita_online: connect failed (" .. rc .. ")") end
  log("connecting to " .. cfg.server .. " as " .. cfg.name)
  local st = -1
  while st == -1 do
    st = NO.lib.no_wait_started(1000)
    drain_events()
  end
  if st ~= NO.STATUS.started then
    error("noita_online: could not join session (status " .. st .. ")")
  end
  local info = NO.start_info()
  session = info
  -- Scenario comes from the server so every client runs the same world events.
  session.scenario = info.params.scenario or ""
  log(string.format("session started: seed=%d delay=%d me=%d players=%s", info.seed, info.input_delay, info.player, table.concat(info.players, ",")))
end

local magic = string.format([[<MagicNumbers
  WORLD_SEED="%d"
  DEBUG_NO_PAUSE_ON_WINDOW_FOCUS_LOST="1"
  DEBUG_SINGLE_THREADED_LOADING="1"
  DEBUG_THREADED_WORLD_CREATION="0"
  BOX2D_THREAD_MAX_WAIT_IN_MS="100000"
  GRID_FLEXIBLE_MAX_UPDATES="0"
/>]], session.seed)
ModTextFileSetContent("mods/noita_online/files/session_magic.xml", magic)
ModMagicNumbersFileAdd("mods/noita_online/files/session_magic.xml")

function OnMagicNumbersAndWorldSeedInitialized()
  if not solo then NO.lib.no_fix_seeds(session.seed) end
end

function OnWorldInitialized()
  if not solo then NO.lib.no_fix_seeds(session.seed) end
end

local ready = false

function OnPlayerSpawned(player)
  if ready then return end
  if solo then
    Players.setup_solo(player)
  else
    Players.setup(player, session.players, session.player)
  end
  ready = true
  log("players ready at frame " .. GameGetFrameNum())
end

local function local_input(f)
  if not ready then return Input.NEUTRAL end
  if cfg.bot then
    local slot = Players.local_slot()
    local px, py = 0, 0
    if slot and EntityGetIsAlive(slot.entity) then px, py = EntityGetTransform(slot.entity) end
    return Bot.input(session.player, f, px, py)
  end
  return Players.capture_local()
end

local lockstep_alive = true

function OnWorldPreUpdate()
  if finished then return end
  local f = GameGetFrameNum()
  NO.lib.no_frame_mark()
  if solo then
    if ready then
      local s = local_input(f)
      Players.apply_tick(f, { [0] = Input.encode(s) })
      if session.scenario == "bot" then Bot.world_events(f, Players.slots) end
    end
    return
  end
  if f == 1 then NO.lib.no_fix_seeds(session.seed) end
  if not lockstep_alive then return end
  NO.send_input(f + session.input_delay, Input.encode(local_input(f)))
  local tick, err
  repeat
    tick, err = NO.wait_tick(f, 1000)
    if not tick then drain_events() end
  until tick or err ~= "timeout"
  if not tick then
    log("lockstep stopped at frame " .. f .. ": " .. tostring(err))
    lockstep_alive = false
    return
  end
  if ready then
    Players.apply_tick(f, tick)
    if session.scenario == "bot" then Bot.world_events(f, Players.slots) end
  end
end

function OnWorldPostUpdate()
  if finished then return end
  local f = GameGetFrameNum()
  if not solo and ready and lockstep_alive and f % 60 == 0 then
    local h, positions = State.collect(Players.proxy)
    NO.report_state(f, h, positions)
  end
  if f % 600 == 0 then drain_events() end
  if cfg.duration and ready and f >= cfg.duration then
    finished = true
    quit_game("duration reached at frame " .. f)
  end
end
