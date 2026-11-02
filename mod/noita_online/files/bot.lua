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

-- NOITA_ONLINE_SCENE=pools (visual parity checks): liquids whose materials have particle
-- effects (sparkles, bubbles) poured beside every player at frame 240.
-- NOITA_ONLINE_SCENE=apart (every player's view): idle players; at frame 240 every player
-- after the first is moved 1200 px further left per slot; at 600 enemies, those liquids and
-- a gravity field appear beside every player, at 1500 a bomb beside the last player; the
-- enemies' distances to their player are logged every 60 frames (`apart` lines). At 1800
-- three more enemies appear beside the last player, which leaves 3000 px at 2100 and comes
-- back (500 px right of where it stood) at 3900 (`apart home` lines: enemies within 400 px
-- of where it stood, and whether that chunk is loaded).
local SCENE = os.getenv("NOITA_ONLINE_SCENE")
local SCENE_POOLS = SCENE == "pools"
local SCENE_APART = SCENE == "apart"
local POOLS = { "acid", "magic_liquid_charm", "magic_liquid_mana_regeneration", "radioactive_liquid" }
-- TEMP (ParityVisual3 repro): NOITA_ONLINE_APART_RETURN=frame returns to the home spot itself
local APART_RETURN = tonumber(os.getenv("NOITA_ONLINE_APART_RETURN") or "")

function M.input(pi, f, px, py)
  local phase = math.floor(f / PHASE) % 8
  local t = f % PHASE
  local dir = ((pi + math.floor(f / PHASE)) % 2 == 0) and 1 or -1
  local s = { buttons = {}, count_r = 0, count_l = 0, mx = px + 60 * dir, my = py, ax = 60 * dir, ay = 0 }
  if SCENE_POOLS or SCENE_APART then return s end
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
    -- bomb wand (second slot), selected like the mouse wheel does: dig downwards with explosions
    if t == 0 then s.count_r = 1; s.buttons = btn("ChangeItemR") end
    s.ax, s.ay = 10 * dir, 80
    s.mx, s.my = px + s.ax, py + s.ay
    if t > 30 and t % 90 < 6 then s.buttons = btn("Fire", "LeftClick") end
    if t > 120 and t < 200 then s.buttons.Down = true end
  elseif phase == 3 then
    -- potion (first item slot), selected with number key 5: throw it, splashing liquid
    if t == 0 then s.select = 5 end
    s.ax, s.ay = 70 * dir, -30
    s.mx, s.my = px + s.ax, py + s.ay
    if t == 60 or t == 61 then s.buttons = btn("Throw", "RightClick") end
    if t > 120 and t < 300 then s.buttons[dir > 0 and "Right" or "Left"] = true end
  elseif phase == 4 then
    -- back to the first wand (number key 1); walk around, kick, pick up items
    if t == 0 then s.select = 1 end
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
    -- idle with occasional interaction (lets liquids and fire settle); edit the first wand
    -- (drag its first spell onto the second, as in the inventory screen)
    if t % 100 < 3 then s.buttons.Interact = true end
    if t == 50 then s.swap = { wand = 1, a = 0, b = 1 } end
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

local apart_enemies = {}
local apart_home = nil

-- The first point at or above (x, y) (10 px steps) with 24 px of open air above it, so a
-- creature spawned there is not stuck in the terrain.
local function open_air(x, y)
  for _ = 1, 40 do
    if not RaytraceSurfaces(x, y, x, y - 24) then return x, y end
    y = y - 10
  end
  return x, y
end

local function apart_events(f, players)
  if f == 240 then
    for i, p in ipairs(players) do
      if i > 1 and p.entity and EntityGetIsAlive(p.entity) then
        local x, y = EntityGetTransform(p.entity)
        EntitySetTransform(p.entity, x - 1200 * (i - 1), y - 300)
      end
    end
  elseif f == 600 then
    for i, p in ipairs(players) do
      if p.entity and EntityGetIsAlive(p.entity) then
        local x, y = EntityGetTransform(p.entity)
        apart_enemies[#apart_enemies + 1] = { i, EntityLoad("data/entities/animals/zombie_weak.xml", open_air(x - 170, y - 10)) }
        apart_enemies[#apart_enemies + 1] = { i, EntityLoad("data/entities/animals/shotgunner_weak.xml", open_air(x + 170, y - 10)) }
        for j, m in ipairs(POOLS) do
          GameCreateParticle(m, x - 120 + 40 * j, y - 60, 300, 0, 0, false, false)
        end
        -- runs data/scripts/buildings/gravity_field.lua (is_in_camera_bounds)
        EntityLoad("data/entities/buildings/biome_modifiers/gravity_field.xml", x + 90, y - 60)
      end
    end
  elseif f == 1500 then
    local p = players[#players]
    if p and p.entity and EntityGetIsAlive(p.entity) then
      local x, y = EntityGetTransform(p.entity)
      EntityLoad("data/entities/projectiles/bomb.xml", x + 60, y - 10)
    end
  elseif f == 1800 or f == 2100 or f == (APART_RETURN or 3900) then
    -- Leave and return: new enemies beside the last player, which then goes 3000 px away
    -- (its chunks unload, CameraBound freezes the enemies into respawn records) and comes
    -- back 500 px to the right (records respawn off screen only, as in vanilla).
    local p = players[#players]
    if p and p.entity and EntityGetIsAlive(p.entity) then
      local x, y = EntityGetTransform(p.entity)
      if f == 1800 then
        apart_home = { x, y }
        for k = 1, 3 do
          EntityLoad("data/entities/animals/zombie_weak.xml", open_air(x - 150 + 100 * k, y - 10))
        end
      elseif f == 2100 then
        EntitySetTransform(p.entity, x - 3000, y - 300)
      else
        if APART_RETURN then EntitySetTransform(p.entity, apart_home[1], apart_home[2] - 20)
        else EntitySetTransform(p.entity, apart_home[1] + 500, apart_home[2] - 300) end
      end
    end
  end
  if apart_home and f >= 1800 and f % 60 == 0 and M.log then
    local hx, hy = apart_home[1], apart_home[2]
    local n = #(EntityGetInRadiusWithTag(hx, hy, 400, "enemy") or {})
    M.log(string.format("apart home f=%d enemies=%d loaded=%s", f, n, tostring(DoesWorldExistAt(hx - 8, hy - 8, hx + 8, hy + 8))))
  end
  if f >= 600 and f % 60 == 0 and M.log then
    local parts = {}
    for _, e in ipairs(apart_enemies) do
      local p, id = players[e[1]], e[2]
      if EntityGetIsAlive(id) and p and p.entity then
        local ex, ey = EntityGetTransform(id)
        local px, py = EntityGetTransform(p.entity)
        local ai = EntityGetFirstComponentIncludingDisabled(id, "AnimalAIComponent")
        parts[#parts + 1] = string.format("p%d:%d d=%.0f ai=%s", e[1], id, math.sqrt((ex - px) ^ 2 + (ey - py) ^ 2),
          ai and tostring(ComponentGetValue2(ai, "ai_state")) or "-")
      else
        parts[#parts + 1] = string.format("p%d:%d dead", e[1], id)
      end
    end
    local where = {}
    for i, p in ipairs(players) do
      if p.entity and EntityGetIsAlive(p.entity) then
        local x, y = EntityGetTransform(p.entity)
        where[#where + 1] = string.format("p%d=%.0f,%.0f", i, x, y)
      end
    end
    M.log(string.format("apart f=%d %s | %s", f, table.concat(where, " "), table.concat(parts, " ")))
  end
end

-- players: array of {id=, entity=} in slot order.
function M.world_events(f, players)
  if f < 120 then return end
  if SCENE_APART then return apart_events(f, players) end
  if SCENE_POOLS then
    if f ~= 240 then return end
    for _, p in ipairs(players) do
      if p.entity and EntityGetIsAlive(p.entity) then
        local x, y = EntityGetTransform(p.entity)
        for j, m in ipairs(POOLS) do
          GameCreateParticle(m, x - 150 + 60 * j, y - 40, 400, 0, 0, false, false)
        end
      end
    end
    return
  end
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
      if f == 1500 and i == #players then
        -- the last player is polymorphed (a sheep for 600 frames, then restored)
        LoadGameEffectEntityTo(p.entity, "data/entities/misc/effect_polymorph.xml")
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
