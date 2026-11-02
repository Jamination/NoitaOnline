-- Stack-zero patch (research): before the per-frame application Update call (CALL [EDX+0xC] at 0x00ddbe0e in
-- FUN_00ddbd80), zero the main-thread stack below ESP, so uninitialized stack reads inside the simulation see the
-- same bytes on every instance. Idempotent per process via env var NOITA_STACKZERO_PAGE.
local ffi = require("ffi")
pcall(ffi.cdef, [[
int __stdcall VirtualProtect(void* addr, size_t size, uint32_t newprot, uint32_t* oldprot);
void* __stdcall VirtualAlloc(void* addr, size_t size, uint32_t type, uint32_t prot);
uint32_t __stdcall GetEnvironmentVariableA(const char* name, char* buf, uint32_t size);
int __stdcall SetEnvironmentVariableA(const char* name, const char* value);
]])
local C = ffi.C
local M = {}
local SITE = 0x00ddbe09
local ORIG = { 0xF3, 0x0F, 0x11, 0x04, 0x24, 0xFF, 0x52, 0x0C }

local function le32(v)
  v = v % 0x100000000
  return { v % 256, math.floor(v / 256) % 256, math.floor(v / 65536) % 256, math.floor(v / 16777216) % 256 }
end
local function append(t, ...)
  for _, x in ipairs({ ... }) do
    if type(x) == "table" then for _, b in ipairs(x) do t[#t + 1] = b end else t[#t + 1] = x end
  end
end

function M.install(bytes)
  bytes = bytes or 0x40000
  local p = ffi.cast("uint8_t*", SITE)
  if p[0] == 0xE8 then return "stackzero=kept" end
  for i, b in ipairs(ORIG) do if p[i - 1] ~= b then return "stackzero=MISMATCH" end end
  local page = tonumber(ffi.cast("uint32_t", C.VirtualAlloc(nil, 0x1000, 0x3000, 0x40)))
  local s = {}
  append(s, 0xF3, 0x0F, 0x11, 0x44, 0x24, 0x04, -- movss [esp+4],xmm0 (the original arg store)
    0x50, 0x51, 0x57, 0x8D, 0x7C, 0x24, 0xFC,    -- push eax/ecx/edi; lea edi,[esp-4]
    0xB9, le32(bytes / 4), 0x31, 0xC0,           -- mov ecx,n; xor eax,eax
    0xFD, 0xF3, 0xAB, 0xFC,                      -- std; rep stosd (downwards, guard-page friendly); cld
    0x5F, 0x59, 0x58, 0xFF, 0x62, 0x0C)          -- pop edi/ecx/eax; jmp [edx+0xc]
  local code = ffi.cast("uint8_t*", page)
  for i, b in ipairs(s) do code[i - 1] = b end
  local patch = {}
  append(patch, 0xE8, le32(page - (SITE + 5)), 0x90, 0x90, 0x90)
  local old = ffi.new("uint32_t[1]")
  C.VirtualProtect(ffi.cast("void*", SITE), #patch, 0x40, old)
  for i, b in ipairs(patch) do p[i - 1] = b end
  C.VirtualProtect(ffi.cast("void*", SITE), #patch, old[0], old)
  return string.format("stackzero=new@%08x/%x", page, bytes)
end

return M
