-- Thread activity probe (research only): enumerates the process's threads, tracks per-thread CPU time and samples
-- the instruction pointer + noita.exe return addresses on the stack of every non-main thread that consumed CPU
-- since the last sample. While a thread is suspended only GetThreadContext and raw memory reads happen
-- (no allocation, no CRT locks).
local ffi = require("ffi")
local bit = require("bit")
pcall(ffi.cdef, [[
typedef struct { uint32_t dwSize, cntUsage, th32ThreadID, th32OwnerProcessID; int32_t tpBasePri, tpDeltaPri; uint32_t dwFlags; } TP_THREADENTRY32;
void* __stdcall CreateToolhelp32Snapshot(uint32_t flags, uint32_t pid);
int __stdcall Thread32First(void* snap, TP_THREADENTRY32* te);
int __stdcall Thread32Next(void* snap, TP_THREADENTRY32* te);
int __stdcall CloseHandle(void* h);
void* __stdcall OpenThread(uint32_t access, int inherit, uint32_t tid);
uint32_t __stdcall GetCurrentProcessId(void);
uint32_t __stdcall GetCurrentThreadId(void);
int __stdcall GetThreadTimes(void* h, uint64_t* c, uint64_t* e, uint64_t* k, uint64_t* u);
uint32_t __stdcall SuspendThread(void* h);
uint32_t __stdcall ResumeThread(void* h);
int __stdcall GetThreadContext(void* h, void* ctx);
int __stdcall ReadProcessMemory(void* proc, const void* base, void* buf, size_t n, size_t* got);
void* __stdcall GetCurrentProcess(void);
int __stdcall NtQueryInformationThread(void* h, int cls, void* info, uint32_t len, uint32_t* ret);
]])
local C = ffi.C
local okn, ntdll = pcall(ffi.load, "ntdll.dll")
local M = {}
local threads = {}   -- tid -> {h=, start=, cpu=, samples=, hist={} }
local ctx = ffi.new("uint8_t[?]", 0x2d0 + 16)
local ctxp = ffi.cast("uint8_t*", bit.band(tonumber(ffi.cast("uint32_t", ctx)) + 15, bit.bnot(15)))
local stackbuf = ffi.new("uint32_t[256]")
local got = ffi.new("size_t[1]")
local tc, te, tk, tu = ffi.new("uint64_t[1]"), ffi.new("uint64_t[1]"), ffi.new("uint64_t[1]"), ffi.new("uint64_t[1]")
local main_tid
local TEXT_LO, TEXT_HI = 0x00401000, 0x00f00000

function M.refresh()
  main_tid = main_tid or C.GetCurrentThreadId()
  local snap = C.CreateToolhelp32Snapshot(4, 0)
  if snap == nil or tonumber(ffi.cast("intptr_t", snap)) == -1 then M.err = "snapshot failed" return end
  M.nseen = 0
  local e = ffi.new("TP_THREADENTRY32")
  e.dwSize = ffi.sizeof(e)
  local pid = C.GetCurrentProcessId()
  local ok = C.Thread32First(snap, e)
  while ok ~= 0 do
    if e.th32OwnerProcessID == pid then M.nseen = M.nseen + 1 end
    if e.th32OwnerProcessID == pid and e.th32ThreadID ~= main_tid and not threads[e.th32ThreadID] then
      -- THREAD_SUSPEND_RESUME|THREAD_GET_CONTEXT|THREAD_QUERY_INFORMATION
      local h = C.OpenThread(0x0002 + 0x0008 + 0x0040, 0, e.th32ThreadID)
      if h ~= nil then
        local start = ffi.new("uint32_t[1]")
        if okn then ntdll.NtQueryInformationThread(h, 9, start, 4, nil) end
        threads[e.th32ThreadID] = { h = h, start = start[0], cpu = -1, samples = 0, hist = {}, eip = {} }
      end
    end
    ok = C.Thread32Next(snap, e)
  end
  C.CloseHandle(snap)
end

local function cpu_of(t)
  if C.GetThreadTimes(t.h, tc, te, tk, tu) == 0 then return nil end
  return tonumber(tk[0] + tu[0])
end

-- sample every thread that ran since the last call
function M.sample()
  local self = C.GetCurrentProcess()
  for tid, t in pairs(threads) do
    local c = cpu_of(t)
    if c then
      if t.cpu >= 0 and c > t.cpu then
        local fl = ffi.cast("uint32_t*", ctxp)
        fl[0] = 0x10001 -- CONTEXT_CONTROL
        local nst = 0
        local eip, esp = 0, 0
        if C.SuspendThread(t.h) ~= 0xffffffff then
          if C.GetThreadContext(t.h, ctxp) ~= 0 then
            eip = fl[0xb8 / 4]
            esp = fl[0xc4 / 4]
            if C.ReadProcessMemory(self, ffi.cast("void*", esp), stackbuf, 1024, got) ~= 0 then nst = 256 end
          end
          C.ResumeThread(t.h)
        end
        t.samples = t.samples + 1
        -- key: eip if inside noita.exe, else first two noita.exe return addresses on the stack
        local key
        if eip >= TEXT_LO and eip < TEXT_HI then
          key = string.format("eip:%08x", eip)
        else
          local r = {}
          for i = 0, nst - 1 do
            local v = stackbuf[i]
            if v >= TEXT_LO and v < TEXT_HI then
              r[#r + 1] = string.format("%08x", v)
              if #r >= 3 then break end
            end
          end
          key = string.format("x%08x<%s", eip, table.concat(r, "<"))
        end
        t.hist[key] = (t.hist[key] or 0) + 1
      end
      t.cpu = c
      t.cpu0 = t.cpu0 or c
    end
  end
end

function M.report(log, top)
  top = top or 12
  local list = {}
  for tid, t in pairs(threads) do list[#list + 1] = { tid = tid, t = t } end
  log(string.format("TPSUM err=%s seen=%s tracked=%d main=%s", tostring(M.err), tostring(M.nseen), #list, tostring(main_tid)))
  table.sort(list, function(a, b) return (a.t.cpu - (a.t.cpu0 or a.t.cpu)) > (b.t.cpu - (b.t.cpu0 or b.t.cpu)) end)
  for _, x in ipairs(list) do
    local t = x.t
    local d = (t.cpu - (t.cpu0 or t.cpu)) / 10000
    do
      local h = {}
      for k, n in pairs(t.hist) do h[#h + 1] = { k, n } end
      table.sort(h, function(a, b) return a[2] > b[2] end)
      local s = {}
      for i = 1, math.min(top, #h) do s[#s + 1] = h[i][1] .. "=" .. h[i][2] end
      log(string.format("TP tid=%d start=%08x cpu_ms=%.1f samples=%d %s", x.tid, t.start, d, t.samples, table.concat(s, " ")))
    end
  end
end

return M
