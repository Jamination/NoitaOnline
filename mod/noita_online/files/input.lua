-- Player input <-> bytes, and application of inputs to a player's ControlsComponent.
--
-- Wire format (little endian):
--   u32 buttons bitmask (BUTTONS order; bit 31 = aiming with a gamepad), u8 change-item-right
--   count, u8 change-item-left count, i32 mouse x * 8, i32 mouse y * 8, i32 aim x * 1024,
--   i32 aim y * 1024,
--   then, if Noita's inventory GUI changed the inventory: u16 length, GUI op bytes
--   (recorded and applied by the client runtime, see crates/client/src/invgui.rs).
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

local GAMEPAD_BIT = 0x80000000

-- state: { buttons = {Fire=true,...}, count_r=, count_l=, mx=, my=, ax=, ay=, gamepad=, ops = "" }
-- With a gamepad the aim (ax, ay) is the stick aim Noita computed; with the mouse the aim
-- follows from the mouse position (mx, my) when the input is applied.
function M.encode(state)
  local mask = state.gamepad and GAMEPAD_BIT or 0
  for i = 1, NB do
    if state.buttons[M.BUTTONS[i]] then mask = bit.bor(mask, bit.lshift(1, i - 1)) end
  end
  local s = u32le(mask) .. string.char(math.min(255, state.count_r or 0), math.min(255, state.count_l or 0))
    .. u32le(q(state.mx or 0, 8)) .. u32le(q(state.my or 0, 8))
    .. u32le(q(state.ax or 0, 1024)) .. u32le(q(state.ay or 0, 1024))
  local ops = state.ops
  if ops and #ops > 0 then
    s = s .. string.char(bit.band(#ops, 255), bit.rshift(#ops, 8)) .. ops
  end
  return s
end

M.NEUTRAL = { buttons = {}, count_r = 0, count_l = 0, mx = 0, my = 0, ax = 0, ay = 0, ops = "" }
M.SIZE = 22

function M.decode(s)
  if s == nil or #s < M.SIZE then return M.NEUTRAL end
  local mask = rd_u32(s, 1)
  local buttons = {}
  for i = 1, NB do
    buttons[M.BUTTONS[i]] = bit.band(mask, bit.lshift(1, i - 1)) ~= 0
  end
  local ops = ""
  if #s >= M.SIZE + 2 then
    local n = s:byte(M.SIZE + 1) + s:byte(M.SIZE + 2) * 256
    ops = s:sub(M.SIZE + 3, M.SIZE + 2 + n)
  end
  return {
    buttons = buttons,
    gamepad = bit.band(mask, GAMEPAD_BIT) ~= 0,
    count_r = s:byte(5),
    count_l = s:byte(6),
    mx = rd_u32(s, 7) / 8,
    my = rd_u32(s, 11) / 8,
    ax = rd_u32(s, 15) / 1024,
    ay = rd_u32(s, 19) / 1024,
    ops = ops,
  }
end

-- Read what Noita's own input system wrote into an *enabled* ControlsComponent (the local
-- input-capture proxy). Returns a state table. `stamp` is the frame number the system
-- stamped this input's presses with: a press shorter than a frame leaves mButtonDown* false
-- but stamps mButtonFrame*, so it counts as held for this frame.
function M.capture_from(controls, stamp)
  local buttons = {}
  for i = 1, NB do
    local name = M.BUTTONS[i]
    buttons[name] = ComponentGetValue2(controls, "mButtonDown" .. name)
      or ComponentGetValue2(controls, "mButtonFrame" .. name) == stamp
  end
  local mx, my = ComponentGetValue2(controls, "mMousePosition")
  local ax, ay = ComponentGetValue2(controls, "mAimingVector")
  return {
    buttons = buttons,
    count_r = ComponentGetValue2(controls, "mButtonCountChangeItemR"),
    count_l = ComponentGetValue2(controls, "mButtonCountChangeItemL"),
    mx = mx, my = my, ax = ax, ay = ay,
  }
end

-- Two consecutive frames' inputs as one input (an input delay change left one frame fewer
-- to send them in): a button pressed in either counts, and both frames' wheel steps and
-- inventory edits are kept; the pointer is the later one's.
function M.merge(a, b)
  local buttons = {}
  for k, v in pairs(a.buttons) do buttons[k] = v or nil end
  for k, v in pairs(b.buttons) do buttons[k] = buttons[k] or v or nil end
  return {
    buttons = buttons,
    count_r = (a.count_r or 0) + (b.count_r or 0),
    count_l = (a.count_l or 0) + (b.count_l or 0),
    mx = b.mx, my = b.my, ax = b.ax, ay = b.ay,
    gamepad = b.gamepad,
    ops = (a.ops or "") .. (b.ops or ""),
  }
end

