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
if cfg.log_path then logf = io.open(cfg.log_path, "a") end
local function log(msg)
  local line = "[noita_online] " .. msg
  print(line)
  if logf then logf:write(line, "\n") logf:flush() end
end

local LatProbe = dofile_once(ROOT .. "latprobe.lua")
if LatProbe.on then LatProbe.open(cfg.lat_path) end

if not cfg.enabled then
  if LatProbe.on then
    -- Vanilla latency reference (no session, no patches): the probe watches the engine's own
    -- player, at the same frame cap as the test sessions.
    log("latency probe on vanilla play")
    local NO = dofile_once(ROOT .. "native.lua")
    if cfg.fps_cap then NO.lib.no_set_frame_cap(cfg.fps_cap) end
    function OnWorldPreUpdate()
      NO.lib.no_frame_mark()
      LatProbe.pre(GameGetFrameNum())
    end
    function OnWorldPostUpdate()
      LatProbe.post((EntityGetWithTag("player_unit") or {})[1])
    end
    return
  end
  log("disabled (no server configured)")
  return
end

local NO = dofile_once(ROOT .. "native.lua")
if cfg.fps_cap then NO.lib.no_set_frame_cap(cfg.fps_cap) end
local Input = dofile_once(ROOT .. "input.lua")
local Players = dofile_once(ROOT .. "players.lua")
local State = dofile_once(ROOT .. "state.lua")
local Bot = dofile_once(ROOT .. "bot.lua")
Players.log = log
Bot.log = log

local session = nil   -- { seed, input_delay, player, players, scenario }
local solo = cfg.solo
local finished = false

-- Diagnostics only (docs/testing.md, "Cost ladder"): NOITA_ONLINE_LADDER=flag,... removes a
-- piece of the session from a 1-player run, or adds one to the solo baseline, to measure its
-- frame cost. Never set in normal play.
local LADDER = {}
for w in (os.getenv("NOITA_ONLINE_LADDER") or ""):gmatch("[%w_]+") do
  LADDER[w] = true
  if w:match("^nudge%d+$") then LADDER.nudge = tonumber(w:sub(6)) end
end

-- In a session the vanilla scripts' "near the camera" tests answer for every player's view
-- (camera_bounds.lua, answered by the `lua_camera_bounds` patch); the radar perk marks
-- enemies on its holder's own screen. The solo baseline (no patches) stays vanilla.
if not solo then
  ModLuaFileAppend("data/scripts/lib/utilities.lua", ROOT .. "camera_bounds.lua")
  local radar = "data/scripts/perks/radar.lua"
  local text, n = ModTextFileGetContent(radar):gsub("is_in_camera_bounds%(enemy_x, enemy_y, %-4%)",
    "noita_online_in_camera_bounds_of(pos_x, pos_y, enemy_x, enemy_y, -4)")
  if n == 1 then ModTextFileSetContent(radar, text) else log("radar.lua: camera test not found (" .. n .. ")") end
end

-- Vanilla scripts' player-scoped messages show only for this client's own player
-- (local_prints.lua); the setting naming that player starts empty (every message shows).
local LocalPrints = dofile_once(ROOT .. "local_prints.lua")
LocalPrints.set_local(nil)
if not solo then LocalPrints.install(log) end

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
if NO.lib.no_status() ~= NO.STATUS.idle then
  -- A new game in the same process (e.g. after a game over): the lockstep session belongs
  -- to the previous world, which only this client left, so it cannot continue it.
  log("new game after a session in this process: leaving the session, playing offline")
  NO.lib.no_shutdown()
  GamePrint("Noita Online: session ended (new game started); restart Noita to join again")
  return
end
if solo then
  session = { seed = cfg.solo_seed, input_delay = 0, player = 0, players = { 0 }, scenario = cfg.bot and "bot" or "" }
  log("solo baseline mode, seed " .. session.seed)
  NO.lib.no_prof_install()
else
  local count = NO.lib.no_apply_patches()
  if count < 0 then
    log("FATAL: determinism patches failed: " .. NO.last_error())
    error("noita_online: " .. NO.last_error())
  end
  log(string.format("determinism patches applied (%d)", count))
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
  log(string.format("session started: seed=%.0f delay=%d me=%d players=%s", info.seed, info.input_delay, info.player, table.concat(info.players, ",")))
end

