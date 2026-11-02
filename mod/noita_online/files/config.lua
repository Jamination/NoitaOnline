-- Runtime configuration: environment variables (used by the test harness) override the
-- in-game mod settings (settings.lua).
local M = {}

-- Bumped with every change that affects the simulation; clients must match exactly.
M.MOD_VERSION = "0.1.0"
-- The only Noita build whose memory layout the native runtime supports.
M.GAME_BUILD = "noita-2025-01-25-6794ee3c"

local function env(name)
  local v = os.getenv(name)
  if v == nil or v == "" then return nil end
  return v
end

local function setting(key)
  local v = ModSettingGet("noita_online." .. key)
  if v == nil or v == "" then return nil end
  return v
end

function M.load()
  local c = {}
  c.server = env("NOITA_ONLINE_SERVER") or setting("server")
  c.name = env("NOITA_ONLINE_NAME") or setting("name") or "wizard"
  c.netsim = env("NOITA_ONLINE_NETSIM")
  c.bot = env("NOITA_ONLINE_BOT") == "1"
  c.solo = env("NOITA_ONLINE_SOLO") == "1"
  c.solo_seed = tonumber(env("NOITA_ONLINE_SOLO_SEED") or "123456789")
  c.duration = tonumber(env("NOITA_ONLINE_DURATION") or "")
  c.exit_on_finish = env("NOITA_ONLINE_EXIT") == "1"
  c.debug = env("NOITA_ONLINE_DEBUG") == "1"
  -- "full": hash every loaded chunk at once on each 60-frame checkpoint (instead of the
  -- default staggered hashing, one slice of chunks per frame).
  c.full_hash = env("NOITA_ONLINE_HASH") == "full"
  -- Diagnostics: report state every frame (all entity positions) in [from, to].
  local dense = env("NOITA_ONLINE_DENSE")
  if dense then
    local a, b = dense:match("^(%d+)%-(%d+)$")
    c.dense_from, c.dense_to = tonumber(a), tonumber(b)
  end
  local root = env("NOITA_ONLINE_ROOT")
  local inst = env("NOITA_ONLINE_INSTANCE") or "0"
  local out = env("NOITA_ONLINE_OUT") or (root and (root .. "/run/logs"))
  if out then
    c.log_path = out .. "/client_" .. inst .. ".log"
    c.perf_path = out .. "/frames_" .. inst .. ".txt"
  end
  c.enabled = c.solo or c.server ~= nil
  return c
end

return M
