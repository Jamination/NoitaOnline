-- Player input <-> bytes, and application of inputs to a player's ControlsComponent.
--
-- Wire format (little endian):
--   u32 buttons bitmask (BUTTONS order), u8 change-item-right count, u8 change-item-left
--   count, i32 mouse x * 8, i32 mouse y * 8, i32 aim x * 1024, i32 aim y * 1024,
--   u8 quick-slot select (0 = none, 1..8 = number key pressed this frame).
-- Quantising floats at capture time means every client applies bit-identical values.
local M = {}

M.BUTTONS = {
  "Fire", "Fire2", "Action", "Throw", "Interact", "Left", "Right", "Up", "Down", "Jump",
  "Run", "Fly", "Dig", "ChangeItemR", "ChangeItemL", "Inventory", "HolsterItem", "DropItem",
  "Kick", "Eat", "LeftClick", "RightClick",
}
local NB = #M.BUTTONS

local bit = require("bit")

local function u32le(v)
  v = bit.tobit(v)
  return string.char(bit.band(v, 255), bit.band(bit.rshift(v, 8), 255), bit.band(bit.rshift(v, 16), 255), bit.band(bit.rshift(v, 24), 255))
end

local function rd_u32(s, i)
  local a, b, c, d = s:byte(i, i + 3)
  return bit.bor(a, bit.lshift(b, 8), bit.lshift(c, 16), bit.lshift(d, 24))
end

local function q(v, scale)
  return math.floor(v * scale + 0.5)
end

-- state: { buttons = {Fire=true,...}, count_r=, count_l=, mx=, my=, ax=, ay=, select=,
--          cmds = { {sc=, ss=, dc=, ds=}, ... } }
-- Inventory commands (wand edits) follow the fixed part: u8 count, count x 4 bytes.
function M.encode(state)
  local mask = 0
  for i = 1, NB do
    if state.buttons[M.BUTTONS[i]] then mask = bit.bor(mask, bit.lshift(1, i - 1)) end
  end
  local s = u32le(mask) .. string.char(math.min(255, state.count_r or 0), math.min(255, state.count_l or 0))
    .. u32le(q(state.mx or 0, 8)) .. u32le(q(state.my or 0, 8))
    .. u32le(q(state.ax or 0, 1024)) .. u32le(q(state.ay or 0, 1024))
    .. string.char(state.select or 0)
  local cmds = state.cmds
  if cmds and #cmds > 0 then
    local n = math.min(#cmds, 16)
    local parts = { s, string.char(n) }
    for i = 1, n do
      local c = cmds[i]
      parts[#parts + 1] = string.char(c.sc, c.ss, c.dc, c.ds)
    end
    s = table.concat(parts)
  end
  return s
end

M.NEUTRAL = { buttons = {}, count_r = 0, count_l = 0, mx = 0, my = 0, ax = 0, ay = 0, select = 0, cmds = {} }
M.SIZE = 23

function M.decode(s)
  if s == nil or #s < M.SIZE then return M.NEUTRAL end
  local mask = rd_u32(s, 1)
  local buttons = {}
  for i = 1, NB do
    buttons[M.BUTTONS[i]] = bit.band(mask, bit.lshift(1, i - 1)) ~= 0
  end
  return {
    buttons = buttons,
    count_r = s:byte(5),
    count_l = s:byte(6),
    mx = rd_u32(s, 7) / 8,
    my = rd_u32(s, 11) / 8,
    ax = rd_u32(s, 15) / 1024,
    ay = rd_u32(s, 19) / 1024,
    select = s:byte(23),
    cmds = M.decode_cmds(s),
  }
end

function M.decode_cmds(s)
  local cmds = {}
  local n = s:byte(24) or 0
  for i = 0, n - 1 do
    local p = 25 + i * 4
    if #s < p + 3 then break end
    cmds[#cmds + 1] = { sc = s:byte(p), ss = s:byte(p + 1), dc = s:byte(p + 2), ds = s:byte(p + 3) }
  end
  return cmds
end

-- Read what Noita's own input system wrote into an *enabled* ControlsComponent (used on the
-- local input-capture proxy). Returns a state table.
function M.capture_from(controls)
  local buttons = {}
  for i = 1, NB do
    local name = M.BUTTONS[i]
    buttons[name] = ComponentGetValue2(controls, "mButtonDown" .. name) and true or false
  end
  local mx, my = ComponentGetValue2(controls, "mMousePosition")
  local ax, ay = ComponentGetValue2(controls, "mAimingVector")
  local select = 0
  for k = 1, 8 do
    if InputIsKeyJustDown(29 + k) then select = k end -- Key_1 .. Key_8 (SDL scancodes 30..37)
  end
  return {
    buttons = buttons,
    count_r = ComponentGetValue2(controls, "mButtonCountChangeItemR"),
    count_l = ComponentGetValue2(controls, "mButtonCountChangeItemL"),
    mx = mx, my = my, ax = ax, ay = ay,
    select = select,
  }
end

-- Drive a player's (disabled) ControlsComponent with `state` for simulation frame `frame`.
-- `prev` is the previously applied state for this player (for "frame pressed" stamps).
function M.apply(controls, state, prev, frame)
  for i = 1, NB do
    local name = M.BUTTONS[i]
    local down = state.buttons[name] or false
    ComponentSetValue2(controls, "mButtonDown" .. name, down)
    if down and not (prev and prev.buttons[name]) then
      ComponentSetValue2(controls, "mButtonFrame" .. name, frame)
    end
  end
  ComponentSetValue2(controls, "mButtonCountChangeItemR", state.count_r or 0)
  ComponentSetValue2(controls, "mButtonCountChangeItemL", state.count_l or 0)
  ComponentSetValue2(controls, "mMousePosition", state.mx, state.my)
  ComponentSetValue2(controls, "mAimingVector", state.ax, state.ay)
  local len = math.sqrt(state.ax * state.ax + state.ay * state.ay)
  if len > 0 then
    ComponentSetValue2(controls, "mAimingVectorNormalized", state.ax / len, state.ay / len)
    ComponentSetValue2(controls, "mAimingVectorNonZeroLatest", state.ax, state.ay)
  end
end

return M
