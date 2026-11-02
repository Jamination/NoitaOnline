-- DETERMINISM SPIKE A (grid determinism research, temporary)
-- env: SPIKE_PATCH=comma list (rand,serial,inline,...)  SPIKE_DUMP=frames to dump all chunk cells
--      SPIKE_SLEEP=ms per frame (default 150)  SPIKE_SLEEP_FRAMES=n (default 300)  SPIKE_PERF=1
local ffi = require("ffi")
local bit = require("bit")
ModMagicNumbersFileAdd("mods/spike_a/files/magic_spike.xml")
local root = os.getenv("NOITA_ONLINE_ROOT") or "."
local inst = os.getenv("NOITA_ONLINE_INSTANCE") or "x"
local PATCH = os.getenv("SPIKE_PATCH_" .. inst) or os.getenv("SPIKE_PATCH") or ""
local function has(p) return ("," .. PATCH .. ","):find("," .. p .. ",", 1, true) ~= nil end
local DUMPS = {}
for n in (os.getenv("SPIKE_DUMP") or ""):gmatch("%d+") do DUMPS[tonumber(n)] = true end
local SLEEP = tonumber(os.getenv("SPIKE_SLEEP") or "150")
local SLEEP_FRAMES = tonumber(os.getenv("SPIKE_SLEEP_FRAMES") or "300")
local PERF = os.getenv("SPIKE_PERF") == "1"
local PROBE = tonumber(os.getenv("SPIKE_PROBE") or "0")
local TPROBE = (os.getenv("SPIKE_TPROBE_" .. inst) or os.getenv("SPIKE_TPROBE")) == "1"
  and dofile("mods/spike_a/files/threadprobe.lua") or nil
local STALL = (os.getenv("SPIKE_STALL_" .. inst) or os.getenv("SPIKE_STALL")) == "1"
local HASH_EVERY_UNTIL = tonumber(os.getenv("SPIKE_EVERY") or "120")
local HASH_FROM = tonumber(os.getenv("SPIKE_FROM") or "1")
local PD_FROM, PD_TO = (os.getenv("SPIKE_PDUMP") or ""):match("(%d+)-(%d+)")
PD_FROM, PD_TO = tonumber(PD_FROM), tonumber(PD_TO)
local DONE_AT = tonumber(os.getenv("SPIKE_DONE") or "1260")
local POST_UNTIL = tonumber(os.getenv("SPIKE_POST") or tostring(DONE_AT + 540))
local logf = io.open(root .. "/run/logs/spike_" .. inst .. ".txt", "w")
local function log(s) logf:write(s, "\n") logf:flush() end
log("init patch=" .. PATCH .. " sleep=" .. SLEEP .. "x" .. SLEEP_FRAMES .. " perf=" .. tostring(PERF))

ffi.cdef([[
void __stdcall Sleep(uint32_t ms);
int __stdcall VirtualProtect(void* addr, size_t size, uint32_t newprot, uint32_t* oldprot);
void* __stdcall VirtualAlloc(void* addr, size_t size, uint32_t type, uint32_t prot);
uint32_t __stdcall GetCurrentThreadId(void);
int __stdcall QueryPerformanceCounter(int64_t* c);
int __stdcall QueryPerformanceFrequency(int64_t* f);
]])
local C = ffi.C
local function P(p) return tonumber(ffi.cast("uint32_t", p)) end
local function rd32(a) return ffi.cast("uint32_t*", a)[0] end
local function rdp(a) return ffi.cast("uint32_t*", a)[0] end

