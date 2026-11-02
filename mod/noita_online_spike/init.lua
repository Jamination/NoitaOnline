-- DETERMINISM SPIKE (temporary)
local ffi = require("ffi")
ModMagicNumbersFileAdd("mods/noita_online_spike/files/magic_spike.xml")
local root = os.getenv("NOITA_ONLINE_ROOT") or "."
local inst = os.getenv("NOITA_ONLINE_INSTANCE") or "x"
local logf = io.open(root .. "/run/logs/spike_" .. inst .. ".txt", "w")
local function log(s) logf:write(s, "\n") logf:flush() end
log("init")

local GG_ADDR = 0x0122374c
local function gg() return ffi.cast("char**", GG_ADDR)[0] end

local bit = require("bit")
local function fnv(h, v)
  h = bit.bxor(h, v)
  return bit.tobit(h * 16777619)
end

local spawned = false
local function P(p) return tonumber(ffi.cast("uint32_t", p)) end
local function world_hash(dump)
  local game_global = gg()
  local world_data = ffi.cast("char**", game_global + 0xc)[0]
  if P(world_data) == 0 then return 0, 0, {} end
  local grid_world = ffi.cast("char**", world_data + 0x44)[0]
  local chunk_map = grid_world + 4 + 318*4 + 4
  local table_ = ffi.cast("uint32_t**", chunk_map + 8)[0]
  local cf = ffi.cast("char**", game_global + 0x18)[0]
  local mat_begin = ffi.cast("uint32_t*", cf + 0x18)[0]
  local total = 0
  local nchunks = 0
  local parts = {}
  for idx = 0, 0x40000 - 1 do
    local chunk = table_[idx]
    if chunk ~= 0 then
      nchunks = nchunks + 1
      local cp = ffi.cast("uint32_t*", chunk)
      local cells = ffi.cast("uint32_t*", cp[0])
      local rng = cp[1]
      local h = 0x811c9dc5
      local n = 0
      local df = nil
      if dump and idx == 131328 then df = io.open(root .. "/run/logs/dump_" .. inst .. "_" .. dump .. ".txt", "w") end
      for i = 0, 0x40000 - 1 do
        local c = cells[i]
        if c ~= 0 then
          local mp = ffi.cast("uint32_t*", c)[5]
          local mat = (mp - mat_begin) / 0x290
          h = fnv(h, i)
          h = fnv(h, mat)
          n = n + 1
          if df then df:write(i % 512, " ", math.floor(i / 512), " ", mat, "\n") end
        end
      end
      if df then df:close() end
      parts[#parts+1] = string.format("%d:%08x:%d:%08x", idx, bit.tobit(h) % 0x100000000, n, rng)
      total = fnv(total, h)
    end
  end
  return nchunks, total, parts
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

function OnPlayerSpawned(player)
  local controls = EntityGetFirstComponentIncludingDisabled(player, "ControlsComponent")
  if controls then ComponentSetValue2(controls, "enabled", false) end
  spawned = true
  log("spawned frame=" .. GameGetFrameNum() .. " player=" .. player)
end

local function ent_list()
  local ents = EntityGetInRadius(0, 0, 100000000)
  local out = {}
  for _, e in ipairs(ents) do
    local x, y = EntityGetTransform(e)
    out[#out+1] = string.format("%d:%s:%.2f,%.2f", e, EntityGetFilename(e), x, y)
  end
  return out
end

ffi.cdef("void __stdcall Sleep(uint32_t ms); void srand(unsigned int seed);")
local crt = ffi.load("msvcr120.dll")
crt.srand(12345)
local lf = 0
function OnWorldPreUpdate()
  lf = lf + 1
  local f = lf
  if f == 1 then crt.srand(12345) end
  if f <= 300 then ffi.C.Sleep(150) end
  if f <= 120 then
    local nc, wh, parts = world_hash((f == 2 or f == 3) and tostring(f) or nil)
    local ne, eh = entity_hash()
    log(string.format("pre frame=%d chunks=%d world=%08x ents=%d ent=%08x gf=%d", f, nc, wh % 0x100000000, ne, eh % 0x100000000, GameGetFrameNum()))
    log("  " .. table.concat(parts, " "))
    log("  E " .. table.concat(ent_list(), " "))
  end
end

function OnWorldPostUpdate()
  local f = GameGetFrameNum()
  if f % 60 == 0 then
    local t0 = os.clock()
    local nc, wh, parts = world_hash()
    local ne, eh = entity_hash()
    log(string.format("frame=%d chunks=%d world=%08x ents=%d ent=%08x dt=%.1fms", f, nc, wh % 0x100000000, ne, eh % 0x100000000, (os.clock()-t0)*1000))
    log("  " .. table.concat(parts, " "))
  end
end
