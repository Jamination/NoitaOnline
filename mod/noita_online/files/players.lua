-- Player entities in a lockstep session.
--
-- Every client holds the same set of player entities, one per session slot, created at the
-- same frame in the same order, so entity ids and all simulation state match. Each slot is
-- driven only by the inputs in the server's ticks (ControlsComponent disabled). The engine's
-- own spawned player ("proxy") is made inert on every client and only serves as the sink for
-- this client's real keyboard/mouse/gamepad input, which we read and send to the server.
local Input = dofile_once("mods/noita_online/files/input.lua")

local M = {}

M.slots = {}          -- array of { id = player_id, entity = eid, controls = cid, prev = state }
M.by_id = {}
M.proxy = nil
M.proxy_controls = nil
M.local_id = nil

local KEEP_ON_PROXY = { ControlsComponent = true, PlatformShooterPlayerComponent = true }

local function make_proxy_inert(e)
  for _, c in ipairs(EntityGetAllComponents(e) or {}) do
    local name = ComponentGetTypeName(c)
    if not KEEP_ON_PROXY[name] then
      EntitySetComponentIsEnabled(e, c, false)
    end
  end
  local shooter = EntityGetFirstComponentIncludingDisabled(e, "PlatformShooterPlayerComponent")
  if shooter then ComponentSetValue2(shooter, "center_camera_on_this_entity", false) end
  for _, tag in ipairs({ "mortal", "human", "hittable", "peasant", "prey", "player_unit", "teleportable" }) do
    EntityRemoveTag(e, tag)
  end
  EntityAddTag(e, "noita_online_proxy")
  -- Drop the proxy's own starting items so its inventory cannot act.
  for _, child in ipairs(EntityGetAllChildren(e) or {}) do
    for _, item in ipairs(EntityGetAllChildren(child) or {}) do
      EntityRemoveFromParent(item)
      EntityKill(item)
    end
  end
end

