-- Player entities in a lockstep session.
--
-- Every client holds the same set of player entities, one per session slot, created at the
-- same frame in the same order, so entity ids and all simulation state match. Each slot is
-- driven only by the inputs in the server's ticks (ControlsComponent disabled). The engine's
-- own spawned player ("proxy") is made inert on every client and only serves as the sink for
-- this client's real keyboard/mouse/gamepad input, which we read and send to the server.
local Input = dofile_once("mods/noita_online/files/input.lua")
local Inventory = dofile_once("mods/noita_online/files/inventory.lua")

local M = {}

M.slots = {}          -- array of { id = player_id, entity = eid, controls = cid, prev = state }
M.by_id = {}
M.proxy = nil
M.proxy_controls = nil
M.local_id = nil

-- PlatformShooterPlayerComponent stays: it is part of how Noita turns raw keyboard/mouse
-- state into the ControlsComponent fields we capture (camera centring is turned off below).
local KEEP_ON_PROXY = { ControlsComponent = true, PlatformShooterPlayerComponent = true }
-- Diagnostics: NOITA_ONLINE_PROXY_NOSHOOTER=1 also disables the proxy's PlatformShooterPlayer.
if os.getenv("NOITA_ONLINE_PROXY_NOSHOOTER") == "1" then KEEP_ON_PROXY.PlatformShooterPlayerComponent = nil end

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
  -- Remove everything hanging off the proxy: its starting items, and the arm/cape/emitter
  -- child entities, which follow this client's own aim (local input) and could otherwise
  -- touch the shared simulation. Done identically on every client.
  for _, child in ipairs(EntityGetAllChildren(e) or {}) do
    local name = EntityGetName(child)
    if name == "inventory_quick" or name == "inventory_full" then
      for _, item in ipairs(EntityGetAllChildren(child) or {}) do
        EntityRemoveFromParent(item)
        EntityKill(item)
      end
    else
      EntityRemoveFromParent(child)
      EntityKill(child)
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
    -- The camera follows only this client's own slot (presentation only).
    local shooter = EntityGetFirstComponentIncludingDisabled(e, "PlatformShooterPlayerComponent")
    if shooter then ComponentSetValue2(shooter, "center_camera_on_this_entity", id == local_id) end
    -- Noita's HUD (InventoryGuiComponent) runs on no slot, not even our own: besides drawing,
    -- its system edits the inventory from this client's raw input (number keys equip items
    -- at once; its slot widgets also move and eat items) and assigns inventory slots to items,
    -- so enabled on one client only it changes the simulation on that client alone. The mod
    -- draws the HUD instead (files/inventory.lua) and assigns slots (normalize_inventory).
    local gui = EntityGetFirstComponentIncludingDisabled(e, "InventoryGuiComponent")
    if gui then EntitySetComponentIsEnabled(e, gui, false) end
  end
end

-- Solo baseline (no networking): the engine's own player is slot 0, driven by applied
-- inputs exactly like a lockstep slot, and like a lockstep slot it never dies by itself
-- (apply_tick turns it into a spectator), so the scenario's world events run as in a session.
function M.setup_solo(player)
  M.local_id = 0
  local controls = EntityGetFirstComponentIncludingDisabled(player, "ControlsComponent")
  ComponentSetValue2(controls, "enabled", false)
  EntityAddTag(player, "noita_online_player")
  local dm = EntityGetFirstComponentIncludingDisabled(player, "DamageModelComponent")
  if dm then ComponentSetValue2(dm, "wait_for_kill_flag_on_death", true) end
  local slot = { id = 0, entity = player, controls = controls, prev = Input.NEUTRAL }
  M.slots = { slot }
  M.by_id = { [0] = slot }
end

function M.local_slot()
  return M.by_id[M.local_id]
end

-- Real input of this client, as captured by the engine into the proxy's ControlsComponent.
-- `mouse` ({x, y}, optional): the pointer in world coordinates under this client's render
-- camera. The proxy's mMousePosition is converted with the shared simulation camera, which
-- is another player's view on every client but the first.
function M.capture_local(mouse)
  if not M.proxy_controls then return Input.NEUTRAL end
  local s = Input.capture_from(M.proxy_controls)
  if mouse then s.mx, s.my = mouse[1], mouse[2] end
  -- Aim is relative to our simulated player, not to the inert proxy.
  local slot = M.local_slot()
  if slot and EntityGetIsAlive(slot.entity) then
    local px, py = EntityGetTransform(slot.entity)
    s.ax, s.ay = s.mx - px, s.my - py
  end
  -- The Inventory key opens our lockstep-safe editor locally; Noita's own inventory screen
  -- is never opened for player slots (it would edit entities on this client only).
  if s.buttons.Inventory and not M.inventory_key_down then Inventory.toggle() end
  M.inventory_key_down = s.buttons.Inventory
  s.buttons.Inventory = false
  if Inventory.open then
    -- Mouse buttons operate the editor while it is open (vanilla pauses the game there).
    for _, b in ipairs({ "Fire", "Fire2", "LeftClick", "RightClick", "Throw" }) do s.buttons[b] = false end
  end
  s.cmds = Inventory.take_pending()
  return s