-- Lockstep sessions stream chunks around every player (foci) with a deterministic unloader
-- instead of the per-client camera and wall-clock unloader. The grid-update cap stays: it
-- prefers tiles nearest the camera, which is the shared simulation camera in the window.
local magic = string.format([[<MagicNumbers
  WORLD_SEED="%.0f"
  DEBUG_NO_PAUSE_ON_WINDOW_FOCUS_LOST="1"
  DEBUG_SINGLE_THREADED_LOADING="1"
  DEBUG_THREADED_WORLD_CREATION="0"
  GRID_FLEXIBLE_MAX_UPDATES="0"
  %s
/>]], session.seed, solo and "" or 'STREAMING_ENABLED="0"')
ModTextFileSetContent("mods/noita_online/files/session_magic.xml", magic)
ModMagicNumbersFileAdd("mods/noita_online/files/session_magic.xml")

function OnMagicNumbersAndWorldSeedInitialized()
  if not solo then NO.lib.no_fix_seeds(session.seed) end
end

function OnWorldInitialized()
  if not solo then NO.lib.no_fix_seeds(session.seed) end
end

function OnPlayerDied(player)
  log("OnPlayerDied entity=" .. tostring(player) .. " frame=" .. GameGetFrameNum())
end

function OnPausedChanged(is_paused, is_inventory_pause)
  log(string.format("pause changed: paused=%s inventory=%s frame=%d", tostring(is_paused), tostring(is_inventory_pause), GameGetFrameNum()))
end

local ready = false

function OnPlayerSpawned(player)
  if ready then return end
  if solo then
    Players.setup_solo(player)
    if LADDER.proxy then Players.ladder_proxy(player) end
    if LADDER.freecam then GameSetCameraFree(true) end
  else
    Players.setup(player, session.players, session.player)
    -- The camera is driven by us from now on (sim camera inside the window, own outside).
    GameSetCameraFree(true)
  end
  ready = true
  log("players ready at frame " .. GameGetFrameNum())
end

-- The mouse in world coordinates as this client sees it, for a frame without the client
-- runtime's early input update (Players.capture_local): the engine then mapped the mouse to
-- the world inside the simulation window, i.e. through the simulation camera (first player),
-- so it is mapped again through the render camera (OnWorldPostUpdate).
local render_mouse = nil

local function local_input(f)
  if not ready then return Input.NEUTRAL end
  if cfg.bot then
    local slot = Players.local_slot()
    local px, py = 0, 0
    if slot and EntityGetIsAlive(slot.entity) then px, py = EntityGetTransform(slot.entity) end
    local s = Players.bot_ops(Bot.input(session.player, f, px, py))
    -- Diagnostics (LADDER nudge<N>): one extra frame of "Right" at frame N sends the bot down
    -- another trajectory (deterministic per N), to sample session trajectories like the solo
    -- baseline's run-to-run ones.
    if f == LADDER.nudge then s.buttons.Right = true end
    return s
  end
  return Players.capture_local(render_mouse)
end

local lockstep_alive = true

-- The input captured while simulating frame f goes out for frame f + delay. The server may
-- change the delay during the session (it follows the measured network margins): a longer
-- delay leaves one frame without an input, which gets the current input held one more frame;
-- a shorter one finds this frame's slot already sent, so the input waits and is merged into
-- the next frame's. Either way every frame gets exactly one input and nothing pressed is lost.
local last_sent = nil -- last frame an input was sent for
local carry = nil     -- input held back by a shorter delay
local delay_now = nil

-- Returns the input and the frame it was sent for (nil: held back).
local function send_local_input(f)
  local s = local_input(f)
  if carry then s, carry = Input.merge(carry, s), nil end
  local d = NO.lib.no_input_delay()
  if d < 0 then d = session.input_delay end
  if d ~= delay_now then
    if delay_now then log(string.format("input delay %d -> %d at frame %d", delay_now, d, f)) end
    delay_now = d
  end
  local target = f + d
  last_sent = last_sent or (session.input_delay - 1)
  if target <= last_sent then
    carry = s
    return s, nil
  end
  for g = last_sent + 1, target - 1 do
    NO.send_input(g, Input.encode(s))
    s = Input.hold(s)
  end
  NO.send_input(target, Input.encode(s))
  last_sent = target
  return s, target
end

-- Focus rectangles (one per player slot, slot order): chunks there stay loaded and simulated;
-- chunks far from every player are unloaded as vanilla's streaming does (no_evict_chunks).
local FOCUS_HW, FOCUS_HH = 400, 300
local ffi = require("ffi")
local foci_buf = ffi.new("int32_t[128]")
local centre_buf = ffi.new("float[64]")

