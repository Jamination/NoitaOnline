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

-- Apply one tick: { [player_id] = input bytes }. Slots absent from the tick have left.
function M.apply_tick(frame, tick)
  for _, slot in ipairs(M.slots) do
    local bytes = tick[slot.id]
    if bytes == nil then
      if not slot.left then
        slot.left = true
        -- Departed player: freeze it in place, identically on every client.
        if EntityGetIsAlive(slot.entity) then
          Input.apply(slot.controls, Input.NEUTRAL, slot.prev, frame)
        end
      end
    elseif EntityGetIsAlive(slot.entity) then
      local state = Input.decode(bytes)
      Input.apply(slot.controls, state, slot.prev, frame)
      slot.prev = state
    end
  end
end

return M
