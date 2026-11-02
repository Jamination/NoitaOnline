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
  -- Every client must run the same mods (they change the simulation); the server compares this.
  local mods = ModGetActiveModIDs() or {}
  table.sort(mods)
  local mod_hash = 5381
  for _, ch in ipairs({ string.byte(table.concat(mods, ",") .. "|" .. Config.MOD_VERSION, 1, -1) }) do
    mod_hash = (mod_hash * 33 + ch) % 4294967296
  end
  log("active mods: " .. table.concat(mods, ","))
  local rc = NO.lib.no_connect(cfg.server, cfg.name, Config.GAME_BUILD, mod_hash, cfg.netsim or "")
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

-- Lockstep sessions stream chunks around every player (foci) with a deterministic unloader
-- instead of the per-client camera and wall-clock unloader, and never cap grid updates
-- (the cap keeps the tiles nearest each client's own camera).
local magic = string.format([[<MagicNumbers
  WORLD_SEED="%d"
  DEBUG_NO_PAUSE_ON_WINDOW_FOCUS_LOST="1"
  DEBUG_SINGLE_THREADED_LOADING="1"
  DEBUG_THREADED_WORLD_CREATION="0"
  BOX2D_THREAD_MAX_WAIT_IN_MS="100000"
  GRID_FLEXIBLE_MAX_UPDATES="0"
  %s
/>]], session.seed, solo and "" or 'STREAMING_ENABLED="0" GRID_MAX_UPDATES_PER_FRAME="100000"')
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
    -- The camera is driven by us from now on (sim camera inside the window, own outside).
    GameSetCameraFree(true)
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

-- Focus rectangles (one per player slot, slot order): chunks there stay loaded and simulated.
local FOCUS_HW, FOCUS_HH = 400, 300
local UNLOAD_MARGIN, UNLOAD_EVERY, UNLOAD_MAX = 512, 30, 2
local ffi = require("ffi")
local foci_buf = ffi.new("int32_t[128]")
local centre_buf = ffi.new("float[64]")

local function current_foci()
  local foci = {}
  for _, slot in ipairs(Players.slots) do
    if EntityGetIsAlive(slot.entity) then
      local x, y = EntityGetTransform(slot.entity)
      x, y = math.floor(x), math.floor(y)
      foci[#foci + 1] = { x - FOCUS_HW, y - FOCUS_HH, x + FOCUS_HW, y + FOCUS_HH, x = x, y = y }
    end
  end
  return foci
end

-- Inside the simulation window every client uses the same camera (first live player),
-- because camera-dependent engine systems feed the simulation; rendering uses our own.
local render_cam = nil
local function set_sim_camera(foci)
  if foci[1] then GameSetCameraPos(foci[1].x, foci[1].y) end
end
local function set_render_camera()
  local slot = Players.local_slot()
  if not slot or not EntityGetIsAlive(slot.entity) then return end
  local x, y = EntityGetTransform(slot.entity)
  if not render_cam then render_cam = { x = x, y = y } end
  render_cam.x = render_cam.x + (x - render_cam.x) * 0.2
  render_cam.y = render_cam.y + (y - render_cam.y) * 0.2
  GameSetCameraPos(render_cam.x, render_cam.y)
end

local in_sim_window = false

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
  NO.lib.no_sim_enter()
  in_sim_window = true
  if f == 1 then NO.lib.no_fix_seeds(session.seed) end
  local waited = NO.lib.no_chunk_prewait()
  if waited > 200 then log(string.format("chunk generation wait %d ms at frame %d", waited, f)) end
  if ready then
    local foci = current_foci()
    set_sim_camera(foci)
    Players.apply_tick(f, tick)
    for _, r in ipairs(foci) do NO.lib.no_stream_focus(r[1], r[2], r[3], r[4]) end
    for i, r in ipairs(foci) do
      centre_buf[2 * (i - 1)] = r.x
      centre_buf[2 * (i - 1) + 1] = r.y
    end
    NO.lib.no_focus_freeze(centre_buf, #foci)
    if session.scenario == "bot" then Bot.world_events(f, Players.slots) end
  end
end

function OnWorldPostUpdate()
  if finished then return end
  local f = GameGetFrameNum()
  if in_sim_window then
    if ready and f % UNLOAD_EVERY == 0 then
      local foci = current_foci()
      for i, r in ipairs(foci) do
        for k = 1, 4 do foci_buf[(i - 1) * 4 + k - 1] = r[k] end
      end
      NO.lib.no_unload_far(foci_buf, #foci, UNLOAD_MARGIN, UNLOAD_MAX)
    end
    -- State reports: entity digest + tracked positions every 60 frames; chunk hashes either
    -- staggered (each loaded chunk hashed once per 60 frames, a few per frame: no frame-time
    -- spike) or, in "full" mode, every loaded chunk at once on the 60-frame checkpoint.
    local dense = cfg.dense_from and f >= cfg.dense_from and f <= cfg.dense_to
    local checkpoint = f % 60 == 0
    if ready and (checkpoint or dense or not cfg.full_hash) then
      local h, positions = 0, {}
      if checkpoint or dense then h, positions = State.collect(Players.proxy, dense) end
      local period = (cfg.full_hash or dense) and 0 or 60
      local us = NO.report_state(f, period, (checkpoint or dense) and 1 or 0, h, positions)
      if f % 600 == 0 or us > 8000 then
        local st = ffi.new("uint32_t[3]")
        NO.lib.no_chunk_stats(st)
        log(string.format("state report f=%d chunk-hash %.2f ms; chunk waits %d (%d ms), in-lock fallbacks %d", f, us / 1000, st[0], st[1], st[2]))
      end
    end
    NO.lib.no_sim_leave()
    in_sim_window = false
    if ready then set_render_camera() end
  end
  if f % 600 == 0 then drain_events() end
  if cfg.debug and ready and f % 60 == 0 then
    local slot = Players.local_slot()
    if slot and EntityGetIsAlive(slot.entity) then
      local x, y = EntityGetTransform(slot.entity)
      local pressed = {}
      for k, v in pairs(slot.prev.buttons) do if v then pressed[#pressed + 1] = k end end
      table.sort(pressed)
      log(string.format("debug f=%d local=(%.1f,%.1f) buttons=%s mouse=(%.1f,%.1f)", f, x, y, table.concat(pressed, "+"), slot.prev.mx, slot.prev.my))
    end
  end
  if cfg.duration and ready and f >= cfg.duration then
    finished = true
    quit_game("duration reached at frame " .. f)
  end
end
