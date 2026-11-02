-- Zero-filling allocation patch (research): IAT-redirect noita.exe's malloc / operator new / _aligned_malloc to
-- stubs that memset the returned block to 0, and HeapAlloc to add HEAP_ZERO_MEMORY. Makes reads of uninitialized
-- heap memory deterministic. Idempotent per process via env var NOITA_ZALLOC_PAGE.
local ffi = require("ffi")
pcall(ffi.cdef, [[
int __stdcall VirtualProtect(void* addr, size_t size, uint32_t newprot, uint32_t* oldprot);
void* __stdcall VirtualAlloc(void* addr, size_t size, uint32_t type, uint32_t prot);
uint32_t __stdcall GetEnvironmentVariableA(const char* name, char* buf, uint32_t size);
int __stdcall SetEnvironmentVariableA(const char* name, const char* value);
]])
local C = ffi.C
local M = {}
local IAT = { malloc = 0x00f074f0, new = 0x00f07500, aligned = 0x00f0742c, heapalloc = 0x00f07060, memset = 0x00f07318 }

local function le32(v)
  v = v % 0x100000000
  return { v % 256, math.floor(v / 256) % 256, math.floor(v / 65536) % 256, math.floor(v / 16777216) % 256 }
end
local function append(t, ...)
  for _, x in ipairs({ ... }) do
    if type(x) == "table" then for _, b in ipairs(x) do t[#t + 1] = b end else t[#t + 1] = x end
  end
end
local function u32(a) return ffi.cast("uint32_t*", a) end
local function set_iat(slot, v)
  local old = ffi.new("uint32_t[1]")
  C.VirtualProtect(ffi.cast("void*", slot), 4, 0x04, old)
  u32(slot)[0] = v
  C.VirtualProtect(ffi.cast("void*", slot), 4, old[0], old)
end

function M.install()
  local buf = ffi.new("char[32]")
  if C.GetEnvironmentVariableA("NOITA_ZALLOC_PAGE", buf, 32) > 0 then return "zalloc=kept" end
  local page = tonumber(ffi.cast("uint32_t", C.VirtualAlloc(nil, 0x1000, 0x3000, 0x40)))
  local code = ffi.cast("uint8_t*", page)
  -- data cells: originals at +0x800..
  local cell = { malloc = page + 0x800, new = page + 0x804, aligned = page + 0x808, heapalloc = page + 0x80c,
    memset = page + 0x810 }
  for k, c in pairs(cell) do u32(c)[0] = u32(IAT[k])[0] end
  local off = 0
  local function emit(t)
    local at = page + off
    for i, b in ipairs(t) do code[off + i - 1] = b end
    off = off + #t + (16 - #t % 16)
    return at
  end
  -- cdecl f(n[, align]) -> p; memset(p, 0, n)
  local function zstub(orig_cell, nargs)
    local s = {}
    for _ = 1, nargs do append(s, 0xFF, 0x74, 0x24, 4 * nargs) end -- push args (reverse order preserved)
    append(s, 0xFF, 0x15, le32(orig_cell), 0x83, 0xC4, 4 * nargs, 0x85, 0xC0)
    local tail = {}
    append(tail, 0xFF, 0x74, 0x24, 0x04, 0x6A, 0x00, 0x50, 0xFF, 0x15, le32(cell.memset), 0x83, 0xC4, 0x0C)
    append(s, 0x74, #tail)
    append(s, tail)
    append(s, 0xC3)
    return emit(s)
  end
  local s_malloc = zstub(cell.malloc, 1)
  local s_new = zstub(cell.new, 1)
  local s_aligned = zstub(cell.aligned, 2)
  -- stdcall HeapAlloc(h, flags, n): flags |= HEAP_ZERO_MEMORY; jmp orig
  local h = {}
  append(h, 0x83, 0x4C, 0x24, 0x08, 0x08, 0xFF, 0x25, le32(cell.heapalloc))
  local s_heap = emit(h)
  set_iat(IAT.malloc, s_malloc)
  set_iat(IAT.new, s_new)
  set_iat(IAT.aligned, s_aligned)
  set_iat(IAT.heapalloc, s_heap)
  C.SetEnvironmentVariableA("NOITA_ZALLOC_PAGE", string.format("%08x", page))
  return string.format("zalloc=new@%08x", page)
end

return M