-- Called from OnPlayerSpawned (same frame on every client).
function M.setup(proxy, ids, local_id)
  M.proxy = proxy
  M.local_id = local_id
  local x, y = EntityGetTransform(proxy)
  make_proxy_inert(proxy)
  M.proxy_controls = EntityGetFirstComponentIncludingDisabled(proxy, "ControlsComponent")
  for _, id in ipairs(ids) do
    local e = EntityLoad("data/entities/player.xml", x, y)
    EntityAddTag(e, "noita_online_player")
    local controls = EntityGetFirstComponentIncludingDisabled(e, "ControlsComponent")
    ComponentSetValue2(controls, "enabled", false)
    -- Players never die by themselves (which would end this client's run); the lockstep
    -- step turns them into spectators instead, identically everywhere.
    local dm = EntityGetFirstComponentIncludingDisabled(e, "DamageModelComponent")
    if dm then ComponentSetValue2(dm, "wait_for_kill_flag_on_death", true) end
    local slot = { id = id, entity = e, controls = controls, prev = Input.NEUTRAL }
    M.slots[#M.slots + 1] = slot
    M.by_id[id] = slot
    -- Camera and HUD follow only this client's own slot (presentation only).
    local is_local = id == local_id
    local shooter = EntityGetFirstComponentIncludingDisabled(e, "PlatformShooterPlayerComponent")
    if shooter then ComponentSetValue2(shooter, "center_camera_on_this_entity", is_local) end
    local gui = EntityGetFirstComponentIncludingDisabled(e, "InventoryGuiComponent")
    if gui then EntitySetComponentIsEnabled(e, gui, is_local) end
  end
end

-- Solo baseline (no networking): the engine's own player is slot 0, driven by applied
-- inputs exactly like a lockstep slot.
function M.setup_solo(player)
  M.local_id = 0
  local controls = EntityGetFirstComponentIncludingDisabled(player, "ControlsComponent")
  ComponentSetValue2(controls, "enabled", false)
  EntityAddTag(player, "noita_online_player")
  local slot = { id = 0, entity = player, controls = controls, prev = Input.NEUTRAL }
  M.slots = { slot }
  M.by_id = { [0] = slot }
end

function M.local_slot()
  return M.by_id[M.local_id]
end

-- Real input of this client, as captured by the engine into the proxy's ControlsComponent.
function M.capture_local()
  if not M.proxy_controls then return Input.NEUTRAL end
  local s = Input.capture_from(M.proxy_controls)
  -- Aim is relative to our simulated player, not to the inert proxy.
  local slot = M.local_slot()
  if slot and EntityGetIsAlive(slot.entity) then
    local px, py = EntityGetTransform(slot.entity)
    s.ax, s.ay = s.mx - px, s.my - py
  end
  return s
end

-- Quick-inventory items of `e` in slot order: { {slot=n, entity=item}, ... }.
local function quick_items(e)
  local out = {}
  for _, child in ipairs(EntityGetAllChildren(e) or {}) do
    if EntityGetName(child) == "inventory_quick" then
      for _, item in ipairs(EntityGetAllChildren(child) or {}) do
        local ic = EntityGetFirstComponentIncludingDisabled(item, "ItemComponent")
        if ic then
          local sx = ComponentGetValue2(ic, "inventory_slot")
          local is_wand = EntityHasTag(item, "wand")
          -- Wands occupy quick slots 1-4, other items 5-8.
          out[#out + 1] = { slot = sx + (is_wand and 0 or 4), entity = item }
        end
      end
    end
  end
  table.sort(out, function(a, b) return a.slot < b.slot end)
  return out
end

-- Number-key selection, expressed as the equivalent change-item step count so the
-- engine's own item switching runs (identically on every client).
local function select_quick_slot(slot, k)
  local inv = EntityGetFirstComponentIncludingDisabled(slot.entity, "Inventory2Component")
  if not inv then return end
  local items = quick_items(slot.entity)
  local active = ComponentGetValue2(inv, "mActiveItem")
  local from, to = nil, nil
  for i, it in ipairs(items) do
    if it.entity == active then from = i end
    if it.slot == k - 1 then to = i end
  end
  if not from or not to or from == to then return end
  local steps = (to - from) % #items
  ComponentSetValue2(slot.controls, "mButtonDownChangeItemR", true)
  ComponentSetValue2(slot.controls, "mButtonFrameChangeItemR", GameGetFrameNum())
  ComponentSetValue2(slot.controls, "mButtonCountChangeItemR", steps)
end

-- A player whose HP reached zero becomes an invulnerable, input-less spectator.
local function check_death(slot)
  if slot.dead then return end
  local dm = EntityGetFirstComponentIncludingDisabled(slot.entity, "DamageModelComponent")
  if not dm or ComponentGetValue2(dm, "hp") > 0 then return end
  slot.dead = true
  ComponentSetValue2(dm, "hp", 0.04)
  ComponentSetValue2(dm, "invincibility_frames", 2000000000)
  EntityRemoveTag(slot.entity, "player_unit")
  EntityRemoveTag(slot.entity, "hittable")
  EntityRemoveTag(slot.entity, "mortal")
  EntityAddTag(slot.entity, "noita_online_dead")
  if slot.id == M.local_id then GamePrint("You died - spectating") end
end

-- Apply one tick: { [player_id] = input bytes }. Slots absent from the tick have left.
function M.apply_tick(frame, tick)
  for _, slot in ipairs(M.slots) do
    local bytes = tick[slot.id]
    if EntityGetIsAlive(slot.entity) then check_death(slot) end
    if bytes == nil or slot.dead then
      if not slot.frozen then
        slot.frozen = true
        -- Departed or dead player: neutral input from now on, identically on every client.
        if EntityGetIsAlive(slot.entity) then
          Input.apply(slot.controls, Input.NEUTRAL, slot.prev, frame)
          slot.prev = Input.NEUTRAL
        end
      end
    elseif EntityGetIsAlive(slot.entity) then
      local state = Input.decode(bytes)
      Input.apply(slot.controls, state, slot.prev, frame)
      if state.select and state.select > 0 then select_quick_slot(slot, state.select) end
      slot.prev = state
    end
  end
end

return M