local function current_foci()
  local foci = {}
  for _, slot in ipairs(Players.slots) do
    if EntityGetIsAlive(slot.entity) then
      local x, y = EntityGetTransform(slot.entity)
      x, y = math.floor(x), math.floor(y)
      foci[#foci + 1] = { x - FOCUS_HW, y - FOCUS_HH, x + FOCUS_HW, y + FOCUS_HH, x = x, y = y, own = slot.id == Players.local_id }
    end
  end
  return foci
end

-- Inside the simulation window every client uses the same camera (first live player),
-- because camera-dependent engine systems feed the simulation; rendering uses our own.
local function set_sim_camera(foci)
  -- Forced: the camera is at the same place on every client for the whole window.
  if foci[1] then NO.lib.no_set_sim_camera(foci[1].x, foci[1].y) end
end
-- After the window: the view is the camera Noita computed for our player in the window
-- (Players.local_camera_target), set like the engine's own camera update does (unforced).
local function set_render_camera()
  local x, y = Players.local_camera_target()
  if not x then
    local slot = Players.local_slot()
    if not slot or not EntityGetIsAlive(slot.entity) then return end
    x, y = EntityGetTransform(slot.entity)
  end
  GameSetCameraPos(x, y)
end

-- GameSetCameraFree(true) makes the engine load its debug free-camera light (two white lights
-- of radius 750, data/entities/_debug/free_camera_light.xml) and keep it at the camera
-- (FUN_006b3ff0). Vanilla play has no such light, so it is switched off, identically on every
-- client (inside the window; the entity is part of the simulation).
local free_light = nil
local function douse_free_camera_light()
  if free_light then return end
  local cx, cy = GameGetCameraPos()
  for _, e in ipairs(EntityGetInRadius(cx, cy, 32) or {}) do
    if EntityGetFilename(e) == "data/entities/_debug/free_camera_light.xml" then
      for _, c in ipairs(EntityGetComponentIncludingDisabled(e, "LightComponent") or {}) do
        ComponentSetValue2(c, "radius", 0)
        ComponentSetValue2(c, "r", 0)
        ComponentSetValue2(c, "g", 0)
        ComponentSetValue2(c, "b", 0)
      end
      free_light = e
    end
  end
end

local ENTLOG = os.getenv("NOITA_ONLINE_ENTLOG")
local PCLOG = os.getenv("NOITA_ONLINE_PCLOG")

local in_sim_window = false

function OnWorldPreUpdate()
  if finished then return end
  local f = GameGetFrameNum()
  NO.lib.no_frame_mark()
  if solo then
    if ready then
      local s = local_input(f)
      Players.apply_tick(f, { [0] = Input.encode(s) })
      if session.scenario == "bot" then Bot.world_events(f, Players.slots)
      elseif session.scenario == "items" then Bot.items_events(f, Players.slots) end
    end
    return
  end
  if not lockstep_alive then return end
  LatProbe.pre(f)
  local s, target = send_local_input(f)
  if target then LatProbe.sent(target, s, render_mouse) end
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
    if ENTLOG and cfg.dense_from and f >= cfg.dense_from and f <= cfg.dense_to and Players.slots[1] then
      local x, y = EntityGetTransform(Players.slots[1].entity)
      local cx, cy = GameGetCameraPos()
      log(string.format("prelog f=%d p0=%.6f,%.6f cam=%.4f,%.4f", f, x, y, cx, cy))
    end
    Players.apply_tick(f, tick)
    if not LADDER.nolocal then Players.feed_local_camera() end
    for i, r in ipairs(foci) do
      NO.lib.no_stream_focus(r[1], r[2], r[3], r[4])
      for k = 1, 4 do foci_buf[(i - 1) * 4 + k - 1] = r[k] end
    end
    NO.lib.no_evict_chunks(foci_buf, #foci, f)
    local own = -1
    for i, r in ipairs(foci) do
      centre_buf[2 * (i - 1)] = r.x
      centre_buf[2 * (i - 1) + 1] = r.y
      if r.own then own = i - 1 end
    end
    NO.lib.no_focus_freeze(centre_buf, #foci)
    NO.lib.no_local_view(own)
    if session.scenario == "bot" then Bot.world_events(f, Players.slots)
    elseif session.scenario == "items" then Bot.items_events(f, Players.slots) end
  end
end

function OnWorldPostUpdate()
  if finished then return end
  local f = GameGetFrameNum()
  if in_sim_window then
    if ready then douse_free_camera_light() end
    if LatProbe.on then
      local slot = Players.local_slot()
      LatProbe.post(slot and slot.entity)
    end
    if ready and PCLOG then
      -- Diagnostics: every slot's PlayerCollision and CharacterData state, every frame.
      for i, slot in ipairs(Players.slots) do
        if EntityGetIsAlive(slot.entity) then
          local x, y = EntityGetTransform(slot.entity)
          local parts = { string.format("pclog f=%d slot=%d e=%d pos=%.6f,%.6f", f, i, slot.entity, x, y) }
          for _, name in ipairs({ "PlayerCollisionComponent", "CharacterDataComponent" }) do
            local c = EntityGetFirstComponentIncludingDisabled(slot.entity, name)
            local m = c and ComponentGetMembers(c) or {}
            local keys = {}
            for k in pairs(m) do keys[#keys + 1] = k end
            table.sort(keys)
            for _, k in ipairs(keys) do parts[#parts + 1] = k .. "=" .. tostring(m[k]) end
          end
          log(table.concat(parts, " "))
        end
      end
    end
    -- State reports: entity digest + tracked positions every 60 frames; chunk hashes either
    -- staggered (each loaded chunk hashed once per 60 frames, a few per frame: no frame-time
    -- spike) or, in "full" mode, every loaded chunk at once on the 60-frame checkpoint.
    local dense = cfg.dense_from and f >= cfg.dense_from and f <= cfg.dense_to
    local checkpoint = f % 60 == 0
    if ready and not LADDER.noreport and (checkpoint or dense or not cfg.full_hash) then
      local h, positions = 0, {}
      if checkpoint or dense then h, positions = State.collect(Players.proxy, dense) end
      local period = (cfg.full_hash or dense) and 0 or 60
      local us = NO.report_state(f, period, (checkpoint or dense) and 1 or 0, h, positions)
      if dense and ENTLOG then
        -- Diagnostics: NOITA_ONLINE_ENTLOG=id[,id...] logs those entities every dense frame.
        for id in ENTLOG:gmatch("%d+") do
          local e = tonumber(id)
          if EntityGetIsAlive(e) then
            local x, y, r, sx, sy = EntityGetTransform(e)
            log(string.format("entlog f=%d id=%d file=%s name=%s tags=%s parent=%d pos=%.4f,%.4f r=%.4f s=%.3f,%.3f",
              f, e, EntityGetFilename(e), EntityGetName(e), EntityGetTags(e) or "", EntityGetParent(e), x, y, r or 0, sx or 0, sy or 0))
            if os.getenv("NOITA_ONLINE_ENTLOG_COMPS") then
              -- Every component's scalar members, sorted, one line per component.
              for _, c in ipairs(EntityGetAllComponents(e) or {}) do
                local m = ComponentGetMembers(c) or {}
                local keys = {}
                for k in pairs(m) do keys[#keys + 1] = k end
                table.sort(keys)
                local parts = {}
                for _, k in ipairs(keys) do parts[#parts + 1] = k .. "=" .. tostring(m[k]) end
                log(string.format("entcomp f=%d id=%d c=%d %s en=%s %s", f, e, c, ComponentGetTypeName(c),
                  tostring(ComponentGetIsEnabled(c)), table.concat(parts, " ")))
              end
              for _, ch in ipairs(EntityGetAllChildren(e) or {}) do
                local cx, cy = EntityGetTransform(ch)
                log(string.format("entchild f=%d id=%d child=%d file=%s pos=%.4f,%.4f", f, e, ch, EntityGetFilename(ch), cx, cy))
              end
            end
          end
        end
      end
      if f % 600 == 0 or us > 8000 then
        local st = ffi.new("uint32_t[6]")
        NO.lib.no_chunk_stats(st)
        local mem = ffi.new("uint32_t[2]")
        NO.lib.no_mem_stats(mem)
        log(string.format("state report f=%d chunk-hash %.2f ms; chunk waits %d (%d ms), in-lock fallbacks %d (longest %d ms, last f=%d, timed out %d); mem %d MiB, %d chunks loaded",
          f, us / 1000, st[0], st[1], st[2], st[3], st[4], st[5], mem[0], mem[1]))
      end
    end
    NO.lib.no_sim_leave()
    in_sim_window = false
    if ready then
      if not LADDER.norendercam then set_render_camera() end
      if not LADDER.nolocal then
        Players.update_reticle()
        render_mouse = { DEBUG_GetMouseWorld() }
      end
      Players.gui_update()
    end
  end
  if solo and ready and LADDER.freecam then
    douse_free_camera_light()
    set_render_camera()
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
