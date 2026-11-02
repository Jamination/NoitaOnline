-- Native call tracer (research only). Hooks function entries with a JMP to a stub that appends one 32-byte record
-- {tag, return address, ECX, arg1, arg2, arg3, global LCG 0x0120741c, particle-RNG high dword 0x0122238c} to a
-- linear buffer, then runs the relocated prologue bytes and jumps back.
local ffi = require("ffi")
local bit = require("bit")
pcall(ffi.cdef, [[
int __stdcall VirtualProtect(void* addr, size_t size, uint32_t newprot, uint32_t* oldprot);
void* __stdcall VirtualAlloc(void* addr, size_t size, uint32_t type, uint32_t prot);
]])
local C = ffi.C
local M = {}
local CAP = 65536
local function le32(v)
  v = v % 0x100000000
  return { v % 256, math.floor(v / 256) % 256, math.floor(v / 65536) % 256, math.floor(v / 16777216) % 256 }
end
local function hex(s)
  local t = {}
  for h in s:gmatch("%x%x") do t[#t + 1] = tonumber(h, 16) end
  return t
end
local function append(t, ...)
  for _, x in ipairs({ ... }) do
    if type(x) == "table" then for _, b in ipairs(x) do t[#t + 1] = b end else t[#t + 1] = x end
  end
end
local function write(addr, t)
  local old = ffi.new("uint32_t[1]")
  C.VirtualProtect(ffi.cast("void*", addr), #t, 0x40, old)
  local p = ffi.cast("uint8_t*", addr)
  for i, b in ipairs(t) do p[i - 1] = b end
  C.VirtualProtect(ffi.cast("void*", addr), #t, old[0], old)
end

-- hooks: { {addr=, orig="hex prologue bytes to relocate (>=5, position independent)", tag=} ... }
function M.install(hooks)
  local code = ffi.cast("uint8_t*", C.VirtualAlloc(nil, 0x10000, 0x3000, 0x40))
  local buf = C.VirtualAlloc(nil, CAP * 32 + 64, 0x3000, 0x04)
  local base = tonumber(ffi.cast("uint32_t", code))
  local bufa = tonumber(ffi.cast("uint32_t", buf))
  local idxa = bufa + CAP * 32
  M.buf = ffi.cast("uint32_t*", buf)
  M.idx = ffi.cast("uint32_t*", idxa)
  local off = 0
  local out = {}
  for _, h in ipairs(hooks) do
    local orig = hex(h.orig)
    local p = ffi.cast("uint8_t*", h.addr)
    local same = true
    for i, b in ipairs(orig) do if p[i - 1] ~= b then same = false end end
    if not same then
      out[#out + 1] = string.format("%08x:MISMATCH", h.addr)
    else
      local s = {}
      local body = {}
      append(body, 0xC1, 0xE0, 0x05, 0x8D, 0xB8, le32(bufa), 0xC7, 0x07, le32(h.tag),
        0x8B, 0x4C, 0x24, 0x20, 0x89, 0x4F, 0x04, 0x8B, 0x4C, 0x24, 0x18, 0x89, 0x4F, 0x08,
        0x8B, 0x4C, 0x24, 0x24, 0x89, 0x4F, 0x0C, 0x8B, 0x4C, 0x24, 0x28, 0x89, 0x4F, 0x10,
        0x8B, 0x4C, 0x24, 0x2C, 0x89, 0x4F, 0x14, 0x8B, 0x0D, le32(0x0120741c), 0x89, 0x4F, 0x18,
        0x8B, 0x0D, le32(0x0122238c), 0x89, 0x4F, 0x1C, 0xFF, 0x05, le32(idxa))
      append(s, 0x60, 0xA1, le32(idxa), 0x3D, le32(CAP), 0x73, #body)
      append(s, body)
      append(s, 0x61)
      append(s, orig)
      local stub = base + off
      local jfrom = stub + #s
      append(s, 0xE9, le32(h.addr + #orig - (jfrom + 5)))
      for i, b in ipairs(s) do code[off + i - 1] = b end
      off = off + #s + (16 - (#s % 16))
      local patch = {}
      append(patch, 0xE9, le32(stub - (h.addr + 5)))
      for _ = 6, #orig do patch[#patch + 1] = 0x90 end
      write(h.addr, patch)
      out[#out + 1] = string.format("%08x:ok", h.addr)
    end
  end
  M.idx[0] = 0
  return table.concat(out, ",")
end

function M.reset() if M.idx then M.idx[0] = 0 end end

function M.dump(log, frame)
  if not M.idx then return end
  local n = M.idx[0]
  local b = M.buf
  local parts = {}
  for i = 0, n - 1 do
    local r = b + i * 8
    parts[#parts + 1] = string.format("%x:%06x:%08x:%08x:%08x:%08x:%08x:%08x", r[0], r[1] % 0x1000000, r[2], r[3], r[4],
      r[5], r[6], r[7])
    if #parts >= 64 then
      log("TR " .. frame .. " " .. table.concat(parts, " "))
      parts = {}
    end
  end
  log("TR " .. frame .. " " .. table.concat(parts, " ") .. " n=" .. n)
  M.idx[0] = 0
end

return M
