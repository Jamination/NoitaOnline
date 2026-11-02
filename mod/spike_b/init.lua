-- MULTI-FOCUS SPIKE (research harness, temporary)
-- Modes (env SPIKE_B_MODE):
--   focus      : instance 20 adds extra streaming foci, instance 21 is the control (none).
--   loadhook   : both instances add foci + force chunk-load integration to wait for all
--                pending loads (deterministic integration frame); per-frame chunk-load log;
--                automatic streaming unload disabled (STREAMING_ENABLED=0).
--   noloadhook : like loadhook but without the integration hook (baseline).
--   streamcmp  : like noloadhook but with vanilla streaming unload enabled.
--   cost       : 400-frame phases with N native foci; inst 20 with load hook, 21 without.
--   costcap    : cost schedule {0,9,12,9,0}; inst 20 GRID_MAX_UPDATES_PER_FRAME=100000, 21 default.
local ffi = require("ffi")
local bit = require("bit")
local MODE = os.getenv("SPIKE_B_MODE") or "focus"
local root = os.getenv("NOITA_ONLINE_ROOT") or "."
local inst = tonumber(os.getenv("NOITA_ONLINE_INSTANCE") or "0") or 0
ModMagicNumbersFileAdd("mods/spike_b/files/magic_spike.xml")
if MODE == "loadhook" or MODE == "noloadhook" then ModMagicNumbersFileAdd("mods/spike_b/files/magic_nounload.xml") end
if MODE == "costcap" and inst % 2 == 0 then ModMagicNumbersFileAdd("mods/spike_b/files/magic_uncap.xml") end
local COSTLIKE = (MODE == "cost" or MODE == "costcap")
MFDET = (MODE == "mfdet_same" or MODE == "mfdet_diff" or MODE == "mfdet_one" or MODE == "mfdet_canon" or
  MODE == "mfsplit")
if MFDET then
  ModMagicNumbersFileAdd("mods/spike_b/files/magic_nounload.xml")
  ModMagicNumbersFileAdd("mods/spike_b/files/magic_uncap.xml")
end

local logf = io.open(root .. "/run/re_multifocus/log_" .. MODE .. "_" .. inst .. ".txt", "w")
local function log(s) logf:write(s, "\n") logf:flush() end
log("init mode=" .. MODE .. " inst=" .. inst)

ffi.cdef [[
typedef struct { int x0, y0, x1, y1; } mf_rect;
void __stdcall Sleep(uint32_t ms);
int __stdcall VirtualProtect(void* addr, size_t size, uint32_t newp, uint32_t* oldp);
void* __stdcall VirtualAlloc(void* addr, size_t size, uint32_t type, uint32_t prot);
int __stdcall FlushInstructionCache(void* proc, const void* addr, size_t size);
void* __stdcall GetCurrentProcess(void);
int __stdcall QueryPerformanceCounter(int64_t* c);
int __stdcall QueryPerformanceFrequency(int64_t* f);
]]

local C = ffi.C
local qpf = ffi.new("int64_t[1]"); C.QueryPerformanceFrequency(qpf)
local qfreq = tonumber(qpf[0])
local qbuf = ffi.new("int64_t[1]")
local function now_ms() C.QueryPerformanceCounter(qbuf) return tonumber(qbuf[0]) * 1000 / qfreq end

local function U32(a) return ffi.cast("uint32_t*", a)[0] end
local function I32(a) return ffi.cast("int32_t*", a)[0] end
local function U8(a) return ffi.cast("uint8_t*", a)[0] end
local function F32(a) return ffi.cast("float*", a)[0] end

local GG_ADDR = 0x0122374c
local function gg() return U32(GG_ADDR) end
local function camobj() return U32(gg() + 0xc) end           -- camera rect float[4] @0, GridWorld* @0x44
local function gridworld() return U32(camobj() + 0x44) end
local function chunkmap() return gridworld() + 0x500 end
local function chunk_table() return U32(chunkmap() + 8) end

local function chunk_index(x, y)
  return bit.band(bit.arshift(y, 9) + 256, 511) * 512 + bit.band(bit.arshift(x, 9) + 256, 511)
end
local function chunk_ptr(x, y) return U32(chunk_table() + 4 * chunk_index(x, y)) end
local function cell_at(x, y)
  local ch = chunk_ptr(x, y)
  if ch == 0 then return nil end
  local cells = U32(ch)
  return U32(cells + 4 * bit.bor(bit.lshift(bit.band(y, 511), 9), bit.band(x, 511)))
end
local function mat_begin() return U32(U32(gg() + 0x18) + 0x18) end

local function fnv(h, v) return bit.tobit(bit.bxor(h, v) * 16777619) end

-- window probe: hash of (cell index, material id) + non-air count + count of one material
local function probe(cx, cy, half, water_mp)
  if chunk_ptr(cx, cy) == 0 then return "unloaded" end
  local mb = mat_begin()
  local h, n, w = 0x811c9dc5, 0, 0
  local x0, y0 = cx - half, cy - half
  for y = y0, y0 + 2 * half - 1 do
    local ch = chunk_ptr(x0, y)
    if ch ~= 0 then
      local cells = U32(ch)
      local row = bit.lshift(bit.band(y, 511), 9)
      for x = x0, x0 + 2 * half - 1 do
        local c
        if bit.arshift(x, 9) == bit.arshift(x0, 9) then
          c = U32(cells + 4 * bit.bor(row, bit.band(x, 511)))
        else
          c = cell_at(x, y) or 0
        end
        if c ~= 0 then
          local mp = U32(c + 0x14)
          h = fnv(h, (y - y0) * 4096 + (x - x0))
          h = fnv(h, mp - mb)
          n = n + 1
          if mp == water_mp then w = w + 1 end
        end
      end
    end
  end
  return string.format("%08x/%d/%d", h % 0x100000000, n, w)
end

-- find a drop point near (cx,cy): 24x24 air box whose centre column hits a non-air cell within
-- 16..64 px below; returns drop x, y and the landing y. Scans nearest-first (rings).
local function find_drop(cx, cy, half)
  for r = 0, half, 8 do
    for dy = -r, r, 8 do
      for dx = -r, r, 8 do
        if math.max(math.abs(dx), math.abs(dy)) == r then
          local x, y = cx + dx, cy + dy
          local ok = true
          for yy = -12, 11, 4 do
            for xx = -12, 11, 4 do
              if cell_at(x + xx, y + yy) ~= 0 then ok = false break end
            end
            if not ok then break end
          end
          if ok then
            for k = 16, 64 do
              local c = cell_at(x, y + k)
              if c == nil then break end
              if c ~= 0 then return x, y, y + k end
            end
          end
        end
      end
    end
  end
  return cx, cy, cy
end

-- whole-chunk probe: hash/non-air count/water count of the 512x512 chunk containing (x,y)
local function probe_chunk(x, y, water_mp)
  local ch = chunk_ptr(x, y)
  if ch == 0 then return "unloaded" end
  local cells = ffi.cast("uint32_t*", U32(ch))
  local mb = mat_begin()
  local h, n, w = 0x811c9dc5, 0, 0
  for i = 0, 0x40000 - 1 do
    local c = cells[i]
    if c ~= 0 then
      local mp = ffi.cast("uint32_t*", c)[5]
      h = fnv(fnv(h, i), mp - mb)
      n = n + 1
      if mp == water_mp then w = w + 1 end
    end
  end
  return string.format("%08x/%d/%d", h % 0x100000000, n, w)
end

