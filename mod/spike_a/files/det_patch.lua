-- Noita determinism runtime patch (build 0x6794ee3c, "Jan 25 2025"). Portable: pure LuaJIT FFI, no files touched.
-- local det = dofile("mods/<mod>/files/det_patch.lua")
-- det.install({ rand=true, streamclock=true, chunkwait=true, inline=true, pinit=true })  -- in init.lua (top level)
-- det.reseed(seed)  -- in OnMagicNumbersAndWorldSeedInitialized, OnWorldInitialized and first OnWorldPreUpdate
-- All code patches are process-wide and idempotent: a re-run of init.lua (new game / world restart, new Lua state)
-- finds the stub page via a process environment variable and only re-verifies the sites.
local ffi = require("ffi")
local bit = require("bit")
pcall(ffi.cdef, [[
int __stdcall VirtualProtect(void* addr, size_t size, uint32_t newprot, uint32_t* oldprot);
void* __stdcall VirtualAlloc(void* addr, size_t size, uint32_t type, uint32_t prot);
uint32_t __stdcall GetEnvironmentVariableA(const char* name, char* buf, uint32_t size);
int __stdcall SetEnvironmentVariableA(const char* name, const char* value);
void* __stdcall GetModuleHandleA(const char* name);
void* __stdcall GetProcAddress(void* mod, const char* name);
]])
local C = ffi.C
local M = {}

local function u8(a) return ffi.cast("uint8_t*", a) end
local function u32(a) return ffi.cast("uint32_t*", a) end
local function le32(v)
  v = v % 0x100000000
  return { v % 256, math.floor(v / 256) % 256, math.floor(v / 65536) % 256, math.floor(v / 16777216) % 256 }