local function patch_bytes(addr, bytes)
  local old = ffi.new("uint32_t[1]")
  assert(C.VirtualProtect(ffi.cast("void*", addr), #bytes, 0x40, old) ~= 0, "VirtualProtect failed")
  local p = ffi.cast("uint8_t*", addr)
  for i, b in ipairs(bytes) do p[i - 1] = b end
  C.VirtualProtect(ffi.cast("void*", addr), #bytes, old[0], old)
end
local function le32(v)
  v = v % 0x100000000
  return { v % 256, math.floor(v / 256) % 256, math.floor(v / 65536) % 256, math.floor(v / 16777216) % 256 }
end
local function cat(...)
  local out = {}
  for _, t in ipairs({ ... }) do for _, b in ipairs(t) do out[#out + 1] = b end end
  return out
end
local function patch_u32(addr, v) patch_bytes(addr, le32(v)) end

local qf = ffi.new("int64_t[1]")
C.QueryPerformanceFrequency(qf)
local qfreq = tonumber(qf[0])
local qc = ffi.new("int64_t[1]")
local function now_ms() C.QueryPerformanceCounter(qc) return tonumber(qc[0]) * 1000 / qfreq end

-- noita.exe facts (build 0x6794ee3c)
local USE_CUSTOM_THREADPOOL = 0x01153177
local PLATFORM = 0x01221bc0
local GLOBAL_LCG = 0x0120741c
local WORLD_SEED = 0x01205004
local GG_ADDR = 0x0122374c

local det = dofile("mods/spike_a/files/det_patch.lua")
local inst_log = det.install({ rand = has("rand"), streamclock = has("streamclock"), chunkwait = has("chunkwait"),
  inline = has("inline"), pinit = has("pinit") })
if os.getenv("SPIKE_REINSTALL") == "1" then
  -- simulate a re-run of init.lua in the same process (world restart): must be idempotent
  inst_log = inst_log .. " | reinstall: " .. det.install({ rand = has("rand"), streamclock = has("streamclock"),
    chunkwait = has("chunkwait"), inline = has("inline"), pinit = has("pinit") })
end
local EXEC = det.page
local RAND_STATE, RAND_COUNT, SRAND_LAST = det.rand_state, det.rand_count, det.srand_last
local chunkwait_stats = { calls = 0, waited = 0, ms = 0, maxms = 0, timeouts = 0, other = 0 }
log("patched=" .. inst_log .. string.format(" exec=%08x", EXEC))
-- SPIKE_TRACE=from-to: native entry trace of particle/explosion functions, dumped per frame (tracer.lua)
local TR_FROM, TR_TO = (os.getenv("SPIKE_TRACE") or ""):match("(%d+)-(%d+)")
TR_FROM, TR_TO = tonumber(TR_FROM), tonumber(TR_TO)
local TRACER
if TR_FROM then
  TRACER = dofile("mods/spike_a/files/tracer.lua")
  log("tracer " .. TRACER.install({
    { addr = 0x007168c0, orig = "558bec83ec20", tag = 1 },     -- AddAsParticle (cell -> particle)
    { addr = 0x00716af0, orig = "558bec83ec14", tag = 2 },     -- create particle
    { addr = 0x00717400, orig = "558bec803da44b200100", tag = 3 }, -- particle slot (lists 0x34/0x4c)
    { addr = 0x007174e0, orig = "558bec803da44b200100", tag = 4 }, -- particle slot (lists 0x40/0x4c)
    { addr = 0x00719670, orig = "558bec83e4f0", tag = 5 },     -- explosions / physics-bridge step
  }))
end

local function gg() return ffi.cast("char**", GG_ADDR)[0] end
local function fnv(h, v)
  h = bit.bxor(h, v)
  return bit.tobit(h * 16777619)
end

local function grid_world()
  local game_global = gg()
  if P(game_global) == 0 then return nil end
  local world_data = ffi.cast("char**", game_global + 0xc)[0]
  if P(world_data) == 0 then return nil end
  local gw = ffi.cast("char**", world_data + 0x44)[0]
  if P(gw) == 0 then return nil end
  return gw
end

local dumpbuf = ffi.new("uint16_t[262144]")
local function world_hash(dumpframe)
  local gw = grid_world()
  if not gw then return 0, 0, {} end
  local game_global = gg()
  local chunk_map = gw + 0x500
  local table_ = ffi.cast("uint32_t**", chunk_map + 8)[0]
  local cf = ffi.cast("char**", game_global + 0x18)[0]
  local mat_begin = ffi.cast("uint32_t*", cf + 0x18)[0]
  local total = 0
  local nchunks = 0
  local parts = {}
  local df = nil
  if dumpframe then df = io.open(root .. "/run/re_grid/dump_" .. inst .. "_" .. dumpframe .. ".bin", "wb") end
  for idx = 0, 0x40000 - 1 do
    local chunk = table_[idx]
    if chunk ~= 0 then
      nchunks = nchunks + 1
      local cp = ffi.cast("uint32_t*", chunk)
      local cells = ffi.cast("uint32_t*", cp[0])
      local h = 0x811c9dc5
      local n = 0
      for i = 0, 0x40000 - 1 do
        local c = cells[i]
        if c ~= 0 then
          local mp = ffi.cast("uint32_t*", c)[5]
          local mat = (mp - mat_begin) / 0x290
          h = fnv(h, i)
          h = fnv(h, mat)
          n = n + 1
          if df then dumpbuf[i] = mat end
        elseif df then
          dumpbuf[i] = 0xffff
        end
      end
      if df then
        df:write(ffi.string(ffi.new("int32_t[1]", idx), 4))
        df:write(ffi.string(dumpbuf, 524288))
      end
      parts[#parts + 1] = string.format("%d:%08x:%d", idx, bit.tobit(h) % 0x100000000, n)
      total = fnv(total, h)
    end
  end
  if df then df:close() end
  return nchunks, total, parts
end

-- per-update-unit (GridWorldThreaded) state: coords + LCG at +4
local function units_state()
  local gw = grid_world()
  if not gw then return 0, 0, "" end
  local ti = rdp(gw + 0x5d0)
  if ti == 0 then return 0, 0, "" end
  local n = rd32(ti)
  local arr = rdp(ti + 4)
  local list = {}
  for i = 0, n - 1 do
    local u = rdp(arr + 4 * i)
    local ux = ffi.cast("int32_t*", u + 0x540)[0]
    local uy = ffi.cast("int32_t*", u + 0x544)[0]
    list[#list + 1] = { ux, uy, rd32(u + 4) }
  end
  table.sort(list, function(a, b) return a[2] < b[2] or (a[2] == b[2] and a[1] < b[1]) end)
  local h = 0x811c9dc5
  local s = {}
  for _, e in ipairs(list) do
    h = fnv(fnv(fnv(h, e[1]), e[2]), e[3])
    s[#s + 1] = string.format("%d,%d:%08x", e[1], e[2], e[3])
  end
  return n, bit.tobit(h) % 0x100000000, table.concat(s, " ")
end

local function entity_hash()
  local ents = EntityGetInRadius(0, 0, 100000000)
  local h = 0x811c9dc5
  for _, e in ipairs(ents) do
    local x, y = EntityGetTransform(e)
    h = fnv(h, e)
    h = fnv(h, math.floor(x * 16))
    h = fnv(h, math.floor(y * 16))
  end
  return #ents, h
end

local fbits_buf = ffi.new("float[1]")
local function fbits(v)
  fbits_buf[0] = v
  return ffi.cast("uint32_t*", fbits_buf)[0]
end
-- exact (float-bit) entity transform hash: catches sub-1/16-pixel divergence the coarse hash misses
local function entity_hash_exact()
  local ents = EntityGetInRadius(0, 0, 100000000)
  local h = 0x811c9dc5
  for _, e in ipairs(ents) do
    local x, y, r = EntityGetTransform(e)
    h = fnv(fnv(fnv(fnv(h, e), fbits(x)), fbits(y)), fbits(r or 0))
  end
  return bit.tobit(h) % 0x100000000
end

local function ent_list()
  local ents = EntityGetInRadius(0, 0, 100000000)
  local out = {}
  for _, e in ipairs(ents) do
    local x, y, r = EntityGetTransform(e)
    out[#out + 1] = string.format("%d:%s:%08x,%08x,%08x", e, EntityGetFilename(e), fbits(x), fbits(y), fbits(r or 0))
  end
  return out
end

local function particles_str()
  local gw = grid_world()
  if not gw then return "gp=nil" end
  local gp = rdp(gw + 0x540)
  if gp == 0 then return "gp=0" end
  local cfg = rdp(gp + 0x2c)
  local q = cfg ~= 0 and ffi.cast("float*", cfg + 0xa8)[0] or -1
  local n1 = (rd32(gp + 0x38) - rd32(gp + 0x34)) / 0x70
  local n2 = (rd32(gp + 0x44) - rd32(gp + 0x40)) / 0x70
  local n3 = (rd32(gp + 0x50) - rd32(gp + 0x4c)) / 0x70
  local n4 = (rd32(gp + 0xa0) - rd32(gp + 0x9c)) / 0x18
  local g = P(gg())
  local cam = rdp(g + 0xc)
  local gb = ffi.cast("int32_t*", gw + 0x498)
  return string.format("pq=%.4f np=%d/%d/%d/%d vb=%08x,%08x,%08x,%08x cam=%08x,%08x,%08x,%08x gb=%d,%d,%d,%d,%d,%d,%d,%d",
    q, n1, n2, n3, n4, rd32(g + 0x180), rd32(g + 0x184), rd32(g + 0x188), rd32(g + 0x18c),
    rd32(cam), rd32(cam + 4), rd32(cam + 8), rd32(cam + 12), gb[0], gb[1], gb[2], gb[3], gb[4], gb[5], gb[6], gb[7])
end

local function dump_particles(tag)
  local gw = grid_world()
  if not gw then return end
  local gp = rdp(gw + 0x540)
  if gp == 0 then return end
  local f = io.open(root .. "/run/re_grid/parts_" .. inst .. "_" .. tag .. ".bin", "wb")
  for _, off in ipairs({ 0x34, 0x40, 0x4c, 0x9c }) do
    local b, e = rd32(gp + off), rd32(gp + off + 4)
    f:write(ffi.string(ffi.new("uint32_t[2]", off, e - b), 8))
    if e > b then f:write(ffi.string(ffi.cast("void*", b), e - b)) end
  end
  f:close()
end

local function globals_str()
  local cw = chunkwait_stats
  local okx, ex = pcall(entity_hash_exact)
  if not okx then ex = 0 end
  return string.format("plat=%08x pseed=%08x glcg=%08x wseed=%08x rand=%08x/%d srand=%08x tp=%d prng=%.0f entx=%08x cw=%d/%d %s",
    rd32(PLATFORM + 0xa0), rd32(PLATFORM + 0xa4), rd32(GLOBAL_LCG), rd32(WORLD_SEED),
    rd32(RAND_STATE), rd32(RAND_COUNT), rd32(SRAND_LAST), ffi.cast("uint8_t*", USE_CUSTOM_THREADPOOL)[0],
    ffi.cast("double*", 0x01222388)[0], ex, cw.calls, cw.waited, particles_str() ..
    string.format(" gt=%08x", P(gg()) ~= 0 and rd32(P(gg()) + 8) or 0))
end

local function fix_seeds(where)
  if not has("seeds") then return end
  det.reseed(123456789)
  log("fix_seeds at " .. where .. " " .. globals_str())
end

function OnWorldInitialized()
  log("OnWorldInitialized gf=" .. GameGetFrameNum() .. " " .. globals_str())
  fix_seeds("OnWorldInitialized")
end

function OnMagicNumbersAndWorldSeedInitialized()
  log("OnMagicNumbersAndWorldSeedInitialized " .. globals_str())
  fix_seeds("OnMagicNumbersAndWorldSeedInitialized")
end

local function apply_frame_patches(f)
  if os.getenv("SPIKE_CAMLOCK") == "1" then
    -- camera follows mouse look-ahead (per-window input) -> pin it to the player deterministically
    local p = EntityGetWithTag("player_unit")[1]
    if p then
      local x, y = EntityGetTransform(p)
      GameSetCameraFree(true)
      GameSetCameraPos(x, y)
    end
  end
  if has("serial") then ffi.cast("uint8_t*", USE_CUSTOM_THREADPOOL)[0] = 0 end
  if f == 1 then fix_seeds("frame1") end
end

function OnPlayerSpawned(player)
  local controls = EntityGetFirstComponentIncludingDisabled(player, "ControlsComponent")
  if controls then ComponentSetValue2(controls, "enabled", false) end
  log("spawned frame=" .. GameGetFrameNum() .. " player=" .. player .. " " .. globals_str())
  log(string.format("magic BOX2D_THREAD_MAX_WAIT_IN_MS@01153288=%f FREEZE_STUCK@011532f4=%d tickscale@01207138=%.9g",
    ffi.cast("float*", 0x01153288)[0], ffi.cast("uint8_t*", 0x011532f4)[0], ffi.cast("double*", 0x01207138)[0]))
end

-- SPIKE_STRESS=1: deterministic heavy activity (liquids/powders across tile borders, enemies, bombs, explosive props)
local STRESS = os.getenv("SPIKE_STRESS") == "1"
local sx, sy
local function stress(f)
  if not STRESS then return end
  if f == 30 then
    local p = EntityGetWithTag("player_unit")[1]
    if p then sx, sy = EntityGetTransform(p) end
    log(string.format("stress origin %s,%s", tostring(sx), tostring(sy)))
  end
  if not sx then return end
  local x, y = sx, sy
  if f == 60 then
    for i = -4, 4 do GameCreateParticle("water", x + i * 40, y - 120, 300, 0, 0, false, false) end
  elseif f == 90 then
    for i = -4, 4 do GameCreateParticle("sand", x + i * 37, y - 140, 300, 0, 0, false, false) end
    GameCreateParticle("oil", x + 100, y - 100, 300, 0, 0, false, false)
    GameCreateParticle("lava", x - 100, y - 100, 200, 0, 0, false, false)
    GameCreateParticle("acid", x + 30, y - 160, 200, 0, 0, false, false)
  elseif f == 150 then
    EntityLoad("data/entities/animals/zombie.xml", x + 120, y - 20)
    EntityLoad("data/entities/animals/shotgunner.xml", x - 140, y - 20)
    EntityLoad("data/entities/animals/rat.xml", x + 60, y - 20)
    EntityLoad("data/entities/animals/miner.xml", x - 60, y - 30)
  elseif f == 240 then
    EntityLoad("data/entities/projectiles/bomb.xml", x + 80, y - 40)
    EntityLoad("data/entities/projectiles/bomb.xml", x - 90, y - 60)
    EntityLoad("data/entities/props/physics_propane_tank.xml", x + 160, y - 40)
    EntityLoad("data/entities/props/physics_box_explosive.xml", x - 170, y - 40)
  elseif f == 420 then
    EntityLoad("data/entities/projectiles/deck/fireball.xml", x + 40, y - 80)
    for i = -3, 3 do GameCreateParticle("water", x + i * 50, y - 200, 400, 0, 0, false, false) end
  elseif f == 600 then
    EntityLoad("data/entities/projectiles/bomb.xml", x + 10, y - 100)
    EntityLoad("data/entities/animals/shotgunner.xml", x + 200, y - 30)
  end
end

-- SPIKE_STRESS=2: long harsh scenario driven only by the local frame number (deterministic on every instance)
local ENEMIES = { "data/entities/animals/zombie.xml", "data/entities/animals/shotgunner.xml",
  "data/entities/animals/rat.xml", "data/entities/animals/miner.xml", "data/entities/animals/slimeshooter.xml" }
local PROPS = { "data/entities/props/physics_crate.xml", "data/entities/props/physics_barrel_oil.xml",
  "data/entities/props/physics_box_explosive.xml", "data/entities/props/physics_propane_tank.xml",
  "data/entities/props/physics_barrel_water.xml", "data/entities/props/physics_box_harmless.xml" }
local LIQUIDS = { "water", "oil", "acid", "lava", "blood", "sand", "gunpowder" }
local stress2_player
local function shoot(file, x, y, tx, ty)
  local e = EntityLoad(file, x, y)
  if e and e ~= 0 then GameShootProjectile(stress2_player or 0, x, y, tx, ty, e) end
  return e
end
local function stress2(f)
  if os.getenv("SPIKE_STRESS") ~= "2" then return end
  local p = EntityGetWithTag("player_unit")[1]
  stress2_player = p
  if f == 30 and p then
    sx, sy = EntityGetTransform(p)
    log(string.format("stress2 origin %s,%s", tostring(sx), tostring(sy)))
  end
  if not sx or f < 60 then return end
  local px, py = sx, sy
  if p then
    px, py = EntityGetTransform(p)
    if f % 60 == 0 then -- keep the (input-less) player alive
      local dm = EntityGetFirstComponentIncludingDisabled(p, "DamageModelComponent")
      if dm then
        ComponentSetValue2(dm, "max_hp", 40000)
        ComponentSetValue2(dm, "hp", 40000)
      end
    end
  end
  local k = math.floor(f / 240)
  local ph = f % 240
  local off = (k * 37) % 300 - 150
  if ph == 0 then
    EntityLoad("data/entities/projectiles/bomb.xml", px + off, py - 60)
    EntityLoad("data/entities/projectiles/bomb.xml", px - off * 0.5, py - 90)
  elseif ph == 40 then
    shoot("data/entities/projectiles/deck/tnt.xml", px + 20, py - 20, px + 200, py - 60)
  elseif ph == 80 then
    shoot("data/entities/projectiles/deck/fireball.xml", px - 20, py - 30, px - 200 + off, py + 20)
  elseif ph == 100 then
    for i = 0, 3 do
      GameCreateParticle(LIQUIDS[(k + i) % #LIQUIDS + 1], px + off + i * 30 - 45, py - 140, 250, 0, 0, false, false)
    end
  elseif ph == 120 then
    EntityLoad("data/entities/items/pickup/potion.xml", px + off * 0.7, py - 220)
    EntityLoad("data/entities/items/pickup/potion.xml", px - off * 0.3, py - 260)
  elseif ph == 160 then
    EntityLoad(PROPS[k % #PROPS + 1], px + off, py - 120)
    EntityLoad(PROPS[(k + 3) % #PROPS + 1], px - off, py - 140)
  elseif ph == 200 and k % 2 == 0 then
    EntityLoad(ENEMIES[(k / 2) % #ENEMIES + 1], px + 130 + (k % 3) * 20, py - 30)
  elseif ph == 220 then
    shoot("data/entities/projectiles/deck/circle_oil.xml", px, py - 10, px + off, py + 40)
  end
  -- player "digging": digger bolts every 20 frames in a rotating fan, black hole downwards every 240 frames
  if f >= 300 and f % 20 == 10 then
    local a = (f / 20) % 8
    local dirs = { { 1, 1 }, { 0, 1 }, { -1, 1 }, { 1, 0.3 }, { -1, 0.3 }, { 0.5, 1 }, { -0.5, 1 }, { 0, 1 } }
    local d = dirs[math.floor(a) + 1]
    shoot("data/entities/projectiles/deck/digger.xml", px, py - 5, px + d[1] * 100, py + d[2] * 100)
  end
  if f >= 300 and f % 240 == 130 then
    shoot("data/entities/projectiles/deck/black_hole.xml", px, py, px + (k % 2 == 0 and 30 or -30), py + 100)
  end
end

local lf = 0
local t_pre_end, t_last_pre = nil, nil
local perf = { n = 0, upd = 0, frame = 0, upd_max = 0 }
-- camera-area material snapshot used by cosmetic-particle collision: GridParticles(+0x24) -> {w,h,..,data@[6]}
local function snapshot_str()
  local gw = grid_world()
  if not gw then return "nil" end
  local gp = rdp(gw + 0x540)
  if gp == 0 then return "nogp" end
  local sb = rdp(gp + 0x24)
  if sb == 0 then return "nosb" end
  local s = ffi.cast("int32_t*", sb)
  local w, hgt = s[0], s[1]
  local d = ffi.cast("int32_t*", s[6])
  local h = 0
  if w > 0 and hgt > 0 and w * hgt <= 4194304 and s[6] ~= 0 then
    for i = 0, w * hgt - 1 do h = bit.tobit(h * 31 + d[i]) end
  end
  local gwi = ffi.cast("int32_t*", gw)
  return string.format("%dx%d@%d,%d:%08x", w, hgt, gwi[0x126], gwi[0x127], h % 0x100000000)
end
local snap_pre = ""
-- per-list content hashes of GridParticles lists (0x70-byte particles at +0x34/+0x40/+0x4c, 0x18-byte at +0x9c).
-- dword 20 (+0x50, heap pointer) excluded; byte 0x6c is a 2-bit bitfield (copy ops copy only bits 0..1), so bits 2..7
-- and bytes 0x6d..0x6f are padding (garbage, hashed separately as pg=).
local function particle_hashes(gp)
  local out = {}
  local gh = 0
  for _, off in ipairs({ 0x34, 0x40, 0x4c }) do
    local b, e = rd32(gp + off), rd32(gp + off + 4)
    local n = (e - b) / 0x70
    local p = ffi.cast("uint32_t*", b)
    local h = 0
    for i = 0, n - 1 do
      local q = p + i * 28
      for k = 0, 19 do h = bit.tobit(h * 31 + q[k]) end
      for k = 21, 26 do h = bit.tobit(h * 31 + q[k]) end
      h = bit.tobit(h * 31 + bit.band(q[27], 0x3))
      gh = bit.tobit(gh * 31 + bit.rshift(q[27], 2))
    end
    out[#out + 1] = string.format("%d:%08x", n, h % 0x100000000)
  end
  local b, e = rd32(gp + 0x9c), rd32(gp + 0xa0)
  local n = (e - b) / 0x18
  local p = ffi.cast("uint32_t*", b)
  local h = 0
  for i = 0, n * 6 - 1 do h = bit.tobit(h * 31 + p[i]) end
  out[#out + 1] = string.format("%d:%08x", n, h % 0x100000000)
  return "ph=" .. table.concat(out, "/") .. string.format(" t_pg=%08x", gh % 0x100000000)
end
local PDPOST = {}
for n in (os.getenv("SPIKE_PDPOST") or ""):gmatch("%d+") do PDPOST[tonumber(n)] = true end
local function dump_particles_post(f, gp)
  local fh = io.open(root .. "/run/re_race/parts_" .. inst .. "_" .. f .. ".bin", "wb")
  for _, off in ipairs({ 0x34, 0x40, 0x4c, 0x9c }) do
    local b, e = rd32(gp + off), rd32(gp + off + 4)
    fh:write(ffi.string(ffi.new("uint32_t[2]", off, e - b), 8))
    if e > b then fh:write(ffi.string(ffi.cast("void*", b), e - b)) end
  end
  fh:close()
end
function OnWorldPreUpdate()
  lf = lf + 1
  local f = lf
  local t0 = now_ms()
  apply_frame_patches(f)
  stress(f)
  stress2(f)
  if PROBE >= 3 then snap_pre = snapshot_str() end
  if f <= SLEEP_FRAMES and SLEEP > 0 then C.Sleep(SLEEP) end
  if f == 2 then
    local mf = io.open(root .. "/run/re_grid/materials_" .. inst .. ".txt", "w")
    for i = 0, 600 do
      local ok, name = pcall(CellFactory_GetName, i)
      if ok and name and name ~= "" then mf:write(i, " ", name, "\n") end
    end
    mf:close()
  end
  if not PERF and f >= HASH_FROM and f <= HASH_EVERY_UNTIL then
    local nc, wh, parts = world_hash(DUMPS[f] and tostring(f) or nil)
    if PD_FROM and f >= PD_FROM and f <= PD_TO then dump_particles(tostring(f)) end
    local ne, eh = entity_hash()
    local nu, uh, us = units_state()
    log(string.format("pre frame=%d chunks=%d world=%08x ents=%d ent=%08x gf=%d units=%d uh=%08x %s", f, nc,
      wh % 0x100000000, ne, eh % 0x100000000, GameGetFrameNum(), nu, uh, globals_str()))
    log("  " .. table.concat(parts, " "))
    log("  E " .. table.concat(ent_list(), " "))
    log("  U " .. us)
  end
  local t1 = now_ms()
  if PERF and t_last_pre and f > SLEEP_FRAMES + 10 then
    perf.frame = perf.frame + (t0 - t_last_pre)
  end
  t_last_pre = t0
  t_pre_end = t1
end

function OnWorldPostUpdate()
  local tpost = now_ms()
  if PERF and t_pre_end and lf > SLEEP_FRAMES + 10 then
    local d = tpost - t_pre_end
    perf.n = perf.n + 1
    perf.upd = perf.upd + d
    if d > perf.upd_max then perf.upd_max = d end
    if perf.n % 300 == 0 then
      log(string.format("perf n=%d avg_world_update=%.3fms max=%.3fms avg_frame=%.3fms", perf.n, perf.upd / perf.n,
        perf.upd_max, perf.frame / perf.n))
    end
  end
  local f = GameGetFrameNum()
  if TRACER then
    if f >= TR_FROM and f <= TR_TO then TRACER.dump(log, f) else TRACER.reset() end
  end
  if STALL and f >= 200 and f % 37 == 0 then
    -- inject wall-clock stalls (one instance only) to test for wall-clock dependencies
    C.Sleep((f * 7) % 150 + 30)
  end
  if TPROBE then
    if f == 30 or f % 300 == 0 then TPROBE.refresh() end
    if f >= 30 then TPROBE.sample() end
    if f % 600 == 0 or f == DONE_AT then TPROBE.report(log) log("TPEND " .. f) end
  end
  if PROBE > 0 then
    -- cheap per-frame probes (first diverging frame + subsystem); PROBE>=2 adds the exact entity hash
    local ne = #EntityGetInRadius(0, 0, 100000000)
    local ex = 0
    if PROBE >= 2 then ex = entity_hash_exact() end
    local gw = grid_world()
    local gp = gw and rdp(gw + 0x540) or 0
    local n1, n3, n4 = 0, 0, 0
    if gp ~= 0 then
      n1 = (rd32(gp + 0x38) - rd32(gp + 0x34)) / 0x70
      n3 = (rd32(gp + 0x50) - rd32(gp + 0x4c)) / 0x70
      n4 = (rd32(gp + 0xa0) - rd32(gp + 0x9c)) / 0x18
    end
    local pr = ffi.cast("uint32_t*", 0x01222388)
    local phs = ""
    if PROBE >= 4 and gp ~= 0 then phs = " " .. particle_hashes(gp) end
    if PDPOST[f] and gp ~= 0 then dump_particles_post(f, gp) end
    -- physics bridge P=[GridWorld+0x530]: node list at P+0x1922c (next +0xb8); step wall time (ms, double) P+0x19270
    local pn, ph, pms = 0, 0x811c9dc5, -1
    local pb = gw and rdp(gw + 0x530) or 0
    if pb ~= 0 then
      pms = ffi.cast("double*", pb + 0x19270)[0]
      local nd = rd32(pb + 0x1922c)
      while nd ~= 0 and pn < 100000 do
        local q = ffi.cast("uint32_t*", nd)
        ph = fnv(fnv(fnv(ph, q[0]), bit.band(q[1], 0xff)), q[2] ~= 0 and 1 or 0)
        pn = pn + 1
        nd = q[0x2e]
      end
    end
    local sn = PROBE >= 3 and (" snp=" .. snap_pre .. " sn=" .. snapshot_str()) or ""
    log(string.format("P %d %d g=%08x p=%08x%08x r=%08x/%d ne=%d np=%d/%d/%d ex=%08x pl=%d:%08x t_phys=%.2f%s", f, lf,
      rd32(GLOBAL_LCG), pr[1], pr[0], rd32(RAND_STATE), rd32(RAND_COUNT), ne, n1, n3, n4, ex, pn, bit.tobit(ph) % 0x100000000,
      pms, sn .. phs))
  end
  if f % 60 == 0 and f <= POST_UNTIL then
    local nc, wh, parts = world_hash(nil)
    local ne, eh = entity_hash()
    local nu, uh, us = units_state()
    log(string.format("frame=%d chunks=%d world=%08x ents=%d ent=%08x units=%d uh=%08x lf=%d %s", f, nc,
      wh % 0x100000000, ne, eh % 0x100000000, nu, uh, lf, globals_str()))
    log("  " .. table.concat(parts, " "))
    log("  U " .. us)
    log(string.format("T %d %.0f", f, tpost))
    if f == DONE_AT then log("DONE") end
  end
end
