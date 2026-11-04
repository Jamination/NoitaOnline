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
-- effects (sparkles, bubbles) poured beside every player at frame 240; a creature appears
-- 170 px to the right of every player at 300 and is killed at 360 (its ragdoll stays).
-- NOITA_ONLINE_SCENE=apart (every player's view): idle players; at frame 240 every player
-- after the first is moved 1200 px further left per slot; at 600 enemies, those liquids and
-- a gravity field appear beside every player, at 1500 a bomb beside the last player; the
-- enemies' distances to their player are logged every 60 frames (`apart` lines). At 1800
-- three more enemies appear beside the last player, which leaves 3000 px at 2100 and comes
-- back (350 px right of where it stood) at 3900 (`apart home` lines: the enemies within
-- 400 px of where it stood with their distance to it, and whether that chunk is loaded).
local SCENE = os.getenv("NOITA_ONLINE_SCENE")
local SCENE_POOLS = SCENE == "pools"
local SCENE_APART = SCENE == "apart"
local SCENE_POLY = SCENE == "poly"
-- NOITA_ONLINE_SCENE=chests (with the human scenario "items"): every 300 frames from 600 a
-- physics chest is put on the spawn point (the previous one removed), so it rolls through
-- the players standing there.
local SCENE_CHESTS = SCENE == "chests"
local SCENE_ACID = SCENE == "acid"
local scene_chest = 0
local POOLS = { "acid", "magic_liquid_charm", "magic_liquid_mana_regeneration", "radioactive_liquid" }

function M.input(pi, f, px, py)
  local phase = math.floor(f / PHASE) % 8
  local t = f % PHASE
  local dir = ((pi + math.floor(f / PHASE)) % 2 == 0) and 1 or -1
  local s = { buttons = {}, count_r = 0, count_l = 0, mx = px + 60 * dir, my = py, ax = 60 * dir, ay = 0 }
  if SCENE_POOLS or SCENE_APART then return s end
  if SCENE_POLY then
    -- Until 1300 like the human session pf4_human3: everyone stands at the spawn point (where
    -- the input proxies are); the first player presses Down every 50 frames at 400-600 (the
    -- "items" cycle, see poly_events) and, as a sheep_bat (650-1250), taps like a human's:
    -- Left/Right for 3 frames every 16, Up+Fly for 5 frames every 25. From 1300: walk back and
    -- forth, fly up now and then, attack every 40 frames (as a creature: one of its AI attacks).
    if f < 1300 then
      if pi == 0 and f >= 400 and f <= 600 and f % 50 < 2 then s.buttons.Down = true end
      if pi == 0 and f >= 700 and f < 1250 then
        if f % 16 < 3 then s.buttons[math.floor(f / 160) % 2 == 0 and "Left" or "Right"] = true end
        if f % 25 < 5 then s.buttons.Up = true; s.buttons.Fly = true end
      end
      return s
    end
    if t % 200 < 120 then s.buttons[dir > 0 and "Right" or "Left"] = true end
    if t % 200 >= 140 and t % 200 < 170 then s.buttons.Up = true; s.buttons.Fly = true end
    if t % 40 < 4 then s.buttons.Fire = true; s.buttons.LeftClick = true end
    return s
  end
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
local POTION = "data/entities/items/pickup/potion_empty.xml"
local ITEMS = {
  "data/entities/items/pickup/goldnugget_50.xml",
  "data/entities/items/wand_level_01.xml",
  POTION,
  "data/entities/items/pickup/heart.xml",
}
-- The scenario's potions hold these in turn (potion.xml mostly picks magic liquids, among
-- them the polymorphines: pools of broken ones kept the players polymorphed for whole phases).
local LIQUIDS = {
  "water", "oil", "acid", "lava", "blood", "alcohol", "slime", "radioactive_liquid", "liquid_fire",
  "blood_cold", "gunpowder_unstable", "magic_liquid_berserk", "magic_liquid_charm",
  "magic_liquid_movement_faster", "magic_liquid_mana_regeneration",
}
local function load_item(file, x, y, n)
  local e = EntityLoad(file, x, y)
  if file == POTION then AddMaterialInventoryMaterial(e, LIQUIDS[n % #LIQUIDS + 1], 1000) end
  return e
end

local apart_enemies = {}
local apart_home = nil
local pool_victims = {}

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
  elseif f == 1800 or f == 2100 or f == 3900 then
    -- Leave and return: new enemies beside the last player, which then goes 3000 px away
    -- (its chunks unload, CameraBound freezes the enemies into respawn records) and comes
    -- back 350 px to the right: the chunk where it stood loads again (focus half-width
    -- 400 px) but lies off its screen, where vanilla respawns the records.
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
        EntitySetTransform(p.entity, apart_home[1] + 350, apart_home[2] - 300)
      end
    end
  end
  if apart_home and f >= 1800 and f % 60 == 0 and M.log then
    local hx, hy = apart_home[1], apart_home[2]
    local p = players[#players]
    local px, py = hx, hy
    if p and p.entity and EntityGetIsAlive(p.entity) then px, py = EntityGetTransform(p.entity) end
    local found = EntityGetInRadiusWithTag(hx, hy, 400, "enemy") or {}
    local list = {}
    for _, e in ipairs(found) do
      local ex, ey = EntityGetTransform(e)
      list[#list + 1] = string.format("%d@%.0f,%.0f/d=%.0f", e, ex, ey, math.sqrt((ex - px) ^ 2 + (ey - py) ^ 2))
    end
    M.log(string.format("apart home f=%d enemies=%d loaded=%s %s", f, #found,
      tostring(DoesWorldExistAt(hx - 8, hy - 8, hx + 8, hy + 8)), table.concat(list, " ")))
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

local function standing(p)
  return p.entity and not p.dead and EntityGetIsAlive(p.entity)
end

-- NOITA_ONLINE_SCENE=poly (polymorph checks). Until 1300 it mirrors the human session
-- pf4_human3 at the spawn point (where every client's input proxy stands): a polymorphine
-- flask is thrown at the first player's feet at 300 (lands whole); the first player's Down
-- presses at 400-600 put the "items" cycle at its feet (heart, wand, polymorphine flask,
-- chest, perk); at 650 the first player (the simulation camera's) becomes a flying sheep_bat
-- (files/poly_bat.xml, 600 frames), driven by taps of Left/Right/Up/Fly. Then everyone walks
-- and presses Fire every 40 frames: the last player becomes a sheep at 1300
-- (effect_polymorph.xml, 600 frames, then restored); a polymorphine flask breaks 24 px
-- beside the first player at 1400; the first player is polymorphed into a random creature
-- at 1900 (effect_polymorph_random.xml) and its creature is killed at 2300 (death while
-- polymorphed); the last player becomes unstable at 2000 (effect_polymorph_unstable.xml).
-- Every slot's entity is logged every 30 frames.
local function polymorphine_flask(x, y)
  local e = EntityLoad("data/entities/items/pickup/potion_empty.xml", x, y)
  AddMaterialInventoryMaterial(e, "magic_liquid_polymorph", 1000)
  return e
end

local function poly_events(f, players)
  local first, last = players[1], players[#players]
  if f == 300 and standing(first) then
    local x, y = EntityGetTransform(first.entity)
    local e = polymorphine_flask(x - 6, y - 16)
    PhysicsApplyForce(e, -20, 40)
    if M.log then M.log(string.format("poly f=%d flask %d thrown at %.1f,%.1f", f, e, x - 6, y - 16)) end
  elseif f == 650 and standing(first) then
    LoadGameEffectEntityTo(first.entity, "mods/noita_online/files/poly_bat.xml")
  elseif f == 1300 and standing(last) then
    LoadGameEffectEntityTo(last.entity, "data/entities/misc/effect_polymorph.xml")
  elseif f == 1400 and standing(first) then
    local x, y = EntityGetTransform(first.entity)
    local e = polymorphine_flask(x + 24, y - 10)
    EntityInflictDamage(e, 10, "DAMAGE_PROJECTILE", "", "NORMAL", 0, 0, 0, x + 24, y - 10, 0)
    if M.log then M.log(string.format("poly f=%d flask %d broken at %.1f,%.1f", f, e, x + 24, y - 10)) end
  elseif f == 1900 and #players > 1 and standing(first) then
    LoadGameEffectEntityTo(first.entity, "data/entities/misc/effect_polymorph_random.xml")
  elseif f == 2000 and standing(last) then
    LoadGameEffectEntityTo(last.entity, "data/entities/misc/effect_polymorph_unstable.xml")
  elseif f == 2300 and #players > 1 and standing(first) then
    local x, y = EntityGetTransform(first.entity)
    EntityInflictDamage(first.entity, 1000, "DAMAGE_CURSE", "", "NORMAL", 0, 0, 0, x, y, 0)
  end
  if f >= 400 and f <= 620 then M.items_events(f, players) end
  if f >= 240 and f % 30 == 0 and M.log then
    for i, q in ipairs(players) do
      local e = q.entity
      local alive = EntityGetIsAlive(e)
      local x, y, hp, attack, hax = 0, 0, -1, -1, false
      if alive then
        x, y = EntityGetTransform(e)
        local dm = EntityGetFirstComponentIncludingDisabled(e, "DamageModelComponent")
        if dm then hp = ComponentGetValue2(dm, "hp") end
        local c = EntityGetFirstComponentIncludingDisabled(e, "ControlsComponent")
        if c then
          hax = ComponentGetValue2(c, "polymorph_hax")
          attack = ComponentGetValue2(c, "polymorph_next_attack_frame")
        end
      end
      M.log(string.format("poly f=%d slot=%d entity=%d alive=%s file=%s pos=%.1f,%.1f hp=%.2f polymorphed=%s dead=%s hax=%s next_attack=%d",
        f, i, e, tostring(alive), alive and EntityGetFilename(e) or "-", x, y, hp,
        tostring(alive and EntityHasTag(e, "polymorphed_player")), tostring(q.dead == true),
        tostring(hax), attack))
    end
  end
end

-- Session scenario "items" (human parity sessions, `e2e.sh --human --scenario items`), driven
-- by each player's own input (part of the ticks, so identical on every client): each press
-- of Down (crouch) puts the next thing of this cycle at the player's feet: a heart (picked up
-- by touch), a wand that always casts teleport bolt, a flask of polymorphine, a chest, a perk
-- (pick up with Interact), then the polymorph effect itself; each press of Kick costs the
-- player 10 HP (while above 30 HP). With NOITA_ONLINE_SCENE=acid (respawn checks) Down pours
-- acid onto the player instead, and Kick gives it 100 gold (to lose on the respawn).
local ITEM_CYCLE = { "heart", "wand", "flask", "chest", "perk", "polymorph" }
local item_state = {}
function M.items_events(f, players)
  if SCENE_CHESTS and f >= 600 and f % 300 == 0 then
    if scene_chest ~= 0 and EntityGetIsAlive(scene_chest) then EntityKill(scene_chest) end
    scene_chest = EntityLoad("data/entities/items/pickup/chest_random.xml", 228, -86)
    if M.log then M.log(string.format("chests: chest %d at frame %d", scene_chest, f)) end
  end
  for i, p in ipairs(players) do
    local st = item_state[i] or { n = 0, down = false, kick = false }
    item_state[i] = st
    local b = (p.prev and p.prev.buttons) or {}
    local down, kick = b.Down == true, b.Kick == true
    if standing(p) then
      local x, y = EntityGetTransform(p.entity)
      if down and not st.down and SCENE_ACID then
        -- A pool of acid poured onto the player: vanilla's acid death unless it gets out.
        for k = -2, 2 do GameCreateParticle("acid", x + 6 * k, y - 24, 120, 0, 0, false, false) end
        if M.log then M.log(string.format("items: acid poured on player %d at %.1f,%.1f frame %d", i, x, y, f)) end
      elseif down and not st.down then
        local what = ITEM_CYCLE[st.n % #ITEM_CYCLE + 1]
        st.n = st.n + 1
        local e = 0
        if what == "heart" then
          e = EntityLoad("data/entities/items/pickup/heart.xml", x, y - 4)
        elseif what == "wand" then
          e = EntityLoad("data/entities/items/wand_level_01.xml", x, y - 6)
          st.wand, st.wand_frame = e, f + 2
        elseif what == "flask" then
          e = EntityLoad("data/entities/items/pickup/potion_empty.xml", x, y - 6)
          AddMaterialInventoryMaterial(e, "magic_liquid_polymorph", 1000)
        elseif what == "chest" then
          e = EntityLoad("data/entities/items/pickup/chest_random.xml", x, y - 6)
        elseif what == "perk" then
          dofile_once("data/scripts/perks/perk.lua")
          e = perk_spawn(x, y - 6, "MOVEMENT_FASTER", true) or 0
        else
          LoadGameEffectEntityTo(p.entity, "data/entities/misc/effect_polymorph.xml")
        end
        st.item = e
        if M.log then M.log(string.format("items: player %d gets %s (entity %d) at %.1f,%.1f frame %d", i, what, e, x, y, f)) end
      end
      if st.item and st.item ~= 0 and f % 30 == 0 and M.log then
        local alive = EntityGetIsAlive(st.item)
        local ix, iy = 0, 0
        if alive then ix, iy = EntityGetTransform(st.item) end
        M.log(string.format("items: f=%d player %d at %.1f,%.1f item %d alive=%s parent=%d at %.1f,%.1f", f, i, x, y, st.item,
          tostring(alive), alive and EntityGetParent(st.item) or -1, ix, iy))
        if not alive or EntityGetParent(st.item) ~= 0 then st.item = 0 end
      end
      if st.wand and f == st.wand_frame and EntityGetIsAlive(st.wand) then
        dofile_once("data/scripts/gun/procedural/gun_action_utils.lua")
        AddGunActionPermanent(st.wand, "TELEPORT_PROJECTILE")
      end
      if kick and not st.kick and SCENE_ACID then
        -- Gold to lose on the respawn (its penalty).
        local wallet = EntityGetFirstComponentIncludingDisabled(p.entity, "WalletComponent")
        if wallet then ComponentSetValue2(wallet, "money", ComponentGetValue2(wallet, "money") + 100) end
        if M.log then M.log(string.format("items: player %d gets 100 gold at frame %d", i, f)) end
      elseif kick and not st.kick then
        local dm = EntityGetFirstComponentIncludingDisabled(p.entity, "DamageModelComponent")
        if dm and ComponentGetValue2(dm, "hp") > 1.2 then
          EntityInflictDamage(p.entity, 0.4, "DAMAGE_PROJECTILE", "", "NORMAL", 0, 0, 0, x, y, 0)
          if M.log then M.log(string.format("items: player %d hurt at frame %d", i, f)) end
        end
      end
    end
    st.down, st.kick = down, kick
  end
end

-- Default scenario: the bot players never die, so every 4800-frame cycle (the 8 phases of
-- M.input) exercises movement, casts, digging with explosions, throws, kicks, pickups and
-- item switching for the whole session. Test
-- scenario only: a standing player (or the creature it is polymorphed into) gets BOT_HP max
-- HP and is healed to full whenever it drops below half (far more than one frame of damage
-- here); before the bomb phase its spells' uses are refilled (vanilla's spell refresher), and
-- before the throw phase a potion is put into its first item slot (key 5) if that is empty.
-- It also keeps its items when another player kicks it: Noita's kick makes a kicked creature
-- with an ItemPickUpperComponent throw every item it carries unless `is_immune_to_kicks`
-- (vanilla sets it only on wand ghosts; the player has it off), and every bot presses Kick
-- at the same frames of the walk/kick phase, so two bots standing together disarmed each
-- other for the rest of the session. Their kicks still push and hurt each other.
local CYCLE = 8 * PHASE
local BOT_HP = 40 -- 1000 HP (a player starts with 4)

local function sustain(e)
  local picker = EntityGetFirstComponentIncludingDisabled(e, "ItemPickUpperComponent")
  if picker and not ComponentGetValue2(picker, "is_immune_to_kicks") then
    ComponentSetValue2(picker, "is_immune_to_kicks", true)
  end
  local dm = EntityGetFirstComponentIncludingDisabled(e, "DamageModelComponent")
  if not dm then return end
  local max_hp = ComponentGetValue2(dm, "max_hp")
  if max_hp < BOT_HP then
    ComponentSetValue2(dm, "max_hp", BOT_HP)
    ComponentSetValue2(dm, "hp", BOT_HP)
  elseif ComponentGetValue2(dm, "hp") < max_hp * 0.5 then
    ComponentSetValue2(dm, "hp", max_hp)
  end
end

local function quick_inventory(e)
  for _, c in ipairs(EntityGetAllChildren(e) or {}) do
    if EntityGetName(c) == "inventory_quick" then return c end
  end
end

-- The item in quick item slot x (number key 5 + x), if any.
local function quick_item(quick, x)
  for _, c in ipairs(EntityGetAllChildren(quick) or {}) do
    local ic = EntityGetFirstComponentIncludingDisabled(c, "ItemComponent")
    if ic and not EntityHasTag(c, "wand") and ComponentGetValue2(ic, "inventory_slot") == x then return c end
  end
end

local function give_potion(f, i, e, x, y)
  local quick = quick_inventory(e)
  if not quick or quick_item(quick, 0) then return end
  local potion = load_item(POTION, x, y, math.floor(f / CYCLE) + i)
  GamePickUpInventoryItem(e, potion, false)
  local ic = EntityGetFirstComponentIncludingDisabled(potion, "ItemComponent")
  if ic then ComponentSetValue2(ic, "inventory_slot", 0, 0) end
  if M.log then
    M.log(string.format("bot f=%d slot=%d potion %d given (in quick bar: %s)", f, i, potion,
      tostring(EntityGetParent(potion) == quick)))
  end
end

-- `bot` lines (every 300 frames per standing player, and just before each throw): HP,
-- position, held item (w<slot> wand, i<slot> item), wands and items in the quick bar, limited
-- spell uses left in its wands, the shots its wands have fired and the last frames its Fire,
-- Throw, Kick and Interact buttons were pressed.
local function report(f, i, e, x, y)
  local dm = EntityGetFirstComponentIncludingDisabled(e, "DamageModelComponent")
  local inv = EntityGetFirstComponentIncludingDisabled(e, "Inventory2Component")
  local held = inv and ComponentGetValue2(inv, "mActiveItem") or 0
  local held_s = "-"
  if held ~= 0 and EntityGetIsAlive(held) then
    local ic = EntityGetFirstComponentIncludingDisabled(held, "ItemComponent")
    held_s = (EntityHasTag(held, "wand") and "w" or "i") .. (ic and ComponentGetValue2(ic, "inventory_slot") or "?")
  end
  local wands, items, uses, shots = 0, 0, 0, 0
  local quick = quick_inventory(e)
  for _, c in ipairs(quick and EntityGetAllChildren(quick) or {}) do
    if EntityHasTag(c, "wand") then
      wands = wands + 1
      local ability = EntityGetFirstComponentIncludingDisabled(c, "AbilityComponent")
      shots = shots + (ability and ComponentGetValue2(ability, "stat_times_player_has_shot") or 0)
      for _, card in ipairs(EntityGetAllChildren(c) or {}) do
        local ic = EntityGetFirstComponentIncludingDisabled(card, "ItemComponent")
        local u = ic and ComponentGetValue2(ic, "uses_remaining") or -1
        if u > 0 then uses = uses + u end
      end
    else
      items = items + 1
    end
  end
  local ctl = EntityGetFirstComponentIncludingDisabled(e, "ControlsComponent")
  local function last(b) return ctl and ComponentGetValue2(ctl, "mButtonFrame" .. b) or -1 end
  M.log(string.format(
    "bot f=%d slot=%d hp=%.0f/%.0f x=%.0f y=%.0f held=%s wands=%d items=%d uses=%d shots=%d fire=%d throw=%d kick=%d interact=%d",
    f, i, dm and ComponentGetValue2(dm, "hp") * 25 or -1, dm and ComponentGetValue2(dm, "max_hp") * 25 or -1,
    x, y, held_s, wands, items, uses, shots, last("Fire"), last("Throw"), last("Kick"), last("Interact")))
end

-- players: array of {id=, entity=} in slot order.
function M.world_events(f, players)
  if f < 120 then return end
  if SCENE_APART then return apart_events(f, players) end
  if SCENE_POLY then return poly_events(f, players) end
  if SCENE_POOLS then
    for _, p in ipairs(players) do
      if p.entity and EntityGetIsAlive(p.entity) then
        local x, y = EntityGetTransform(p.entity)
        if f == 240 then
          for j, m in ipairs(POOLS) do
            GameCreateParticle(m, x - 150 + 60 * j, y - 40, 400, 0, 0, false, false)
          end
        elseif f == 300 then
          pool_victims[#pool_victims + 1] = EntityLoad("data/entities/animals/zombie_weak.xml", open_air(x + 170, y - 10))
        end
      end
    end
    if f == 360 then
      for _, e in ipairs(pool_victims) do
        if EntityGetIsAlive(e) then
          local ex, ey = EntityGetTransform(e)
          EntityInflictDamage(e, 1000, "DAMAGE_PROJECTILE", "", "NORMAL", 0, 0, 0, ex, ey, 0)
        end
      end
    end
    return
  end
  for i, p in ipairs(players) do
    if p.entity and EntityGetIsAlive(p.entity) then
      local x, y = EntityGetTransform(p.entity)
      local k = math.floor(f / 300)
      if standing(p) then
        sustain(p.entity)
        if f % CYCLE == 2 * PHASE - 30 then GameRegenItemActionsInPlayer(p.entity) end
        if f % CYCLE == 3 * PHASE - 30 then give_potion(f, i, p.entity, x, y - 10) end
        if M.log and (f % 300 == 150 or f % CYCLE == 3 * PHASE + 50) then report(f, i, p.entity, x, y) end
      end
      if f % 900 == 450 then
        EntityLoad(SPAWNS[(k + i) % #SPAWNS + 1], x + 90 * ((i % 2 == 0) and 1 or -1), y - 20)
      end
      if f % 600 == 300 then
        load_item(ITEMS[(k + i) % #ITEMS + 1], x + 25, y - 10, k + i)
      end
      if f % CYCLE == 6 * PHASE then
        -- every cycle the last player still standing is polymorphed for the long-walk phase
        -- (a sheep for 600 frames, then restored before the wand edit of the idle phase)
        local target = nil
        for _, q in ipairs(players) do
          if standing(q) then target = q end
        end
        if target == p then LoadGameEffectEntityTo(p.entity, "data/entities/misc/effect_polymorph.xml") end
      end
      if f % 1800 == 900 then
        -- liquids pouring from above, a bounded amount (circle-of-water spells, emitting a
        -- ring of water every frame for 260 frames, piled up a sea the players floated up in)
        for j = 0, 4 do
          GameCreateParticle("water", x - 40 + j * 20, y - 60, 300, 0, 0, false, false)
        end
      end
      -- an explosive barrel and a bomb nearby
      if f % 2400 == 1200 then
        EntityLoad("data/entities/props/physics_propane_tank.xml", x + 70, y - 30)
        EntityLoad("data/entities/projectiles/bomb.xml", x + 70, y - 10)
      end
    end
  end
end

-- Session scenario "death" (`e2e.sh --scenario death`): the bot scenario (its world events and
-- inputs) plus deaths and respawns of the last player. A dead bot chooses, 180 frames after
-- its death, to respawn at the first living player (M.respawn_choice), as the respawn picker
-- does. Run 0 and every later run of the session (a new world after everyone died):
--   300  the last player gets 200 gold (the respawn costs half of it);
--   900  the last player is killed (a curse hit with an impulse up and to the right);
--   2100 the last player is polymorphed into a sheep, killed as the sheep at 2250 (it
--        respawns as the player the polymorph kept);
-- run 0 only:
--   3300 every player is killed: game over for everyone, then a new run (nextrun.lua).
-- `death` lines log the last player's gold and quick inventory before each death and after
-- each respawn, each client's game-over stats (`stats:`, cause of death; finished only on the
-- dying player's client) 5 frames after each death, and the player's ragdoll parts 5, 60,
-- 1200 and 2400 frames after each death.
local deaths = {} -- { x, y, f } of the last player's deaths

local function inventory_summary(e)
  local quick = quick_inventory(e)
  local names = {}
  for _, c in ipairs(quick and EntityGetAllChildren(quick) or {}) do
    names[#names + 1] = (EntityHasTag(c, "wand") and "wand:" or "item:") .. (EntityGetFilename(c) or "?"):match("[^/]*$")
  end
  local wallet = EntityGetFirstComponentIncludingDisabled(e, "WalletComponent")
  local dm = EntityGetFirstComponentIncludingDisabled(e, "DamageModelComponent")
  return string.format("gold=%d hp=%.0f/%.0f items=%d [%s]", wallet and ComponentGetValue2(wallet, "money") or -1,
    dm and ComponentGetValue2(dm, "hp") * 25 or -1, dm and ComponentGetValue2(dm, "max_hp") * 25 or -1, #names,
    table.concat(names, ","))
end

local function kill(p, f)
  local x, y = EntityGetTransform(p.entity)
  if M.log then M.log(string.format("death f=%d slot=%d killed (%s): %s", f, p.id, EntityGetFilename(p.entity), inventory_summary(p.entity))) end
  -- A hit with an impulse up and to the right: the ragdoll flies off with it (death_cause).
  EntityInflictDamage(p.entity, 1000, "DAMAGE_CURSE", "death scenario", "NORMAL", 300, -300, 0, x, y, 0)
end

function M.death_events(f, players, run)
  M.world_events(f, players)
  local last = players[#players]
  if not last then return end
  if f == 300 and standing(last) then
    local wallet = EntityGetFirstComponentIncludingDisabled(last.entity, "WalletComponent")
    if wallet then ComponentSetValue2(wallet, "money", 200) end
  elseif (f == 900 or f == 2250) and standing(last) then
    deaths[#deaths + 1] = { x = EntityGetTransform(last.entity), f = f }
    deaths[#deaths].y = select(2, EntityGetTransform(last.entity))
    kill(last, f)
  elseif f == 2100 and standing(last) then
    LoadGameEffectEntityTo(last.entity, "data/entities/misc/effect_polymorph.xml")
  elseif f == 3300 and run == 0 then
    for _, p in ipairs(players) do
      if standing(p) then kill(p, f) end
    end
  end
  if M.log then
    if last.respawn_frame == f then
      M.log(string.format("death f=%d slot=%d respawned as %d (%s): %s", f, last.id, last.entity, EntityGetFilename(last.entity), inventory_summary(last.entity)))
    end
    for _, d in ipairs(deaths) do
      if f == d.f + 5 then
        -- The player's ragdoll: vanilla's death makes it from the dead entity's ragdoll images
        -- (the robe's copies, mods/noita_online/robes/<n>/ragdoll/, after the first player) as
        -- data/entities/props/physics_ragdoll_part.xml entities.
        d.parts = {}
        local near = {}
        for _, e in ipairs(EntityGetInRadius(d.x, d.y, 64) or {}) do
          local file = EntityGetFilename(e) or ""
          if file:find("ragdoll") then d.parts[#d.parts + 1] = { id = e, img = file } end
          near[#near + 1] = string.format("%d:%s", e, file)
        end
        if #d.parts == 0 then
          M.log(string.format("death f=%d no ragdoll parts; entities near: %s", f, table.concat(near, " ")))
        end
        -- This client's game-over stats (the dying player's client finishes them).
        M.log(string.format("death f=%d stats: killed_by=%q killed_by_extra=%q gold=%s playtime=%s dead=%s", f,
          StatsGetValue("killed_by") or "", StatsGetValue("killed_by_extra") or "", StatsGetValue("gold") or "",
          StatsGetValue("playtime") or "", StatsGetValue("dead") or ""))
      end
      if d.parts and (f == d.f + 5 or f == d.f + 60 or f == d.f + 1200 or f == d.f + 2400) then
        local s = {}
        for _, p in ipairs(d.parts) do
          local x, y = 0, 0
          if EntityGetIsAlive(p.id) then x, y = EntityGetTransform(p.id) end
          s[#s + 1] = string.format("%d:%s:%s@%.0f,%.0f", p.id, p.img:match("[^/]*$"), EntityGetIsAlive(p.id) and "alive" or "gone", x, y)
        end
        M.log(string.format("death f=%d ragdoll of the death at f=%d (%.0f,%.0f), %d parts: %s", f, d.f, d.x, d.y, #d.parts, table.concat(s, " ")))
        if #d.parts > 0 and f == d.f + 5 then M.log("death ragdoll images: " .. d.parts[1].img) end
      end
    end
  end
end

-- The player a dead bot respawns at: the first living one, 180 frames after its death.
function M.respawn_choice(f, me, players)
  if not me or not me.death_frame or f < me.death_frame + 180 then return nil end
  for _, p in ipairs(players) do
    if p ~= me and standing(p) and not p.left then return p.id end
  end
end

return M