end
local function cat(...)
  local out = {}
  for _, t in ipairs({ ... }) do for _, b in ipairs(t) do out[#out + 1] = b end end
  return out
end
local function hex(s)
  local t = {}
  for h in s:gmatch("%x%x") do t[#t + 1] = tonumber(h, 16) end
  return t
end
local function bytes_eq(addr, t)
  local p = u8(addr)
  for i, b in ipairs(t) do if p[i - 1] ~= b then return false end end
  return true
end
local function write(addr, t)
  local old = ffi.new("uint32_t[1]")
  assert(C.VirtualProtect(ffi.cast("void*", addr), #t, 0x40, old) ~= 0, "VirtualProtect failed")
  local p = u8(addr)
  for i, b in ipairs(t) do p[i - 1] = b end
  C.VirtualProtect(ffi.cast("void*", addr), #t, old[0], old)
end
-- patch a site: already patched -> keep; original -> patch; anything else -> error (wrong build)
local function site(name, addr, orig, new)
  if bytes_eq(addr, new) then return "kept" end
  if not bytes_eq(addr, orig) then error(string.format("det_patch: unexpected bytes at %08x (%s)", addr, name)) end
  write(addr, new)
  return "patched"
end
local function call_to(from, to) return cat({ 0xE8 }, le32(to - (from + 5))) end

-- stub page layout: code 0x000..0x0ff (run/re_grid/stubs.s), data at 0x800
local STUBS = "ff0522222222a11111111169c0fd43030005c39e2600a311111111c1e81025ff7f0000c3eb1a66902e8db426000000002e8db426000000002e8db426000000008b442404a333333333c38db6000000002e8db426000000002e8db42600000000a14c37220185c07409db00dc0d44444444c3d9eec38d76002e8db4260000000051565753bb204e00008b3d0880200185ff74558b3739fe744f8b462485c0741783b8c800000001740e4b743c6a01b855555555ffd0ebd28b460880780d00750e89c68b0680780d0075cb89c6ebf48b460480780d00750d3b7008750489c6ebee89c6ebb189c6ebad5b5f5e59b8a0d67300ffe0"
local OFF_RAND, OFF_SRAND, OFF_CLOCK, OFF_CHUNKWAIT = 0x00, 0x40, 0x60, 0x80
local D_RAND_STATE, D_RAND_COUNT, D_SRAND_LAST, D_INV60 = 0x800, 0x804, 0x808, 0x810

local function stub_page()
  local buf = ffi.new("char[32]")
  local n = C.GetEnvironmentVariableA("NOITA_DET_STUBS", buf, 32)
  if n > 0 then return tonumber(ffi.string(buf, n), 16), false end
  local page = tonumber(ffi.cast("uint32_t", C.VirtualAlloc(nil, 4096, 0x3000, 0x40)))
  assert(page ~= 0, "VirtualAlloc failed")
  local code = hex(STUBS)
  local subst = {
    [0x11111111] = page + D_RAND_STATE, [0x22222222] = page + D_RAND_COUNT, [0x33333333] = page + D_SRAND_LAST,
    [0x44444444] = page + D_INV60,
    [0x55555555] = tonumber(ffi.cast("uint32_t", C.GetProcAddress(C.GetModuleHandleA("kernel32.dll"), "Sleep"))),
  }
  for i = 1, #code - 3 do
    local v = code[i] + code[i + 1] * 256 + code[i + 2] * 65536 + code[i + 3] * 16777216
    if subst[v] then
      local r = le32(subst[v])
      code[i], code[i + 1], code[i + 2], code[i + 3] = r[1], r[2], r[3], r[4]
    end
  end
  local p = u8(page)
  for i, b in ipairs(code) do p[i - 1] = b end
  ffi.cast("double*", page + D_INV60)[0] = 1 / 60
  u32(page + D_RAND_STATE)[0] = 0x1234567
  C.SetEnvironmentVariableA("NOITA_DET_STUBS", string.format("%x", page))
  return page, true
end

function M.install(opts)
  local page, fresh = stub_page()
  M.page = page
  M.rand_state, M.rand_count, M.srand_last = page + D_RAND_STATE, page + D_RAND_COUNT, page + D_SRAND_LAST
  local log = { fresh and "stubs=new" or "stubs=reused" }
  if opts.rand then
    -- msvcr120 rand/srand IAT -> process-global LCG (CRT rand is per-thread and srand(time)-seeded)
    for _, e in ipairs({ { 0x00f07450, page + OFF_RAND }, { 0x00f073d4, page + OFF_SRAND } }) do
      if u32(e[1])[0] ~= e[2] then write(e[1], le32(e[2])) end
    end
    log[#log + 1] = "rand"
  end
  if opts.streamclock then
    -- streaming throttle FUN_00743990: platform->GetTime() (mov eax,[eax+0x6c]; call eax) -> frame_num/60
    for _, a in ipairs({ 0x00743a47, 0x00743a92, 0x00743ab7 }) do
      site("streamclock", a, { 0x8B, 0x40, 0x6C, 0xFF, 0xD0 }, call_to(a, page + OFF_CLOCK))
    end
    log[#log + 1] = "streamclock"
  end
  if opts.chunkwait then
    -- GridWorld::Update: call FUN_0073d6a0 (integrate finished chunk jobs) -> wait for all pending jobs first
    site("chunkwait", 0x0071a825, { 0xE8, 0x76, 0x2E, 0x02, 0x00 }, call_to(0x0071a825, page + OFF_CHUNKWAIT))
    log[#log + 1] = "chunkwait"
  end
  if opts.inline then
    -- job pool: FUN_0084c240 (group submit) always takes its inline path
    site("inline_group", 0x0084c251, { 0x80, 0x7F, 0x25, 0x00, 0x74, 0x20 }, { 0x80, 0x7F, 0x25, 0x00, 0x90, 0x90 })
    -- FUN_0084c1a0 (async submit): run slot runner FUN_0084c430 inline, return the completed slot
    local a = 0x0084c1e0
    local new = cat({ 0x66, 0xC7, 0x43, 0x24, 0x00, 0x00, 0x89, 0x7B, 0x20, 0x53, 0xE8 }, le32(0x0084c430 - (a + 15)),
      { 0x83, 0xC4, 0x04, 0x8B, 0xC3, 0x5F, 0x5E, 0x5B, 0x8B, 0xE5, 0x5D, 0xC2, 0x04, 0x00 })
    site("inline_async", a,
      { 0x8D, 0x45, 0xF4, 0x66, 0xC7, 0x43, 0x24, 0x00, 0x00, 0x50, 0x8D, 0x45, 0xEC, 0x89, 0x7B, 0x20, 0x8B, 0x4F, 0x18,
        0x50, 0xC7, 0x45, 0xF4, 0x30, 0xC4, 0x84, 0x00, 0x89, 0x5D }, new)
    log[#log + 1] = "inline"
  end
  if opts.pinit then
    -- grid particle default ctor FUN_00712010 keeps bit1 of +0x6a (cosmetic/snapshot-collision flag) and bits 2..7
    -- of +0x6c from stale memory: AND byte[esi+0x6a],2 -> ,0 ; AND byte[esi+0x6c],0xfc -> ,0
    site("pinit_6a", 0x0071213b, { 0x80, 0x66, 0x6A, 0x02 }, { 0x80, 0x66, 0x6A, 0x00 })
    site("pinit_6c", 0x00712141, { 0x80, 0x66, 0x6C, 0xFC }, { 0x80, 0x66, 0x6C, 0x00 })
    log[#log + 1] = "pinit"
  end
  return table.concat(log, ",")
end

-- reset every engine RNG that is seeded from time/pid (call on each world init)
function M.reseed(seed)
  seed = seed % 0x100000000
  u32(0x01221bc0 + 0xa0)[0] = 0x0012d687        -- platform LCG (GetRandom, vtbl+0x9c): grid tile RNG seeds etc.
  u32(0x01221bc0 + 0xa4)[0] = seed              -- platform random seed (vtbl+0xa8)
  u32(0x0120741c)[0] = bit.bxor(seed, 0xc)      -- global MSVC-style LCG
  ffi.cast("double*", 0x01207420)[0] = seed % 0x7fffffff + 1   -- global Park-Miller (double)
  ffi.cast("double*", 0x01222388)[0] = (seed * 7 + 12345) % 0x7fffffff + 1  -- grid particle Park-Miller
  if M.rand_state then u32(M.rand_state)[0] = bit.bxor(seed, 0x1234567) end
end

return M