end

local function child_named(e, name)
  for _, c in ipairs(EntityGetAllChildren(e) or {}) do
    if EntityGetName(c) == name then return c end
  end
end

-- Give every item of `items` ({ {e=, ic=}, ... } in child order) its own slot x in [0, cap):
-- an item keeps a valid slot no earlier item holds, the rest take the lowest free slots in
-- order (past `cap` only if the container is over-full).
local function assign_slots(items, cap)
  local used, todo = {}, {}
  for _, it in ipairs(items) do
    local x = ComponentGetValue2(it.ic, "inventory_slot")
    if x >= 0 and x < cap and not used[x] then used[x] = true else todo[#todo + 1] = it end
  end
  local x = 0
  for _, it in ipairs(todo) do
    while used[x] do x = x + 1 end
    used[x] = true
    ComponentSetValue2(it.ic, "inventory_slot", x, 0)
  end
end

local function items_of(container, keep)
  local out = {}
  for _, e in ipairs(EntityGetAllChildren(container) or {}) do
    local ic = EntityGetFirstComponentIncludingDisabled(e, "ItemComponent")
    if ic and keep(e, ic) then out[#out + 1] = { e = e, ic = ic } end
  end
  return out
end

local function is_wand(e) return EntityHasTag(e, "wand") end
local function not_wand(e) return not EntityHasTag(e, "wand") end
local function is_card(e, ic)
  return EntityGetFirstComponentIncludingDisabled(e, "ItemActionComponent") ~= nil
    and not ComponentGetValue2(ic, "permanently_attached")
end

-- Inventory slots of a player, assigned identically on every client (Noita's HUD, which
-- assigns them in vanilla, runs on no slot). Quick inventory: wands and other items each
-- own slots 0-3 (number keys 1-4 and 5-8); spell inventory: 16 slots; each wand: one slot
-- per deck card (always-cast cards excluded). Only containers whose children changed are
-- visited (`slot.inv_sig`), so this costs a few calls per player per frame.
local function normalize_inventory(slot)
  slot.inv_sig = slot.inv_sig or {}
  local function changed(container)
    local sig = table.concat(EntityGetAllChildren(container) or {}, ",")
    if slot.inv_sig[container] == sig then return false end
    slot.inv_sig[container] = sig
    return true
  end
  local quick = child_named(slot.entity, "inventory_quick")
  if quick then
    if changed(quick) then
      assign_slots(items_of(quick, is_wand), 4)
      assign_slots(items_of(quick, not_wand), 4)
    end
    for _, wand in ipairs(EntityGetAllChildren(quick) or {}) do
      if is_wand(wand) and changed(wand) then
        local ability = EntityGetFirstComponentIncludingDisabled(wand, "AbilityComponent")
        local cap = ability and ComponentObjectGetValue2(ability, "gun_config", "deck_capacity") or 0
        assign_slots(items_of(wand, is_card), cap)
      end
    end
  end
  local full = child_named(slot.entity, "inventory_full")
  if full and changed(full) then
    local inv = EntityGetFirstComponentIncludingDisabled(slot.entity, "Inventory2Component")
    local cap = inv and ComponentGetValue2(inv, "full_inventory_slots_x") * ComponentGetValue2(inv, "full_inventory_slots_y") or 16
    assign_slots(items_of(full, function() return true end), cap)
  end
end

-- Quick-inventory items of `e` in the engine's item-switching order (wands by slot, then
-- other items by slot): { {slot=n, entity=item}, ... }, slot n = number key n + 1.
local function quick_items(e)
  local out = {}
  local quick = child_named(e, "inventory_quick")
  for _, item in ipairs(quick and EntityGetAllChildren(quick) or {}) do
    local ic = EntityGetFirstComponentIncludingDisabled(item, "ItemComponent")
    if ic then
      local sx = ComponentGetValue2(ic, "inventory_slot")
      out[#out + 1] = { slot = sx + (is_wand(item) and 0 or 4), entity = item }
    end
  end
  table.sort(out, function(a, b) return a.slot < b.slot end)
  return out
end

-- Number-key selection, expressed as the equivalent change-item step count so the
-- engine's own item switching runs (identically on every client).
local function select_quick_slot(slot, k, frame)
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
  ComponentSetValue2(slot.controls, "mButtonFrameChangeItemR", Input.press_frame(frame))
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
      normalize_inventory(slot)
      local state = Input.decode(bytes)
      Input.apply(slot.controls, state, slot.prev, frame)
      if state.select and state.select > 0 then select_quick_slot(slot, state.select, frame) end
      for _, cmd in ipairs(state.cmds or {}) do Inventory.apply(slot.entity, cmd) end
      slot.prev = state
    end
  end
end

return M