-- loaded chunk set
local function loaded_chunks()
  local t = ffi.cast("uint32_t*", chunk_table())
  local out = {}
  for i = 0, 0x40000 - 1 do
    if t[i] ~= 0 then out[#out + 1] = i end
  end
  return out
end

-- grid sim: active (updated) tiles from GridWorldThreadImpl (GridWorld+0x5d0)
local function active_tiles()
  local impl = U32(gridworld() + 0x5d0)
  if impl == 0 then return 0, {} end
  local n = I32(impl)
  local arr = U32(impl + 4)
  local out = {}
  for i = 0, n - 1 do
    local t = U32(arr + 4 * i)
    out[#out + 1] = { I32(t + 0x540), I32(t + 0x544), I32(t + 0x470), I32(t + 0x474) }
  end
  return n, out
end

-- pending async chunk loads: std::map head @0x01208008, size @0x0120800c, node+0x24 = job, job+0xc8 = done flag
local function pending_jobs(fn)
  local head = U32(0x01208008)
  local node = U32(head)  -- leftmost
  local k = 0
  while node ~= head and k < 100000 do
    k = k + 1
    fn(node, U32(node + 0x24), I32(node + 0x18))
    local r = U32(node + 8)
    if U8(r + 0xd) == 0 then
      node = r
      while U8(U32(node) + 0xd) == 0 do node = U32(node) end
    else
      local p = U32(node + 4)
      while U8(p + 0xd) == 0 and node == U32(p + 8) do node = p; p = U32(p + 4) end
      node = p
    end
  end
end

-- ---------------------------------------------------------------------------------------
-- Load-integration hook: GridWorld::IntegrateFinishedChunkLoads (FUN_0073d6a0) is patched to
-- first block until every pending chunk-load job has finished (job+0xc8 == 1).
local hook_stats = { calls = 0, waited_ms = 0, max_ms = 0, jobs = 0, timeouts = 0 }
local function wait_all_pending()
  local t0 = now_ms()
  local njobs = 0
  pending_jobs(function(node, job, idx)
    if job ~= 0 then
      njobs = njobs + 1
      local spins = 0
      while I32(job + 0xc8) ~= 1 do
        C.Sleep(0)
        spins = spins + 1
        if spins > 200000 then hook_stats.timeouts = hook_stats.timeouts + 1 break end
      end
    end
  end)
  local dt = now_ms() - t0
  hook_stats.calls = hook_stats.calls + 1
  hook_stats.jobs = hook_stats.jobs + njobs
  hook_stats.waited_ms = hook_stats.waited_ms + dt
  if dt > hook_stats.max_ms then hook_stats.max_ms = dt end
end
local hook_cb -- keep alive
local function install_load_hook()
  local target = 0x0073d6a0
  local orig = ffi.cast("uint8_t*", target)
  local expect = { 0x55, 0x8b, 0xec, 0x6a, 0xff }
  for i = 1, 5 do
    if orig[i - 1] ~= expect[i] then log("hook: unexpected prologue byte " .. i) return false end
  end
  hook_cb = ffi.cast("void(*)(void)", function()
    local ok, err = pcall(wait_all_pending)
    if not ok then log("hook error: " .. tostring(err)) end
  end)
  local cbaddr = tonumber(ffi.cast("uint32_t", hook_cb))
  local stub = ffi.cast("uint8_t*", C.VirtualAlloc(nil, 4096, 0x3000, 0x40))
  local s = tonumber(ffi.cast("uint32_t", stub))
  local code = { 0x60, 0x9c, 0xb8 }
  local function put32(t, v) for i = 0, 3 do t[#t + 1] = bit.band(bit.rshift(v, 8 * i), 0xff) end end
  put32(code, cbaddr)
  for _, b in ipairs({ 0xff, 0xd0, 0x9d, 0x61, 0x55, 0x8b, 0xec, 0x6a, 0xff, 0xe9 }) do code[#code + 1] = b end
  local jmp_from = s + #code + 4
  put32(code, bit.tobit(target + 5 - jmp_from))
  for i, b in ipairs(code) do stub[i - 1] = b end
  local old = ffi.new("uint32_t[1]")
  C.VirtualProtect(ffi.cast("void*", target), 5, 0x40, old)
  local rel = bit.tobit(s - (target + 5))
  orig[0] = 0xe9
  for i = 0, 3 do orig[1 + i] = bit.band(bit.rshift(rel, 8 * i), 0xff) end
  C.VirtualProtect(ffi.cast("void*", target), 5, old[0], old)
  C.FlushInstructionCache(C.GetCurrentProcess(), ffi.cast("void*", target), 5)
  log(string.format("hook installed stub=%08x cb=%08x", s, cbaddr))
  return true
end

-- Native keep-alive: FUN_0073e080(__fastcall, ECX = int rect[4] {x0,y0,x1,y1}) — same call the
-- StreamingKeepAliveSystem makes: RequestLoad(rect) + keep chunks within rect±256 for 10 frames.
local keep_alive_rect = ffi.cast("void(__fastcall*)(mf_rect*)", 0x0073e080)
local rbuf = ffi.new("mf_rect")
local function native_focus(x, y, hw, hh)
  rbuf.x0, rbuf.y0, rbuf.x1, rbuf.y1 = x - hw, y - hh, x + hw, y + hh
  keep_alive_rect(rbuf)
end

-- ---------------------------------------------------------------------------------------
-- mfdet: lockstep-style determinism with N foci. Both instances: deterministic rand (IAT stub),
-- fixed seeds, chunk-load barrier, STREAMING_ENABLED=0, GRID_MAX_UPDATES_PER_FRAME uncapped,
-- camera-driven RequestLoad calls removed (0x00720210/0x00720290), identical foci incl. player.
-- mfdet_same: both cameras follow the player.  mfdet_diff: inst 20 camera at F2, inst 21 at F3.
local function patch_bytes(addr, bytes)
  local old = ffi.new("uint32_t[1]")
  C.VirtualProtect(ffi.cast("void*", addr), #bytes, 0x40, old)
  local p = ffi.cast("uint8_t*", addr)
  for i, b in ipairs(bytes) do p[i - 1] = b end
  C.VirtualProtect(ffi.cast("void*", addr), #bytes, old[0], old)
  C.FlushInstructionCache(C.GetCurrentProcess(), ffi.cast("void*", addr), #bytes)
end
local function le32(v)
  v = v % 0x100000000
  return { v % 256, math.floor(v / 256) % 256, math.floor(v / 65536) % 256, math.floor(v / 16777216) % 256 }
end
local det_exec, RAND_STATE
if MFDET then
  det_exec = ffi.cast("uint8_t*", C.VirtualAlloc(nil, 4096, 0x3000, 0x40))
  local E = tonumber(ffi.cast("uint32_t", det_exec))
  RAND_STATE = E + 0x800
  local RAND_COUNT, SRAND_LAST = E + 0x804, E + 0x808
  ffi.cast("uint32_t*", RAND_STATE)[0] = 0x1234567
  local code = { 0xFF, 0x05 }
  for _, b in ipairs(le32(RAND_COUNT)) do code[#code + 1] = b end
  code[#code + 1] = 0xA1
  for _, b in ipairs(le32(RAND_STATE)) do code[#code + 1] = b end
  for _, b in ipairs({ 0x69, 0xC0, 0xFD, 0x43, 0x03, 0x00, 0x05, 0xC3, 0x9E, 0x26, 0x00, 0xA3 }) do code[#code + 1] = b end
  for _, b in ipairs(le32(RAND_STATE)) do code[#code + 1] = b end
  for _, b in ipairs({ 0xC1, 0xE8, 0x10, 0x25, 0xFF, 0x7F, 0x00, 0x00, 0xC3 }) do code[#code + 1] = b end
  for i, b in ipairs(code) do det_exec[i - 1] = b end
  local sr = { 0x8B, 0x44, 0x24, 0x04, 0xA3 }
  for _, b in ipairs(le32(SRAND_LAST)) do sr[#sr + 1] = b end
  sr[#sr + 1] = 0xC3
  for i, b in ipairs(sr) do det_exec[0x40 + i - 1] = b end
  patch_bytes(0x00f07450, le32(E))         -- IAT msvcr120!rand
  patch_bytes(0x00f073d4, le32(E + 0x40))  -- IAT msvcr120!srand
  log(string.format("mfdet: rand/srand IAT -> %08x", E))
  if os.getenv("SPIKE_B_INLINE") == "1" then
    -- (same as ReGridDeterminism) run every thread-pool job inline on the submitting thread:
    -- FUN_0084c240 group submit -> always its inline path; FUN_0084c1a0 async submit -> run slot
    -- runner FUN_0084c430(slot) inline and return the completed slot.
    if U8(0x0084c255) == 0x74 and U8(0x0084c1e0) == 0x8D and U8(0x0084c1e3) == 0x66 then
      patch_bytes(0x0084c255, { 0x90, 0x90 })
      local site = 0x0084c1e0
      local code = { 0x66, 0xC7, 0x43, 0x24, 0x00, 0x00, 0x89, 0x7B, 0x20, 0x53, 0xE8 }
      for _, b in ipairs(le32(0x0084c430 - (site + #code + 4))) do code[#code + 1] = b end
      for _, b in ipairs({ 0x83, 0xC4, 0x04, 0x8B, 0xC3, 0x5F, 0x5E, 0x5B, 0x8B, 0xE5, 0x5D, 0xC2, 0x04, 0x00 }) do
        code[#code + 1] = b
      end
      patch_bytes(site, code)
      log("mfdet: thread-pool jobs forced inline")
    else
      log("mfdet: inline patch bytes unexpected, skipped")
    end
  end
  -- (ReGridDeterminism) particle ctor FUN_00712010 leaves bits of +0x6a/+0x6c uninitialized:
  -- AND byte [esi+0x6a],0x02 -> 0x00 (imm8 @0x0071213e); AND byte [esi+0x6c],0xFC -> 0x00 (imm8 @0x00712144)
  if U8(0x0071213e) == 0x02 and U8(0x00712144) == 0xFC then
    patch_bytes(0x0071213e, { 0x00 })
    patch_bytes(0x00712144, { 0x00 })
    log("mfdet: particle ctor init patch applied")
  else
    log(string.format("mfdet: particle ctor bytes unexpected %02x %02x", U8(0x0071213e), U8(0x00712144)))
  end
  -- P14: cosmetic particles (+0x6a bit1, set from the create flag in FUN_007168c0 @0x00716a0f and FUN_00716af0
  -- @0x00716c51: ADD AL,AL; XOR AL,[ESI+0x6a]; AND AL,2; XOR [ESI+0x6a],AL) collide against the render-camera
  -- material snapshot (GridParticles+0x24, built from the render camera, indexed with GridWorld+0x498) and turn into
  -- real cells when they land -> camera-dependent world. AND AL,2 -> AND AL,0 keeps bit1 = 0 (P4 clears it in the
  -- ctor), so they collide against the real grid like normal particles.
  if os.getenv("SPIKE_B_NOP14") ~= "1" then
    if U8(0x00716a0f) == 0x24 and U8(0x00716a10) == 0x02 and U8(0x00716c51) == 0x24 and U8(0x00716c52) == 0x02 then
      patch_bytes(0x00716a10, { 0x00 })
      patch_bytes(0x00716c52, { 0x00 })
      log("mfdet: P14 cosmetic-particle snapshot collision disabled")
    else
      log("mfdet: P14 bytes unexpected, skipped")
    end
  end
  if os.getenv("SPIKE_B_FIXDT") == "1" then
    -- DeathMatch::Update (FUN_006b26f0, thiscall(this, float dt)): force dt = 1/60 at entry.
    -- prologue 55 8B EC 83 E4 F0 -> JMP stub; stub: mov dword [esp+4],0x3C888889; prologue; jmp 006b26f6
    if U8(0x006b26f0) == 0x55 and U8(0x006b26f3) == 0x83 and U8(0x006b26f5) == 0xF0 then
      local s = 0x300
      local sa = tonumber(ffi.cast("uint32_t", det_exec)) + s
      local code = { 0xC7, 0x44, 0x24, 0x04, 0x89, 0x88, 0x88, 0x3C, 0x55, 0x8B, 0xEC, 0x83, 0xE4, 0xF0, 0xE9 }
      for _, b in ipairs(le32(0x006b26f6 - (sa + #code + 4))) do code[#code + 1] = b end
      for k, b in ipairs(code) do det_exec[s + k - 1] = b end
      local rel = le32(sa - (0x006b26f0 + 5))
      patch_bytes(0x006b26f0, { 0xE9, rel[1], rel[2], rel[3], rel[4], 0x90 })
      log("mfdet: DeathMatch::Update dt fixed to 1/60")
    else
      log("mfdet: Update prologue unexpected, dt patch skipped")
    end
  end
end
local function fix_seeds(where)
  if not MFDET then return end
  ffi.cast("uint32_t*", 0x01221bc0 + 0xa0)[0] = 0x0012d687
  ffi.cast("uint32_t*", 0x01221bc0 + 0xa4)[0] = 0x075bcd15
  ffi.cast("uint32_t*", 0x0120741c)[0] = bit.bxor(0x075bcd15, 0xc)
  ffi.cast("double*", 0x01207420)[0] = 123456789.0
  ffi.cast("double*", 0x01222388)[0] = 123456789.0  -- GridParticles RNG (seeded from platform in FUN_00712bc0)
  ffi.cast("uint32_t*", RAND_STATE)[0] = 0x1234567
  log("mfdet: seeds fixed at " .. where)
end
function OnMagicNumbersAndWorldSeedInitialized() fix_seeds("OnMagicNumbersAndWorldSeedInitialized") end
function OnWorldInitialized() fix_seeds("OnWorldInitialized") end

function mfdet_spawn_patches()
  -- GridWorld::OnCameraRectChanged (FUN_00720130): drop both RequestLoad calls (thiscall, RET 8)
  for _, site in ipairs({ 0x00720210, 0x00720290 }) do
    if U8(site) ~= 0xE8 then log(string.format("mfdet: unexpected byte at %08x", site)) return end
    patch_bytes(site, { 0x83, 0xC4, 0x08, 0x90, 0x90 })
  end
  log("mfdet: camera RequestLoad calls removed")
end

local function chunk_hash_all()
  local t = ffi.cast("uint32_t*", chunk_table())
  local mb = mat_begin()
  local parts = {}
  for idx = 0, 0x40000 - 1 do
    local ch = t[idx]
    if ch ~= 0 then
      local cells = ffi.cast("uint32_t*", ffi.cast("uint32_t*", ch)[0])
      local h, n = 0x811c9dc5, 0
      for i = 0, 0x40000 - 1 do
        local c = cells[i]
        if c ~= 0 then
          h = fnv(fnv(h, i), ffi.cast("uint32_t*", c)[5] - mb)
          n = n + 1
        end
      end
      parts[#parts + 1] = string.format("%d:%08x:%d", idx, h % 0x100000000, n)
    end
  end
  return parts
end

local function region_entity_hash(x, y, r)
  local ents = EntityGetInRadius(x, y, r)
  table.sort(ents)
  local h = 0x811c9dc5
  for _, e in ipairs(ents) do
    local ex, ey = EntityGetTransform(e)
    h = fnv(fnv(fnv(h, e), math.floor(ex * 16)), math.floor(ey * 16))
  end
  return string.format("%d:%08x", #ents, h % 0x100000000)
end

-- ---------------------------------------------------------------------------------------
local player, P = nil, nil
local foci_on = (MODE == "focus" and inst % 2 == 0) or MODE == "loadhook" or MODE == "noloadhook" or MODE == "streamcmp"
local F = {}          -- focus points
local focus_ents = {}
local lf = 0
local t_pre, t_prev_pre = nil, nil
local stats = {}      -- phase -> list of {upd, frame}
local prev_loaded = {}
local water_mp = 0
local spawn_sites = {}

function OnPlayerSpawned(p)
  player = p
  local controls = EntityGetFirstComponentIncludingDisabled(p, "ControlsComponent")
  if controls then ComponentSetValue2(controls, "enabled", false) end
  local x, y = EntityGetTransform(p)
  P = { math.floor(x), math.floor(y) }
  F = {
    { P[1] + 300, P[2] + 1800 },
    { P[1] - 1500, P[2] + 1200 },
    { P[1] + 1800, P[2] + 600 },
    { P[1] + 200, P[2] + 3200 },
  }
  log(string.format("spawned frame=%d player=%d P=%d,%d foci_on=%s gw=%08x gg10=%08x gw494=%d", GameGetFrameNum(), p, P[1],
    P[2], tostring(foci_on), gridworld(), U32(gg() + 0x10), U8(gridworld() + 0x494)))
  if MODE == "loadhook" or (MODE == "cost" and inst % 2 == 0) or MFDET then install_load_hook() end
  if MFDET then mfdet_spawn_patches() end
end

-- cost mode: 400-frame phases with N native foci (rect ±300x±200 each), N = schedule[k]
local COST_SCHED = (MODE == "costcap") and { 0, 9, 12, 9, 0 } or { 0, 3, 6, 9, 6, 3, 0, 9, 0 }
local COST_LEN = 400
local COST_POS = {}
for _, dy in ipairs({ 600, 1500, 2600 }) do
  for _, dx in ipairs({ -3000, -1500, 1500, 3000 }) do COST_POS[#COST_POS + 1] = { dx, dy } end
end
local function cost_phase(f)
  local k = math.floor((f - 1) / COST_LEN) + 1
  return k, COST_SCHED[k]
end

local function phase_of(f)
  if COSTLIKE then
    local k, n = cost_phase(f)
    if not n then return "end" end
    if (f - 1) % COST_LEN < 100 then return string.format("k%d_N%d_warm", k, n) end
    return string.format("k%d_N%d", k, n)
  end
  if f <= 600 then return "A_base" elseif f <= 1200 then return "B_1focus" elseif f <= 1800 then return "C_4foci"
  elseif f <= 2400 then return "D_camfar" else return "E_end" end
end

local function add_stat(ph, upd, frm)
  stats[ph] = stats[ph] or {}
  local t = stats[ph]
  t[#t + 1] = { upd, frm }
end

local function summarize(ph)
  local t = stats[ph]
  if not t or #t == 0 then return ph .. ": none" end
  local u, fr = {}, {}
  for i, v in ipairs(t) do u[i] = v[1]; fr[i] = v[2] end
  table.sort(u); table.sort(fr)
  local function mean(a) local s = 0 for _, v in ipairs(a) do s = s + v end return s / #a end
  local function pct(a, p) return a[math.max(1, math.floor(#a * p))] end
  return string.format("%s: n=%d upd mean=%.2f p50=%.2f p95=%.2f max=%.2f | frame mean=%.2f p50=%.2f p95=%.2f",
    ph, #u, mean(u), pct(u, 0.5), pct(u, 0.95), u[#u], mean(fr), pct(fr, 0.5), pct(fr, 0.95))
end

local measuring = false -- set on frames that run expensive probes (excluded from stats)
-- ---------------------------------------------------------------------------------------
-- mfsplit: render cameras differ per client; the simulation window (OnWorldPreUpdate ..
-- OnWorldPostUpdate) gets (a) its own copy of every engine RNG state and (b) an identical sim
-- camera. Foci: player + 2 far foci in fixed order via FUN_0073e080; focus B moves at f=600;
-- own deterministic unloader (STREAMING_ENABLED=0) via thunk to FUN_00740e70.
local RNG_U32 = { 0x0120741c, 0x01221c60, 0x01221c64 }
local rng_sim, rng_render = nil, nil
local function rng_save()
  local t = {}
  for i, a in ipairs(RNG_U32) do t[i] = U32(a) end
  t.pm = ffi.cast("double*", 0x01207420)[0]
  t.pp = ffi.cast("double*", 0x01222388)[0]
  t.rs = U32(RAND_STATE)
  return t
end
local function rng_load(t)
  for i, a in ipairs(RNG_U32) do ffi.cast("uint32_t*", a)[0] = t[i] end
  ffi.cast("double*", 0x01207420)[0] = t.pm
  ffi.cast("double*", 0x01222388)[0] = t.pp
  ffi.cast("uint32_t*", RAND_STATE)[0] = t.rs
end
local SPLIT_RNG = os.getenv("SPIKE_B_NORNG") ~= "1"
local SPLIT_CAM = os.getenv("SPIKE_B_NOSIMCAM") ~= "1"
local unload_thunk
local function make_unload_thunk()
  local code = { 0x8B, 0x4C, 0x24, 0x04, 0x8B, 0x54, 0x24, 0x08, 0x6A, 0x01, 0xB8 }
  for _, b in ipairs(le32(0x00740e70)) do code[#code + 1] = b end
  for _, b in ipairs({ 0xFF, 0xD0, 0x83, 0xC4, 0x04, 0xC3 }) do code[#code + 1] = b end
  for i, b in ipairs(code) do det_exec[0x100 + i - 1] = b end
  unload_thunk = ffi.cast("void(__cdecl*)(uint32_t, int32_t*)", det_exec + 0x100)
end
local function split_foci(f)
  local A = { P[1] - 1500, P[2] + 1200 }
  local B = (f < 600) and { P[1] + 1800, P[2] + 600 } or { P[1] + 200, P[2] + 3200 }
  return { { P[1], P[2], 400, 300 }, { A[1], A[2], 300, 200 }, { B[1], B[2], 300, 200 } }
end
local unload_log = {}
local coordbuf = ffi.new("int32_t[3]")
local function own_unloader(f)
  if f % 30 ~= 0 then return end
  local foci = split_foci(f)
  local t = ffi.cast("uint32_t*", chunk_table())
  local victims = {}
  for idx = 0, 0x40000 - 1 do
    if t[idx] ~= 0 then
      local cx, cy = (idx % 512) - 256, math.floor(idx / 512) - 256
      local x0, y0 = cx * 512, cy * 512
      local keep = false
      for _, q in ipairs(foci) do
        if x0 < q[1] + q[3] + 512 and x0 + 512 > q[1] - q[3] - 512 and y0 < q[2] + q[4] + 512 and
            y0 + 512 > q[2] - q[4] - 512 then keep = true break end
      end
      if not keep then victims[#victims + 1] = { cx, cy, idx } end
    end
  end
  for k = 1, math.min(#victims, 2) do  -- at most 2 per pass, ascending index order
    local v = victims[k]
    coordbuf[0], coordbuf[1], coordbuf[2] = v[1], v[2], v[3]
    unload_thunk(chunkmap(), coordbuf)
    unload_log[#unload_log + 1] = string.format("%d@%d", v[3], f)
  end
  if #victims > 0 then log(string.format("  unload f=%d victims=%d done=%s", f, #victims, table.concat(unload_log, ","))) end
end
-- RNG probes: redirect direct CALLs inside the sim window through a stub that reports glcg
-- before the callee runs (site order = execution order within DeathMatch::Update FUN_006b26f0).
local PROBE_SITES = { 0x006b3267, 0x006b3279, 0x006b3284, 0x006b3295, 0x006b32a1, 0x006b32d8,
  0x006b32e4, 0x006b32e9 }
if os.getenv("SPIKE_B_PROBE") == "2" then
  -- inside the world update: FUN_006363b0, world update FUN_006f1d40 entry, FUN_00657680, grid
  -- FUN_0071a6b0, WorldLightAndFog job submit (after grid, before the vtbl+0x60(dt) updates), after.
  PROBE_SITES = { 0x006b32a1, 0x006b32ae, 0x006f1d74, 0x006f1eed, 0x006f1f13, 0x006b32d8 }
elseif os.getenv("SPIKE_B_PROBE") == "3" then
  -- inside the grid update FUN_0071a6b0 (in call order)
  PROBE_SITES = { 0x0071a741, 0x0071a748, 0x0071a74f, 0x0071a75a, 0x0071a769, 0x0071a779, 0x0071a780, 0x0071a787,
    0x0071a7bf, 0x0071a7f2, 0x0071a825 }
end
local probe_vals = {}
local probe_cb
local function install_probes()
  probe_cb = ffi.cast("void(__cdecl*)(int)", function(i)
    probe_vals[i] = string.format("%08x/%08x", U32(0x0120741c), U32(RAND_STATE))
    if os.getenv("SPIKE_B_PROBE") == "3" then
      local gw = gridworld()
      probe_vals[i] = probe_vals[i] .. string.format("/n%d", (U32(gw + 0x5a8) - U32(gw + 0x5a4)) / 4)
      if i == 5 then
        local g = ffi.cast("float*", gg() + 0x180)
        probe_vals.q7 = nil
        probe_vals.gb5 = string.format("%g,%g,%g,%g/%d,%d", g[0], g[1], g[2], g[3], I32(gw + 0x498), I32(gw + 0x49c))
      end
      if i == 7 then
        probe_vals.q7 = string.format("new=%d pend=%d bud=%d", (U32(gw + 0x5a8) - U32(gw + 0x5a4)) / 4,
          (U32(gw + 0x5b4) - U32(gw + 0x5b0)) / 4, I32(gw + 0x5bc))
      end
    end
  end)
  local cb = tonumber(ffi.cast("uint32_t", probe_cb))
  local xs = tonumber(ffi.cast("uint32_t", det_exec)) + 0xC00 -- XMM0..3 save area
  for i, site in ipairs(PROBE_SITES) do
    if U8(site) ~= 0xE8 then log(string.format("probe: site %08x not a CALL", site)) return end
    local target = site + 5 + ffi.cast("int32_t*", site + 1)[0]
    local s = 0x400 + (i - 1) * 80
    local sa = tonumber(ffi.cast("uint32_t", det_exec)) + s
    local code = { 0x60, 0x9C }
    for r = 0, 3 do  -- movups [xs+16r], xmm_r  (XMM regs carry float args in LTCG code)
      code[#code + 1] = 0x0F; code[#code + 1] = 0x11; code[#code + 1] = 0x05 + 8 * r
      for _, b in ipairs(le32(xs + 16 * r)) do code[#code + 1] = b end
    end
    code[#code + 1] = 0x6A; code[#code + 1] = i; code[#code + 1] = 0xB8
    for _, b in ipairs(le32(cb)) do code[#code + 1] = b end
    for _, b in ipairs({ 0xFF, 0xD0, 0x83, 0xC4, 0x04 }) do code[#code + 1] = b end
    for r = 0, 3 do  -- movups xmm_r, [xs+16r]
      code[#code + 1] = 0x0F; code[#code + 1] = 0x10; code[#code + 1] = 0x05 + 8 * r
      for _, b in ipairs(le32(xs + 16 * r)) do code[#code + 1] = b end
    end
    for _, b in ipairs({ 0x9D, 0x61, 0xE9 }) do code[#code + 1] = b end
    for _, b in ipairs(le32(target - (sa + #code + 4))) do code[#code + 1] = b end
    for k, b in ipairs(code) do det_exec[s + k - 1] = b end
    local rel = le32(sa - (site + 5))
    patch_bytes(site, { 0xE8, rel[1], rel[2], rel[3], rel[4] })
  end
  log("probe: installed " .. #PROBE_SITES .. " sites")
end
-- Queue trace (SPIKE_B_QTRACE=1): hook vector<Cell*>::push_back FUN_0089d7a0 and record every push into
-- GridWorld+0x5a4 (new items) / +0x5b0 (pending) with phase (1 = inside Pre..Post, 2 = outside), frame,
-- return address, caller chain (EBP walk), cell, material, cell x/y. Source: run/re_camleak/qtrace.s
local QT = nil
-- hex template -> byte list, replacing placeholder dwords 0xAAAA00nn by subst[nn]
function QT_assemble(hex, subst)
  local code = {}
  for i = 1, #hex, 2 do code[#code + 1] = tonumber(hex:sub(i, i + 1), 16) end
  local i = 1
  while i <= #code - 3 do
    if code[i + 2] == 0xaa and code[i + 3] == 0xaa and code[i + 1] == 0 and subst[code[i]] then
      local v = le32(subst[code[i]])
      code[i], code[i + 1], code[i + 2], code[i + 3] = v[1], v[2], v[3], v[4]
      i = i + 4
    else
      i = i + 1
    end
  end
  return code
end
-- Generic entry trace into the same ring (run/re_camleak/ftrace.s): records tag, phase, frame, return address,
-- ECX, arg0, 4 dwords at arg0+off, EBP caller chain. stolen = original prologue bytes (no rel. addressing).
local FT_NAMES = {}
local ft_slot = 0
local function install_ftrace(addr, stolen, tag, off, name)
  for k, b in ipairs(stolen) do
    if U8(addr + k - 1) ~= b then log(string.format("ftrace: %s prologue mismatch", name)) return end
  end
  local hex = "609c8b150300aaaa81e2ff030000c1e20681c20400aaaaa10500aaaa8902a10600aaaa8942048b442424894208894a0cc7423c" ..
      "0700aaaa8b44242889421085c0741c050800aaaa8b08894a148b4804894a188b4808894a1c8b480c894a3889e8bb060000008d7a20" ..
      "89c129e181f900001000730d8b4804890f83c7048b004b75e7ff050300aaaa9d61"
  local code = QT_assemble(hex, { [3] = QT.idx, [4] = QT.buf, [5] = QT.phase, [6] = QT.frame, [7] = tag, [8] = off })
  for _, b in ipairs(stolen) do code[#code + 1] = b end
  code[#code + 1] = 0x68
  for _, b in ipairs(le32(addr + #stolen)) do code[#code + 1] = b end
  code[#code + 1] = 0xC3
  local s = 0x400 + ft_slot * 0x100
  ft_slot = ft_slot + 1
  for k, b in ipairs(code) do QT.page[s + k - 1] = b end
  local rel = le32(QT.base + s - (addr + 5))
  local p = { 0xE9, rel[1], rel[2], rel[3], rel[4] }
  for k = 6, #stolen do p[k] = 0x90 end
  patch_bytes(addr, p)
  FT_NAMES[tag] = name
  log(string.format("ftrace: %s @%08x tag=%d", name, addr, tag))
end
local function install_qtrace()
  if U8(0x0089d7a0) ~= 0x55 or U8(0x0089d7a3) ~= 0x56 or U8(0x0089d7a5) ~= 0xF1 then
    log("qtrace: push_back prologue unexpected") return
  end
  local page = ffi.cast("uint8_t*", C.VirtualAlloc(nil, 0x20000, 0x3000, 0x40))
  local base = tonumber(ffi.cast("uint32_t", page))
  QT = { page = page, base = base, vnew = base + 0x200, vpend = base + 0x204, idx = base + 0x208, phase = base + 0x20c,
    frame = base + 0x210, buf = base + 0x1000, rd = 0 }
  local hex = "3b0d0100aaaa74143b0d0200aaaa740c5589e55689ce68a6d78900c3609c8b150300aaaa81e2ff030000c1e20681c20400aaaa" ..
      "a10500aaaa8902a10600aaaa8942048b442424894208894a0c8b4424288b0089421085c074128b4814894a148b4818894a188b481c" ..
      "894a1c89e8bb060000008d7a2089c129e181f900001000730d8b4804890f83c7048b004b75e7ff050300aaaa9d61e975ffffff"
  local code = QT_assemble(hex, { QT.vnew, QT.vpend, QT.idx, QT.buf, QT.phase, QT.frame })
  for k, b in ipairs(code) do page[k - 1] = b end
  local gw = gridworld()
  ffi.cast("uint32_t*", QT.vnew)[0] = gw + 0x5a4
  ffi.cast("uint32_t*", QT.vpend)[0] = gw + 0x5b0
  local rel = le32(base - (0x0089d7a0 + 5))
  patch_bytes(0x0089d7a0, { 0xE9, rel[1], rel[2], rel[3], rel[4], 0x90 })
  log(string.format("qtrace: installed stub %08x gw=%08x", base, gw))
end
local function qtrace_sizes(tag, f)
  local gw = gridworld()
  log(string.format("QS f=%d %s new=%d pend=%d bud=%d", f, tag, (U32(gw + 0x5a8) - U32(gw + 0x5a4)) / 4,
    (U32(gw + 0x5b4) - U32(gw + 0x5b0)) / 4, I32(gw + 0x5bc)))
end
local function qtrace_dump()
  if not QT then return end
  local n = U32(QT.idx)
  if n - QT.rd > 1024 then log(string.format("Q overflow lost=%d", n - QT.rd - 1024)) QT.rd = n - 1024 end
  local mb = mat_begin()
  local gw = gridworld()
  while QT.rd < n do
    local e = ffi.cast("uint32_t*", QT.buf + bit.band(QT.rd, 0x3ff) * 64)
    local ch = {}
    for k = 8, 13 do ch[#ch + 1] = string.format("%08x", e[k]) e[k] = 0 end
    local tag = e[15]
    e[15] = 0
    if tag ~= 0 then
      local u = ffi.new("uint32_t[4]", e[5], e[6], e[7], e[14])
      local d = ffi.cast("double*", u)
      log(string.format("T %s ph=%d f=%d ret=%08x ecx=%08x a0=%08x w=%08x,%08x,%08x,%08x d=%.3f,%.3f chain=%s",
        FT_NAMES[tag] or tostring(tag), e[0], e[1], e[2], e[3], e[4], e[5], e[6], e[7], e[14], d[0], d[1],
        table.concat(ch, ",")))
    else
      local mat = (e[5] ~= 0 and e[5] >= mb) and math.floor((e[5] - mb) / 0x290) or -1
      log(string.format("Q ph=%d f=%d ret=%08x %s cell=%08x mat=%d xy=%d,%d chain=%s", e[0], e[1], e[2],
        (e[3] == gw + 0x5a4) and "NEW" or "PEND", e[4], mat, tonumber(ffi.cast("int32_t", e[6])),
        tonumber(ffi.cast("int32_t", e[7])), table.concat(ch, ",")))
    end
    QT.rd = QT.rd + 1
  end
end
-- Box2D body dump (SPIKE_B_BODIES=<last frame>): b2World = *(GridWorld+0x530), body list head +0x1922c,
-- next = body+0xb8; per body hash of the numeric fields +0x08..+0xaf (type, flags, xf, sweep, vel, force).
local BODIES_TO = tonumber(os.getenv("SPIKE_B_BODIES") or "0") or 0
local BODIES_FROM = tonumber(os.getenv("SPIKE_B_BODIES_FROM") or "1") or 1
local BRAW = os.getenv("SPIKE_B_BRAW") == "1"
local function body_dump(tag, f)
  local w = U32(gridworld() + 0x530)
  if w == 0 then return end
  local b = U32(w + 0x1922c)
  local parts, H, n = {}, 0x811c9dc5, 0
  while b ~= 0 and n < 5000 do
    local p = ffi.cast("uint32_t*", b)
    local h = 0x811c9dc5
    -- skip padding: high half of +0x0c (after uint16 m_flags) and +0x14 (before the double m_xf)
    h = fnv(fnv(fnv(h, p[2]), bit.band(p[3], 0xffff)), p[4])
    for k = 6, 43 do h = fnv(h, p[k]) end
    H = fnv(H, h)
    n = n + 1
    local d = ffi.cast("double*", b)
    parts[#parts + 1] = string.format("%d,%d,%d:%08x", math.floor(d[3]), math.floor(d[4]), bit.band(p[3], 0xffff),
      h % 0x100000000)
    if BRAW and tag == "post" then
      local w = {}
      for k = 0, 59 do w[#w + 1] = string.format("%08x", p[k]) end
      log(string.format("BR f=%d %d %s", f, n, table.concat(w, " ")))
    end
    b = p[46]
  end
  log(string.format("BD f=%d %s n=%d h=%08x %s", f, tag, n, H % 0x100000000, table.concat(parts, " ")))
end
-- Per-frame chunk hash (SPIKE_B_CHASH=<last frame>, chunks SPIKE_B_CHUNKS="cx,cy;cx,cy") at pre and post, and a
-- full cell dump (index:material) of those chunks at frame SPIKE_B_CDUMP to run/re_camleak/cdump_<inst>_<f>_<tag>.txt
local CHASH_TO = tonumber(os.getenv("SPIKE_B_CHASH") or "0") or 0
local CDUMP_F = tonumber(os.getenv("SPIKE_B_CDUMP") or "-1") or -1
local CH_LIST = {}
for cx, cy in (os.getenv("SPIKE_B_CHUNKS") or "0,-1;2,1"):gmatch("(-?%d+),(-?%d+)") do
  CH_LIST[#CH_LIST + 1] = { tonumber(cx), tonumber(cy) }
end
local function chunk_trace(tag, f)
  local mb = mat_begin()
  local parts = {}
  local df = (f == CDUMP_F) and io.open(string.format("%s/run/re_camleak/cdump_%d_%d_%s.txt", root, inst, f, tag), "w")
  for _, c in ipairs(CH_LIST) do
    local ch = chunk_ptr(c[1] * 512, c[2] * 512)
    if ch == 0 then
      parts[#parts + 1] = c[1] .. "," .. c[2] .. "=unloaded"
    else
      local cells = ffi.cast("uint32_t*", U32(ch))
      local h, n = 0x811c9dc5, 0
      local rows = {}
      for i = 0, 0x40000 - 1 do
        local cc = cells[i]
        if cc ~= 0 then
          local m = ffi.cast("uint32_t*", cc)[5] - mb
          h = fnv(fnv(h, i), m)
          n = n + 1
          if df then rows[#rows + 1] = string.format("%d %d %d", c[1] * 512 + i % 512, c[2] * 512 + math.floor(i / 512), m / 0x290) end
        end
      end
      if df then df:write(table.concat(rows, "\n"), "\n") end
      parts[#parts + 1] = string.format("%d,%d=%08x/%d", c[1], c[2], h % 0x100000000, n)
    end
  end
  if df then df:close() end
  log(string.format("CH f=%d %s %s", f, tag, table.concat(parts, " ")))
end
-- GridParticles dump (SPIKE_B_PARTS=<last frame>): GridParticles = *(GridWorld+0x540); particle vectors at +0x34/+0x40/
-- +0x4c (0x70-byte particles). Hash = dwords +0x00..+0x4f and +0x54..+0x6b plus low byte of +0x6c (skips the +0x50
-- pointer and padding). Also counts particles with +0x6a bit1 (snapshot-collision particles).
local PARTS_TO = tonumber(os.getenv("SPIKE_B_PARTS") or "0") or 0
local function parts_dump(tag, f)
  local gp = U32(gridworld() + 0x540)
  if gp == 0 then return end
  local out = {}
  for _, off in ipairs({ 0x34, 0x40, 0x4c, 0x88 }) do
    local b, e = U32(gp + off), U32(gp + off + 4)
    if off == 0x88 then
      out[#out + 1] = string.format("L88:%d", (e - b) / 4)
    else
      local n = (e - b) / 0x70
      local h, s1 = 0x811c9dc5, 0
      for i = 0, n - 1 do
        local p = ffi.cast("uint32_t*", b + i * 0x70)
        for k = 0, 19 do h = fnv(h, p[k]) end
        for k = 21, 26 do h = fnv(h, p[k]) end
        h = fnv(h, bit.band(p[27], 0xff))
        if bit.band(ffi.cast("uint8_t*", p)[0x6a], 2) ~= 0 then s1 = s1 + 1 end
      end
      out[#out + 1] = string.format("L%x:%d:%d:%08x", off, n, s1, h % 0x100000000)
    end
  end
  out[#out + 1] = string.format("prng=%.17g", ffi.cast("double*", 0x01222388)[0])
  local g = ffi.cast("float*", gg() + 0x180)
  local gw = gridworld()
  out[#out + 1] = string.format("gb=%g,%g,%g,%g r498=%d,%d,%d,%d", g[0], g[1], g[2], g[3], I32(gw + 0x498),
    I32(gw + 0x49c), I32(gw + 0x4a0), I32(gw + 0x4a4))
  log(string.format("PD f=%d %s %s", f, tag, table.concat(out, " ")))
  if f == (tonumber(os.getenv("SPIKE_B_PRAW") or "-1") or -1) then
    local b, e = U32(gp + 0x4c), U32(gp + 0x50)
    for i = 0, math.min((e - b) / 0x70, 64) - 1 do
      local p = ffi.cast("uint32_t*", b + i * 0x70)
      local w = {}
      for k = 0, 27 do w[#w + 1] = string.format("%08x", p[k]) end
      log(string.format("PRAW f=%d %s %d %s", f, tag, i, table.concat(w, " ")))
    end
  end
end
-- P13: camera-driven far-body freeze. Camera::SetRect FUN_006f2d20 -> GridWorld::OnCameraRectChanged FUN_00720130
-- -> (camera centre moved > 512 px since GridWorld+0x4b8) FUN_007212b0 -> FUN_00721050: freezes dynamic bodies
-- with PhysicsBody flag +0x29 that are >= 350 px (asleep) / 750 px (awake) from the camera centre into grid cells
-- (FUN_00720960) and culls terrain collision bodies away from dynamic bodies (FUN_007202c0 -> FUN_0072b5e0 ->
-- b2World::DestroyBody). Render-camera moves therefore changed Box2D state. Patch: (a) drop the call at 0x007202a7,
-- (b) replace the camera-centre distance (0x00721177, 26 bytes) by min distance over the sim foci, (c) trigger
-- FUN_00721050 inside the sim window when any focus moved > 512 px since the last trigger.
local P13 = nil
local function install_p13()
  if U8(0x007202a7) ~= 0xE8 or I32(0x007202a8) ~= 0x1004 or U8(0x00721177) ~= 0x0F or U8(0x00721178) ~= 0x28 or
      U8(0x0072118d) ~= 0xF3 or U8(0x00721190) ~= 0xD0 then
    log("p13: unexpected bytes, skipped") return
  end
  local s = 0x880 -- det_exec: code 0x880.., N 0x900, save 0x910, scratch 0x920, foci 0x940.. (max 16)
  local E = tonumber(ffi.cast("uint32_t", det_exec))
  local code = QT_assemble("50510f112d0300aaaa8b0d0100aaaab80200aaaac7050400aaaaffff7f7ff30f10150400aaaa85c97433f30f" ..
    "1028f30f5ce8f30f59edf30f112d0400aaaaf30f106804f30f5ce9f30f59edf30f582d0400aaaaf30f5dd583c00849ebc90f102d0300aaaa5958c3",
    { E + 0x900, E + 0x940, E + 0x910, E + 0x920 })
  for k, b in ipairs(code) do det_exec[s + k - 1] = b end
  P13 = { n = ffi.cast("int32_t*", E + 0x900), foci = ffi.cast("float*", E + 0x940), last = {},
    freeze = ffi.cast("void(__fastcall*)(uint32_t, uint32_t, int32_t*)", 0x00721050), rect = ffi.new("int32_t[4]"),
    calls = 0 }
  P13.n[0] = 0
  patch_bytes(0x007202a7, { 0x83, 0xC4, 0x04, 0x90, 0x90 })
  local rel = le32(E + s - (0x00721177 + 5))
  local p = { 0xE8, rel[1], rel[2], rel[3], rel[4] }
  for k = 6, 26 do p[k] = 0x90 end
  patch_bytes(0x00721177, p)
  log(string.format("p13: installed (stub %08x)", E + s))
end
-- inside the sim window: foci = list of {x, y, ...} centres in fixed order
local function p13_step(f, foci)
  if not P13 then return end
  local trig = false
  for i, q in ipairs(foci) do
    P13.foci[2 * (i - 1)] = q[1]
    P13.foci[2 * (i - 1) + 1] = q[2]
    local l = P13.last[i]
    if not l or (q[1] - l[1]) ^ 2 + (q[2] - l[2]) ^ 2 > 262144 then trig = true end
  end
  P13.n[0] = #foci
  if trig then
    for i, q in ipairs(foci) do P13.last[i] = { q[1], q[2] } end
    local q = foci[1]
    P13.rect[0], P13.rect[1], P13.rect[2], P13.rect[3] = q[1] - 213, q[2] - 120, 427, 240 -- centre unused (patched)
    P13.freeze(gridworld(), 0, P13.rect)
    P13.calls = P13.calls + 1
    log(string.format("p13: freeze pass f=%d n=%d calls=%d", f, #foci, P13.calls))
  end
end
function mfsplit_step(f)
  if f == 1 then
    fix_seeds("frame1")
    if os.getenv("SPIKE_B_NOP13") ~= "1" then install_p13() end
    make_unload_thunk()
    if (os.getenv("SPIKE_B_PROBE") or "") ~= "" then install_probes() end
    rng_sim = rng_save()
    log(string.format("mfsplit: rng=%s simcam=%s", tostring(SPLIT_RNG), tostring(SPLIT_CAM)))
    if os.getenv("SPIKE_B_QTRACE") == "1" then install_qtrace() end
    if QT and os.getenv("SPIKE_B_FTRACE") == "1" then
      install_ftrace(0x009bf120, { 0x55, 0x8b, 0xec, 0x51, 0x56 }, 1, 8, "CreateBody")
      install_ftrace(0x009bf200, { 0x55, 0x8b, 0xec, 0x83, 0xe4, 0xf8 }, 2, 0x18, "DestroyBody")
    end
  end
  if QT then
    qtrace_dump()
    ffi.cast("uint32_t*", QT.phase)[0] = 1
    ffi.cast("uint32_t*", QT.frame)[0] = f
    qtrace_sizes("pre", f)
  end
  if f <= BODIES_TO and f >= BODIES_FROM then body_dump("pre", f) end
  if f <= CHASH_TO or f == CDUMP_F then chunk_trace("pre", f) end
  -- enter sim window
  if SPLIT_RNG then rng_render = rng_save(); rng_load(rng_sim) end
  if f <= PARTS_TO then parts_dump("pre", f) end
  if f == 2 then GameSetCameraFree(true) end
  if SPLIT_CAM and f >= 2 then GameSetCameraPos(P[1], P[2]) end  -- identical sim camera (player 0)
  for _, q in ipairs(split_foci(f)) do native_focus(q[1], q[2], q[3], q[4]) end
  p13_step(f, split_foci(f))
  probe_vals[0] = string.format("%08x/%08x", U32(0x0120741c), U32(RAND_STATE))
  local sites = {}
  for i, q in ipairs(split_foci(f)) do sites[i] = { "S" .. i, q[1], q[2] } end
  if f == 150 or (f > 150 and f % 200 == 0) then
    for _, s in ipairs(sites) do
      for i = -2, 2 do GameCreateParticle("water", s[2] + i * 40, s[3] - 100, 150, 0, 0, false, false) end
      GameCreateParticle("sand", s[2] + 20, s[3] - 120, 150, 0, 0, false, false)
    end
  end
  if f == 160 or f == 760 then
    for _, s in ipairs(sites) do
      EntityLoad("data/entities/animals/zombie.xml", s[2] + 60, s[3] - 20)
      EntityLoad("data/entities/animals/shotgunner.xml", s[2] - 60, s[3] - 20)
    end
  end
  if os.getenv("SPIKE_B_BOMBS") == "1" and (f == 240 or f == 840) then
    for _, s in ipairs(sites) do EntityLoad("data/entities/projectiles/bomb.xml", s[2] + 30, s[3] - 40) end
  end
  if f % 60 == 0 then
    measuring = true
    log(string.format("H f=%d %s", f, table.concat(chunk_hash_all(), " ")))
    local eh = {}
    for _, s in ipairs(sites) do eh[#eh + 1] = s[1] .. "=" .. region_entity_hash(s[2], s[3], 700) end
    log(string.format("E f=%d %s rand=%08x glcg=%08x", f, table.concat(eh, " "), U32(RAND_STATE), U32(0x0120741c)))
  end
end
function mfsplit_post(f)
  if QT then qtrace_dump() qtrace_sizes("post", f) end
  if f <= BODIES_TO and f >= BODIES_FROM then body_dump("post", f) end
  if f <= CHASH_TO or f == CDUMP_F then chunk_trace("post", f) end
  if f <= PARTS_TO then parts_dump("post", f) end
  if probe_cb then
    local parts = { "0:" .. tostring(probe_vals[0]) }
    for i = 1, #PROBE_SITES do parts[#parts + 1] = i .. ":" .. tostring(probe_vals[i]) end
    parts[#parts + 1] = string.format("99:%08x/%08x", U32(0x0120741c), U32(RAND_STATE))
    if probe_vals.q7 then parts[#parts + 1] = "q7:" .. probe_vals.q7 end
    if probe_vals.gb5 then parts[#parts + 1] = "gb5:" .. probe_vals.gb5 end
    local gp = U32(gridworld() + 0x540)
    if gp ~= 0 and U32(gp + 0x2c) ~= 0 then parts[#parts + 1] = string.format("qs:%g", F32(U32(gp + 0x2c) + 0xa8)) end
    local w = U32(gridworld() + 0x530)
    if w ~= 0 then  -- cheap Box2D hash: type, flags, xf, sweep c/a, velocities (+0x08,+0x0c lo16,+0x18..+0x97)
      local b, H, n = U32(w + 0x1922c), 0x811c9dc5, 0
      while b ~= 0 and n < 20000 do
        local p = ffi.cast("uint32_t*", b)
        H = fnv(fnv(H, p[2]), bit.band(p[3], 0xffff))
        for k = 6, 37 do H = fnv(H, p[k]) end
        n = n + 1
        b = p[46]
      end
      parts[#parts + 1] = string.format("bh:%08x/%d", H % 0x100000000, n)
    end
    log("PR f=" .. f .. " " .. table.concat(parts, " "))
  end
  own_unloader(f)                       -- still inside the sim window
  -- leave sim window
  if SPLIT_RNG then rng_sim = rng_save(); rng_load(rng_render) end
  if f >= 2 then
    local off = ((inst % 2 == 0) or os.getenv("SPIKE_B_SAMEVIEW") == "1") and 0 or 1000
    GameSetCameraPos(P[1] + off, P[2])  -- this client's own render camera
  end
  if QT then ffi.cast("uint32_t*", QT.phase)[0] = 2 end
end
function mfdet_step(f)
  if MODE == "mfsplit" then return mfsplit_step(f) end
  if f == 1 then fix_seeds("frame1") end
  if os.getenv("SPIKE_B_SERIAL") == "1" then ffi.cast("uint8_t*", 0x01153177)[0] = 0 end -- USE_CUSTOM_THREADPOOL=0
  local sites = { { "P", P[1], P[2] } }
  local nf = (MODE == "mfdet_one") and 0 or 4
  for i = 1, nf do sites[#sites + 1] = { "F" .. i, F[i][1], F[i][2] } end
  native_focus(P[1], P[2], 400, 300)
  if f >= 30 then for i = 1, nf do native_focus(F[i][1], F[i][2], 300, 200) end end
  if (MODE == "mfdet_diff" or MODE == "mfdet_canon") and f >= 120 then
    if f == 120 then GameSetCameraFree(true) log("mfdet: camera split") end
    local tgt = (inst % 2 == 0) and F[2] or F[3]
    if MODE == "mfdet_canon" then tgt = P end -- canonical simulation camera, identical on all clients
    GameSetCameraPos(tgt[1], tgt[2])
  end
  if f == 150 then
    for _, s in ipairs(sites) do
      for i = -2, 2 do GameCreateParticle("water", s[2] + i * 40, s[3] - 100, 200, 0, 0, false, false) end
      GameCreateParticle("sand", s[2] + 20, s[3] - 120, 200, 0, 0, false, false)
      EntityLoad("data/entities/animals/zombie.xml", s[2] + 60, s[3] - 20)
      EntityLoad("data/entities/animals/shotgunner.xml", s[2] - 60, s[3] - 20)
    end
  elseif f == 240 and os.getenv("SPIKE_B_NOBOMB") ~= "1" then
    for _, s in ipairs(sites) do
      EntityLoad("data/entities/projectiles/bomb.xml", s[2] + 30, s[3] - 40)
      EntityLoad("data/entities/props/physics_box_explosive.xml", s[2] - 40, s[3] - 60)
    end
  end
  if f % 60 == 0 then
    measuring = true
    local parts = chunk_hash_all()
    log(string.format("H f=%d %s", f, table.concat(parts, " ")))
    local eh = {}
    for _, s in ipairs(sites) do eh[#eh + 1] = s[1] .. "=" .. region_entity_hash(s[2], s[3], 700) end
    log(string.format("E f=%d %s rand=%08x glcg=%08x", f, table.concat(eh, " "), U32(RAND_STATE), U32(0x0120741c)))
  end
end

local function create_entity_focus(x, y)
  local e = EntityCreateNew("mf_focus")
  EntitySetTransform(e, x, y)
  EntityAddComponent2(e, "StreamingKeepAliveComponent", {})
  focus_ents[#focus_ents + 1] = e
  log(string.format("f=%d entity focus %d at %d,%d", lf, e, x, y))
end

local function step()
  if not P then return end
  lf = lf + 1
  local f = lf
  if f == 1 then
    water_mp = mat_begin() + 0x290 * CellFactory_GetType("water")
  end
  measuring = false

  if MFDET then
    mfdet_step(f)
    return
  end
  if COSTLIKE then
    local k, n = cost_phase(f)
    if (f - 1) % COST_LEN == 0 then log(string.format("f=%d phase k=%d N=%s", f, k, tostring(n))) end
    for i = 1, (n or 0) do native_focus(P[1] + COST_POS[i][1], P[2] + COST_POS[i][2], 300, 200) end
    return
  end

  -- foci
  if foci_on then
    if f == 601 then create_entity_focus(F[1][1], F[1][2]) end
    if f >= 1201 and f <= 2400 then
      for i = 2, 4 do native_focus(F[i][1], F[i][2], 300, 200) end
    end
  end
  if f == 2401 and foci_on then
    for _, e in ipairs(focus_ents) do EntityKill(e) end
    log("f=" .. f .. " foci removed")
  end
  -- camera far away in phase D (both instances)
  if f == 1801 then
    GameSetCameraFree(true)
    GameSetCameraPos(P[1] - 2600, P[2] + 300)
    log("f=" .. f .. " camera moved far")
  end
  if f >= 1801 and f <= 2400 then GameSetCameraPos(P[1] - 2600, P[2] + 300) end

  -- water drops: near player and at focus 1, same frames on both instances
  local drop_frames = { [700] = true, [1300] = true, [1900] = true }
  if drop_frames[f] then
    spawn_sites = {}
    local sites = { { "player", P[1], P[2] - 40 }, { "F1", F[1][1], F[1][2] } }
    for _, s in ipairs(sites) do
      local loaded = chunk_ptr(s[2], s[3]) ~= 0
      local ax, ay, ly = s[2], s[3], s[3]
      if loaded then ax, ay, ly = find_drop(s[2], s[3], 200) end
      spawn_sites[s[1]] = { ax, ay, ly }
      log(string.format("f=%d drop %s at %d,%d landing_y=%d loaded=%s", f, s[1], ax, ay, ly, tostring(loaded)))
    end
  end
  for name, s in pairs(spawn_sites) do
    if (f >= 700 and f < 710) or (f >= 1300 and f < 1310) or (f >= 1900 and f < 1910) then
      GameCreateParticle("water", s[1], s[2], 60, 0, 0, false, false, true)
    end
  end
end

local function report_pre()
  local f = lf
  local cm = chunkmap()
  local nloaded = I32(cm + 0xc)
  local npend = U32(0x0120800c)
  local keepn = math.floor((U32(0x01207fe8) - U32(0x01207fe4)) / 0x18)
  local cx, cy = GameGetCameraPos()
  local line = string.format("f=%d gf=%d loaded=%d pend=%d keep=%d cam=%.0f,%.0f", f, GameGetFrameNum(), nloaded, npend,
    keepn, cx, cy)
  if MFDET then line = line .. string.format(" glcg=%08x rand=%08x", U32(0x0120741c), U32(RAND_STATE)) end
  if not COSTLIKE and f % 5 == 0 then
    measuring = true
    for _, k in ipairs({ "player", "F1" }) do
      local s = spawn_sites[k]
      if s then line = line .. " L" .. k .. "=" .. probe(s[1], s[3], 48, water_mp) end
    end
    local fl = {}
    for i = 1, 4 do fl[#fl + 1] = (chunk_ptr(F[i][1], F[i][2]) ~= 0) and "1" or "0" end
    line = line .. " Floaded=" .. table.concat(fl)
  end
  if not COSTLIKE and f % 30 == 0 then
    line = line .. " Cp=" .. probe_chunk(P[1], P[2], water_mp) .. " C1=" .. probe_chunk(F[1][1], F[1][2], water_mp) ..
        " C2=" .. probe_chunk(F[2][1], F[2][2], water_mp)
  end
  log(line)
  -- chunk load/unload events
  local per_frame_scan = (MODE ~= "focus" and not COSTLIKE and not MFDET)
  if per_frame_scan or f % 30 == 0 then
    measuring = true
    local cur = loaded_chunks()
    local set = {}
    local added, removed = {}, {}
    for _, i in ipairs(cur) do set[i] = true; if not prev_loaded[i] then added[#added + 1] = i end end
    for i, _ in pairs(prev_loaded) do if not set[i] then removed[#removed + 1] = i end end
    table.sort(added); table.sort(removed)
    if #added > 0 or #removed > 0 then
      log(string.format("  chunks f=%d +[%s] -[%s]", f, table.concat(added, ","), table.concat(removed, ",")))
    end
    prev_loaded = set
  end
end

local function report_post()
  local f = lf
  if f % 30 == 0 then
    measuring = true
    local n, tiles = active_tiles()
    -- count active tiles per region: near camera (within 600px of cam center), near each focus chunk, near player
    local cx, cy = GameGetCameraPos()
    local impl = U32(gridworld() + 0x5d0)
    local tsz = I32(U32(impl + 0x40) + 0xc)
    local near_cam, near_p, nearF = 0, 0, { 0, 0, 0, 0 }
    for _, t in ipairs(tiles) do
      local tx, ty = t[3] + 32, t[4] + 32
      if math.abs(tx - cx) < 600 and math.abs(ty - cy) < 400 then near_cam = near_cam + 1 end
      if math.abs(tx - P[1]) < 600 and math.abs(ty - P[2]) < 400 then near_p = near_p + 1 end
      for i = 1, 4 do
        if math.abs(tx - F[i][1]) < 600 and math.abs(ty - F[i][2]) < 400 then nearF[i] = nearF[i] + 1 end
      end
    end
    local infoci = 0
    if COSTLIKE then
      for _, t in ipairs(tiles) do
        local tx, ty = t[3] + 32, t[4] + 32
        for i = 1, #COST_POS do
          if math.abs(tx - (P[1] + COST_POS[i][1])) < 556 and math.abs(ty - (P[2] + COST_POS[i][2])) < 456 then
            infoci = infoci + 1
            break
          end
        end
      end
    end
    log(string.format("  tiles f=%d active=%d tsz=%d near_cam=%d near_player=%d nearF=%d,%d,%d,%d infoci=%d", f, n, tsz,
      near_cam, near_p, nearF[1], nearF[2], nearF[3], nearF[4], infoci))
    if hook_cb then
      log(string.format("  hook calls=%d jobs=%d waited=%.1fms max=%.1fms timeouts=%d", hook_stats.calls, hook_stats.jobs,
        hook_stats.waited_ms, hook_stats.max_ms, hook_stats.timeouts))
    end
  end
  if not COSTLIKE and (f % 600 == 0 or f == 2450) then
    for _, ph in ipairs({ "A_base", "B_1focus", "C_4foci", "D_camfar", "E_end" }) do log("STAT " .. summarize(ph)) end
  end
  if COSTLIKE and f == #COST_SCHED * COST_LEN + 10 then
    local keys = {}
    for k, _ in pairs(stats) do keys[#keys + 1] = k end
    table.sort(keys, function(a, b)
      local ka, kb = tonumber(a:match("^k(%d+)")) or 99, tonumber(b:match("^k(%d+)")) or 99
      if ka ~= kb then return ka < kb end
      return a < b
    end)
    for _, k in ipairs(keys) do log("STAT " .. summarize(k)) end
    log(string.format("HOOK calls=%d jobs=%d waited=%.1fms max=%.1fms timeouts=%d", hook_stats.calls, hook_stats.jobs,
      hook_stats.waited_ms, hook_stats.max_ms, hook_stats.timeouts))
  end
end

function OnWorldPreUpdate()
  local ok, err = pcall(function()
    step()
    if P then report_pre() end
  end)
  if not ok then log("pre error: " .. tostring(err)) end
  t_prev_pre, t_pre = t_pre, now_ms()  -- update timing starts after our own work
end

function OnWorldPostUpdate()
  local t_post = now_ms()
  -- mfdet_canon: after the simulation part of the frame, switch to this client's own view for rendering
  if MODE == "mfdet_canon" and P and lf >= 120 then
    local tgt = (inst % 2 == 0) and F[2] or F[3]
    GameSetCameraPos(tgt[1], tgt[2])
  end
  if MODE == "mfsplit" and P and lf >= 1 then
    local ok, err = pcall(mfsplit_post, lf)
    if not ok then log("mfsplit post error: " .. tostring(err)) end
  end
  if P and t_pre and t_prev_pre then
    local upd = t_post - t_pre
    local frm = t_pre - t_prev_pre
    if not measuring and lf > 120 then add_stat(phase_of(lf), upd, frm) end
  end
  local ok, err = pcall(function() if P then report_post() end end)
  if not ok then log("post error: " .. tostring(err)) end
end
