-- Input-latency probe (NOITA_ONLINE_LATPROBE=1, diagnostics only): logs, per frame, every
-- stage a key press goes through, so the frames between the engine's input poll and the
-- character's reaction can be counted (harness/latency.py). Frames are numbered like
-- GameGetFrameNum() in OnWorldPreUpdate; the world update that follows it is "update f".
--   pre(f):   at OnWorldPreUpdate, the polled keyboard/mouse state (the engine polls its
--             input before the world update; the Lua Input* API reads that state).
--   sent(...):lockstep only: what this client captured and the tick it was sent for.
--   post(e):  at OnWorldPostUpdate, the controlled entity's ControlsComponent (Right, Fire),
--             its horizontal velocity, its held wand's last cast frame and its active item.
-- Watched inputs: key D (walk right, SDL scancode 7), the left mouse button (Fire) and the
-- number keys 1 and 2 (quick-bar item, handled by Noita's inventory GUI; scancodes 30, 31).
local M = {}

M.on = os.getenv("NOITA_ONLINE_LATPROBE") == "1"
local KEY_D = 7
local KEY_1, KEY_2 = 30, 31
local MOUSE_LEFT = 1

local logf = nil
local cur = nil
local last_line = nil

function M.open(path)
  if path then logf = io.open(path, "w") end
end

local function b(v) return v and 1 or 0 end

function M.pre(f)
  if not logf then return end
  cur = {
    f = f,
    t = GameGetRealWorldTimeSinceStarted(),
    pd = b(InputIsKeyDown(KEY_D) or InputIsKeyJustDown(KEY_D)),
    pm = b(InputIsMouseButtonDown(MOUSE_LEFT) or InputIsMouseButtonJustDown(MOUSE_LEFT)),
    pk = b(InputIsKeyDown(KEY_1) or InputIsKeyJustDown(KEY_1) or InputIsKeyDown(KEY_2) or InputIsKeyJustDown(KEY_2)),
    sd = 0, sm = 0, tick = -1,
  }
end

-- Lockstep: the captured input state `s` was sent for tick `target`; `render_mouse`: the
-- pointer under the render camera after the last update ({x, y}), for comparison.
function M.sent(target, s, render_mouse)
  if not cur then return end
  cur.sd = b(s.buttons and s.buttons.Right)
  cur.sm = b(s.buttons and s.buttons.Fire)
  cur.tick = target
  cur.mouse = string.format(" mouse=%.2f,%.2f", s.mx or 0, s.my or 0)
  if render_mouse then cur.mouse = cur.mouse .. string.format(" rmouse=%.2f,%.2f", render_mouse[1], render_mouse[2]) end
end

local function active_item(e)
  local inv = EntityGetFirstComponentIncludingDisabled(e, "Inventory2Component")
  return inv and ComponentGetValue2(inv, "mActiveItem") or 0
end

local function cast_frame(item)
  local ab = item ~= 0 and EntityGetFirstComponentIncludingDisabled(item, "AbilityComponent")
  return ab and ComponentGetValue2(ab, "mCastDelayStartFrame") or -1
end

-- After the world update: the state of `entity`, the character this client controls.
function M.post(entity)
  if not cur or not logf then return end
  local c = cur
  cur = nil
  local cd, cm, vx, cast, item = 0, 0, 0, -1, 0
  if entity and EntityGetIsAlive(entity) then
    local ctl = EntityGetFirstComponentIncludingDisabled(entity, "ControlsComponent")
    if ctl then
      cd = b(ComponentGetValue2(ctl, "mButtonDownRight"))
      cm = b(ComponentGetValue2(ctl, "mButtonDownFire"))
    end
    local cdc = EntityGetFirstComponentIncludingDisabled(entity, "CharacterDataComponent")
    if cdc then vx = ComponentGetValue2(cdc, "mVelocity") end
    item = active_item(entity)
    cast = cast_frame(item)
  end
  -- One line per frame in which anything is active or changed (the timestamps of idle
  -- frames are not needed).
  local state = string.format("pd=%d pm=%d pk=%d sd=%d sm=%d cd=%d cm=%d vx=%.2f cast=%d item=%d",
    c.pd, c.pm, c.pk, c.sd, c.sm, cd, cm, vx, cast, item)
  local active = c.pd + c.pm + c.pk + c.sd + c.sm + cd + cm > 0 or math.abs(vx) > 0.01
  if active or state ~= last_line then
    logf:write(string.format("lat f=%d t=%.4f tick=%d %s%s\n", c.f, c.t, c.tick, state, c.mouse or ""))
    logf:flush()
  end
  last_line = state
end

return M