-- The same input for one more frame (an input delay change left one frame more): held
-- buttons and the pointer stay, one-off events (wheel steps, inventory edits) do not repeat.
function M.hold(s)
  return { buttons = s.buttons, count_r = 0, count_l = 0, mx = s.mx, my = s.my, ax = s.ax, ay = s.ay, gamepad = s.gamepad, ops = "" }
end

-- The engine's systems run the world update that follows OnWorldPreUpdate(f) as frame f + 1:
-- that is the number they compare mButtonFrame* stamps with to detect a press made this
-- frame (item switching, throwing, kicking, interacting, ...).
function M.press_frame(frame)
  return frame + 1
end

-- What Noita's ControlsSystem (FUN_00a8d7f0) derives every frame for an *enabled*
-- ControlsComponent (vanilla's player) but leaves alone on a disabled one like a slot's;
-- recomputed here from the applied input and the player's own state.
local AIM_FULL_LENGTH = 100 -- aim vectors are normalised against 100 px (DAT_011524ec)
local FLY_TARGET_ABOVE = 10 -- mFlyingTargetY = y - 10 (DAT_01053ccc): the fly model's target
-- Presses that go through a delay line of `input_latency_frames` frames (a LensValue some
-- game effects raise; 0 normally). Fly is the Up key's flag.
local DELAYED = { Fire = true, Fire2 = true, Left = true, Right = true, Up = true, Down = true, Kick = true, Throw = true, Fly = true }
local delay_lines = {} -- [controls component] = { [button] = press history, newest in bit 0 }
-- Buttons the ControlsSystem never writes for its player (no key binding): left as they are,
-- e.g. Run, which the engine holds for a player polymorphed into a creature.
local NOT_FROM_INPUT = { Action = true, Jump = true, Run = true, Dig = true, HolsterItem = true, Eat = true }

-- The aim origin: the centre of the player's first enabled hitbox.
local function aim_origin(entity)
  local x, y, _, sx, sy = EntityGetTransform(entity)
  local hb = EntityGetFirstComponent(entity, "HitboxComponent")
  if not hb then return x, y end
  local ox, oy = ComponentGetValue2(hb, "offset")
  return (ComponentGetValue2(hb, "aabb_min_x") + ComponentGetValue2(hb, "aabb_max_x")) * 0.5 + ox * sx + x,
    (ComponentGetValue2(hb, "aabb_min_y") + ComponentGetValue2(hb, "aabb_max_y")) * 0.5 + y + oy * sy
end

-- Drive player `entity`'s (disabled) ControlsComponent with `state` for simulation frame
-- `frame` (GameGetFrameNum() in OnWorldPreUpdate). "Just pressed" stamps compare with the
-- component's previous state, as in the engine.
function M.apply(controls, state, frame, entity)
  local latency = ComponentGetValue2(controls, "input_latency_frames") or 0
  local lines = delay_lines[controls]
  if not lines then
    lines = {}
    delay_lines[controls] = lines
  end
  local stamp = M.press_frame(frame)
  for i = 1, NB do
    local name = M.BUTTONS[i]
    if not NOT_FROM_INPUT[name] then
      local down = state.buttons[name] or false
      if DELAYED[name] then
        local line = ((lines[name] or 0) * 2 + (down and 1 or 0)) % 4294967296
        lines[name] = line
        down = math.floor(line / 2 ^ math.min(latency, 31)) % 2 == 1
      end
      if down and not ComponentGetValue2(controls, "mButtonDown" .. name) then
        ComponentSetValue2(controls, "mButtonFrame" .. name, stamp)
      end
      ComponentSetValue2(controls, "mButtonDown" .. name, down)
    end
  end
  if ComponentGetValue2(controls, "mButtonDownFire") then
    ComponentSetValue2(controls, "mButtonLastFrameFire", stamp)
  end
  ComponentSetValue2(controls, "mButtonCountChangeItemR", state.count_r or 0)
  ComponentSetValue2(controls, "mButtonCountChangeItemL", state.count_l or 0)
  ComponentSetValue2(controls, "mMousePosition", state.mx, state.my)
  local _, y = EntityGetTransform(entity)
  ComponentSetValue2(controls, "mFlyingTargetY", y - FLY_TARGET_ABOVE)
  local ax, ay = state.ax, state.ay
  if not state.gamepad then
    local ox, oy = aim_origin(entity)
    ax, ay = state.mx - ox, state.my - oy
  end
  ComponentSetValue2(controls, "mAimingVector", ax, ay)
  local len = math.sqrt(ax * ax + ay * ay)
  local nx, ny = ax, ay
  if len > AIM_FULL_LENGTH then nx, ny = ax / len * AIM_FULL_LENGTH, ay / len * AIM_FULL_LENGTH end
  nx, ny = nx / AIM_FULL_LENGTH, ny / AIM_FULL_LENGTH
  ComponentSetValue2(controls, "mAimingVectorNormalized", nx, ny)
  if state.gamepad and len > 0 then ComponentSetValue2(controls, "mAimingVectorNonZeroLatest", nx, ny) end
end

return M
