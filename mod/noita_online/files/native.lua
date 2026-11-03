-- LuaJIT FFI bindings for the Noita Online client runtime (bin/noita_online.dll).
local ffi = require("ffi")

ffi.cdef([[
typedef struct {
  uint32_t seed;
  uint16_t input_delay;
  uint8_t player;
  uint8_t num_players;
  uint64_t session_id;
  uint8_t player_ids[32];
} no_start_info_t;

typedef struct { uint32_t id; uint32_t kind; float x; float y; } no_entity_pos_t;

uint32_t no_abi_version(void);
int32_t no_init(void);
int32_t no_connect(const char* addr, const char* name, const char* game_build, uint64_t mod_hash, const char* netsim);
int32_t no_status(void);
int32_t no_wait_started(uint32_t timeout_ms);
int32_t no_start_info(no_start_info_t* out);
int32_t no_send_input(uint32_t frame, const uint8_t* data, uint32_t len);
int32_t no_wait_tick(uint32_t frame, uint32_t timeout_ms, uint8_t* out, uint32_t cap);
int32_t no_report_state(uint32_t frame, uint32_t chunk_period, uint32_t has_entities, uint64_t entity_hash, const no_entity_pos_t* positions, uint32_t n);
uint32_t no_frame_mark(void);
void no_set_frame_cap(uint32_t fps);
void no_mem_stats(uint32_t* out);
int32_t no_perf_dump(const char* path);
int32_t no_poll_event(uint8_t* out, uint32_t cap);
void no_log(const char* text);
uint64_t no_rtt_us(void);
void no_shutdown(void);
int32_t no_apply_patches(void);
void no_prof_install(void);
void no_fix_seeds(uint32_t seed);
void no_set_session_time(int64_t unix_secs);
int32_t no_last_error(uint8_t* out, uint32_t cap);
int32_t no_start_params(uint8_t* out, uint32_t cap);
void no_stream_focus(int32_t x0, int32_t y0, int32_t x1, int32_t y1);
uint32_t no_evict_chunks(const int32_t* foci, uint32_t n, uint32_t frame);
void no_sim_enter(void);
void no_sim_leave(void);
uint32_t no_chunk_prewait(void);
void no_chunk_stats(uint32_t* out);
void no_focus_freeze(const float* xy, uint32_t n);
void no_local_view(int32_t i);
void no_set_sim_camera(float x, float y);
void no_gui_set_local(uint32_t entity_id);
int32_t no_gui_update(void);
int32_t no_gui_swap_update(void);
int32_t no_gui_take_ops(uint8_t* out, uint32_t cap);
int32_t no_gui_apply(uint32_t owner_id, const uint8_t* data, uint32_t len);
void no_slots_set(const uint32_t* ids, uint32_t n, int32_t local, uint32_t proxy_id);
uint32_t no_slots_get(uint32_t* out, uint32_t cap);
int32_t no_slots_install(void);
]])

local M = {}
M.ABI = 1
M.STATUS = { idle = 0, connecting = 1, lobby = 2, started = 3, disconnected = 4 }

local lib = ffi.load("mods/noita_online/bin/noita_online.dll")
if lib.no_abi_version() ~= M.ABI then
  error("noita_online.dll ABI mismatch: got " .. tostring(lib.no_abi_version()) .. ", want " .. M.ABI)
end
lib.no_init()
M.lib = lib

local err_buf = ffi.new("uint8_t[1024]")
function M.last_error()
  local n = lib.no_last_error(err_buf, 1024)
  return ffi.string(err_buf, n)
end

local event_buf = ffi.new("uint8_t[512]")
function M.poll_events()
  local out = {}
  while true do
    local n = lib.no_poll_event(event_buf, 512)
    if n <= 0 then break end
    out[#out + 1] = ffi.string(event_buf, n)
  end
  return out
end

function M.start_info()
  local info = ffi.new("no_start_info_t")
  if lib.no_start_info(info) == 0 then return nil end
  local ids = {}
  for i = 0, info.num_players - 1 do ids[#ids + 1] = info.player_ids[i] end
  local params = {}
  local n = lib.no_start_params(err_buf, 1024)
  if n > 0 then
    for k, v in ffi.string(err_buf, n):gmatch("([%w_]+)=([^;]*)") do params[k] = v end
  end
  return {
    seed = tonumber(info.seed),
    input_delay = tonumber(info.input_delay),
    player = tonumber(info.player),
    players = ids,
    session_id = info.session_id,
    params = params,
  }
end

local tick_buf = ffi.new("uint8_t[16384]")
-- Returns { [player_id] = input string } for `frame`, or nil + reason ("timeout"/"disconnected").
function M.wait_tick(frame, timeout_ms)
  local n = lib.no_wait_tick(frame, timeout_ms, tick_buf, 16384)
  if n == -1 then return nil, "timeout" end
  if n < 0 then return nil, "disconnected" end
  local count = tick_buf[0]
  local pos = 1
  local inputs = {}
  for _ = 1, count do
    local p = tick_buf[pos]
    local len = tick_buf[pos + 1] + tick_buf[pos + 2] * 256
    inputs[p] = ffi.string(tick_buf + pos + 3, len)
    pos = pos + 3 + len
  end
  return inputs
end

function M.send_input(frame, s)
  return lib.no_send_input(frame, s, #s)
end

local ops_buf = ffi.new("uint8_t[4096]")
-- Noita's inventory GUI: the simulation changes it made since the last call, as op bytes
-- (dropped, with "" returned, in the impossible case that they exceed the buffer).
function M.gui_take_ops()
  local n = lib.no_gui_take_ops(ops_buf, 4096)
  if n <= 0 then return "" end
  return ffi.string(ops_buf, n)
end

local slot_buf = ffi.new("uint32_t[32]")
-- Player slots (see crates/client/src/slots.rs): register the slot entities (slot order),
-- this client's slot index (0-based, -1: none) and its input proxy (0: none).
function M.slots_set(ids, local_index, proxy)
  local n = math.min(#ids, 32)
  for i = 1, n do slot_buf[i - 1] = ids[i] end
  lib.no_slots_set(slot_buf, n, local_index, proxy or 0)
end

-- Current entity id of every slot, in slot order (they change when a player is polymorphed
-- and back).
function M.slots_get()
  local n = lib.no_slots_get(slot_buf, 32)
  local out = {}
  for i = 1, n do out[i] = slot_buf[i - 1] end
  return out
end

local pos_buf = ffi.new("no_entity_pos_t[4096]")
-- chunk_period: 0 = hash every loaded chunk now, else only chunks with
-- index % chunk_period == frame % chunk_period. positions: array of {id=, kind=, x=, y=}.
function M.report_state(frame, chunk_period, has_entities, entity_hash, positions)
  local n = math.min(#positions, 4096)
  for i = 1, n do
    local p = positions[i]
    local e = pos_buf[i - 1]
    e.id = p.id
    e.kind = p.kind
    e.x = p.x
    e.y = p.y
  end
  return lib.no_report_state(frame, chunk_period, has_entities, entity_hash, pos_buf, n)
end

return M
