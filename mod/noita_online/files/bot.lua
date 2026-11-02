-- Scripted test session.
--
-- `input(player_index, frame, px, py)` produces the input a bot player sends for `frame`
-- (each client generates its own player's input; it reaches the others only through the
-- server, exactly like human input). `world_events(frame, players)` runs on every client in
-- the deterministic simulation step when the session scenario is "bot": it spawns enemies,
-- items, liquids and explosives near the players so the session exercises every system.
local M = {}

local PHASE = 600 -- frames per phase (10 s)

local function btn(...)
  local b = {}
  for _, k in ipairs({ ... }) do b[k] = true end
  return b
end

-- Deterministic pseudo-random in [0,1) from integers (no global RNG use).
local function hash01(a, b, c)
  local h = (a * 73856093 + b * 19349663 + c * 83492791) % 2147483647
  h = (h * 48271) % 2147483647
  return h / 2147483647
end

function M.input(pi, f, px, py)
  local phase = math.floor(f / PHASE) % 8
  local t = f % PHASE
  local dir = ((pi + math.floor(f / PHASE)) % 2 == 0) and 1 or -1
  local s = { buttons = {}, count_r = 0, count_l = 0, mx = px + 60 * dir, my = py, ax = 60 * dir, ay = 0 }
  if phase == 0 then
    -- walk, turn around, levitate
    if t < 200 then s.buttons = btn(dir > 0 and "Right" or "Left")
    elseif t < 400 then s.buttons = btn(dir > 0 and "Left" or "Right")
    elseif t < 460 then s.buttons = btn("Up", "Fly", "Jump")
    end
  elseif phase == 1 then
    -- spell casts with the first wand, sweeping the aim
    local ang = (t / PHASE) * math.pi * 2 + pi
    s.ax, s.ay = math.cos(ang) * 80, math.sin(ang) * 80
    s.mx, s.my = px + s.ax, py + s.ay
    if t % 20 < 12 then s.buttons = btn("Fire", "LeftClick") end
    if t > 300 and t < 360 then s.buttons.Right = dir > 0; s.buttons.Left = dir < 0 end
  elseif phase == 2 then
    -- bomb wand (second slot): dig downwards with explosions
    if t == 0 then s.count_r = 1; s.buttons = btn("ChangeItemR") end
    s.ax, s.ay = 10 * dir, 80
    s.mx, s.my = px + s.ax, py + s.ay
    if t > 30 and t % 90 < 6 then s.buttons = btn("Fire", "LeftClick") end
    if t > 120 and t < 200 then s.buttons.Down = true end
  elseif phase == 3 then
    -- potion (third slot): throw it, splashing liquid
    if t == 0 then s.count_r = 1; s.buttons = btn("ChangeItemR") end
    s.ax, s.ay = 70 * dir, -30
    s.mx, s.my = px + s.ax, py + s.ay
    if t == 60 or t == 61 then s.buttons = btn("Throw", "RightClick") end
    if t > 120 and t < 300 then s.buttons[dir > 0 and "Right" or "Left"] = true end
  elseif phase == 4 then
    -- back to first wand; walk around, kick, pick up items
    if t == 0 then s.count_r = 1; s.buttons = btn("ChangeItemR") end
    if t > 10 and t < 250 then s.buttons[dir > 0 and "Right" or "Left"] = true end
    if t % 45 == 0 then s.buttons.Kick = true end
    if t % 30 < 3 then s.buttons.Interact = true end
  elseif phase == 5 then
    -- fight: fire at the nearest enemy direction (straight ahead), random hops
    if t % 15 < 9 then s.buttons = btn("Fire", "LeftClick") end
    if hash01(pi, f, 7) < 0.05 then s.buttons.Up = true; s.buttons.Fly = true end
    s.ay = (hash01(pi, math.floor(f / 30), 3) - 0.5) * 60
    s.my = py + s.ay
  elseif phase == 6 then
    -- long walk away from the others then back (spreads the players out)
    if t < 300 then s.buttons[dir > 0 and "Right" or "Left"] = true
    else s.buttons[dir > 0 and "Left" or "Right"] = true end
    if t % 120 < 20 then s.buttons.Up = true; s.buttons.Fly = true end
  else
    -- idle with occasional interaction (lets liquids and fire settle)
    if t % 100 < 3 then s.buttons.Interact = true end
  end
  return s
end

local SPAWNS = {
  "data/entities/animals/zombie_weak.xml",
  "data/entities/animals/rat.xml",
  "data/entities/animals/shotgunner_weak.xml",
  "data/entities/animals/slimeshooter_weak.xml",
}
local ITEMS = {
  "data/entities/items/pickup/goldnugget_50.xml",
  "data/entities/items/wand_level_01.xml",
  "data/entities/items/pickup/potion.xml",
  "data/entities/items/pickup/heart.xml",
}

-- players: array of {id=, entity=} in slot order.
function M.world_events(f, players)
  if f < 120 then return end
  for i, p in ipairs(players) do
    if p.entity and EntityGetIsAlive(p.entity) then
      local x, y = EntityGetTransform(p.entity)
      local k = math.floor(f / 300)
      if f % 900 == 450 then
        EntityLoad(SPAWNS[(k + i) % #SPAWNS + 1], x + 90 * ((i % 2 == 0) and 1 or -1), y - 20)
      end
      if f % 600 == 300 then
        EntityLoad(ITEMS[(k + i) % #ITEMS + 1], x + 25, y - 10)
      end
      if f % 1800 == 900 then
        -- liquids pouring from above and an explosive barrel nearby
        for j = 0, 4 do
          EntityLoad("data/entities/projectiles/deck/circle_water.xml", x - 40 + j * 20, y - 60)
        end
      end
      if f % 2400 == 1200 then
        EntityLoad("data/entities/props/physics_propane_tank.xml", x + 70, y - 30)
        EntityLoad("data/entities/projectiles/bomb.xml", x + 70, y - 10)
      end
    end
  end
end

return M
