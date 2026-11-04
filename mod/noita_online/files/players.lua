-- Player entities in a lockstep session.
--
-- Every client holds the same set of player entities, one per session slot, created at the
-- same frame in the same order, so entity ids and all simulation state match. Each slot is
-- driven only by the inputs in the server's ticks (ControlsComponent disabled). The engine's
-- own spawned player ("proxy") is made inert on every client and only serves as the sink for
-- this client's real keyboard/mouse/gamepad input, which we read and send to the server.
local Input = dofile_once("mods/noita_online/files/input.lua")
local NO = dofile_once("mods/noita_online/files/native.lua")
local LocalPrints = dofile_once("mods/noita_online/files/local_prints.lua")
local Robes = dofile_once("mods/noita_online/files/robes.lua")

local M = {}

M.slots = {}          -- array of { id = player_id, entity = eid, controls = cid, prev = state }
M.by_id = {}
M.proxy = nil
M.proxy_controls = nil
M.local_id = nil

-- Only the ControlsComponent stays on: Noita's input system writes this client's keyboard,
-- mouse and gamepad state into it (ControlsSystem). Everything else is off, including the
-- PlatformShooterPlayerComponent, whose update reacts to that local input (crouching, eating
-- the cells around the proxy, the reticle) and would change the world on this client alone.
local KEEP_ON_PROXY = { ControlsComponent = true }

local function make_proxy_inert(e)
  for _, c in ipairs(EntityGetAllComponents(e) or {}) do
    local name = ComponentGetTypeName(c)
    if not KEEP_ON_PROXY[name] then
      EntitySetComponentIsEnabled(e, c, false)
    end
  end
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

-- Tell the client runtime (slots.rs) the slot entities, which it follows through polymorphs,
-- our own slot (none once we died: vanilla shows a dead player nothing more) and the proxy;
-- and the patched vanilla scripts (local_prints.lua) our own player's entity.
local function register_slots()
  local ents, local_index = {}, -1
  for i, slot in ipairs(M.slots) do
    ents[i] = slot.entity
    if slot.id == M.local_id and not slot.dead then local_index = i - 1 end
  end
  NO.slots_set(ents, local_index, M.proxy or 0)
  LocalPrints.set_local(local_index >= 0 and ents[local_index + 1] or 0)
end

-- Called from OnPlayerSpawned (same frame on every client).
function M.setup(proxy, ids, local_id)
  M.proxy = proxy
  M.local_id = local_id
  local x, y = EntityGetTransform(proxy)
  make_proxy_inert(proxy)
  M.proxy_controls = EntityGetFirstComponentIncludingDisabled(proxy, "ControlsComponent")
  for i, id in ipairs(ids) do
    local e = EntityLoad(Robes.entity_file(i), x, y)
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
    -- Noita's own inventory GUI (InventoryGuiComponent) stays enabled on every slot, so the
    -- simulation sees the same components everywhere, but the engine never runs it for a
    -- slot: this client runs it for its own player outside the simulation window
    -- (M.gui_update), and the client runtime turns its inventory edits into input ops.
    if id == local_id then NO.lib.no_gui_set_local(e) end
  end
  register_slots()
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
  if NO.lib.no_slots_install() ~= 0 and M.log then M.log("slot polymorph patch failed: " .. NO.last_error()) end
  register_slots()
end

-- Diagnostics (init.lua, NOITA_ONLINE_LADDER=proxy): the solo baseline plus an inert input
-- proxy like a session's, to measure what the proxy costs.
function M.ladder_proxy(player)
  local x, y = EntityGetTransform(player)
  make_proxy_inert(EntityLoad("data/entities/player.xml", x, y))
end

function M.local_slot()
  return M.by_id[M.local_id]
end

-- This client's view is vanilla's camera: Noita's PlatformShooterPlayerSystem computes it
-- (mouse look-ahead, smoothing, recoil, item camera distance, teleport snaps) inside the
-- window for the slot with center_camera_on_this_entity, i.e. only our own, into
-- mDesiredCameraPos, and the render camera follows that (init.lua). Its look-ahead reads the
-- raw screen mouse position from the slot's ControlsComponent, which the engine only writes
-- for an enabled one: copied from the proxy here, inside the window before the systems run.
-- Presentation only: no simulation system reads a slot's raw mouse position. With a gamepad
-- the camera's look-ahead follows the raw stick aim instead (mGamepadAimingVectorRaw, read
-- only by that camera code; slots.rs gives our slot the proxy's gamepad there).
function M.feed_local_camera()
  local slot = M.local_slot()
  if not slot or not slot.controls or not M.proxy_controls or not EntityGetIsAlive(slot.entity) then return end
  ComponentSetValue2(slot.controls, "mMousePositionRaw", ComponentGetValue2(M.proxy_controls, "mMousePositionRaw"))
  ComponentSetValue2(slot.controls, "mGamepadAimingVectorRaw", ComponentGetValue2(M.proxy_controls, "mGamepadAimingVectorRaw"))
end

-- Vanilla's camera for a player without a PlatformShooterPlayerComponent, i.e. polymorphed
-- into a creature (camera update FUN_006b3ff0): each frame it moves a quarter of the way to
-- the entity, or all the way when that is under 1 px or over 512 px; it holds still while
-- the player is "polymorphed_cessation".
local POLY_CAM_EASE, POLY_CAM_MIN2, POLY_CAM_MAX2 = 0.25, 1, 512 * 512

-- The camera position Noita computes for our player this frame (nil before the first one).
function M.local_camera_target()
  local slot = M.local_slot()
  if not slot or not EntityGetIsAlive(slot.entity) then return nil end
  local x, y
  local psp = EntityGetFirstComponentIncludingDisabled(slot.entity, "PlatformShooterPlayerComponent")
  if psp then
    x, y = ComponentGetValue2(psp, "mDesiredCameraPos")
    if x == 0 and y == 0 then return nil end
  elseif EntityHasTag(slot.entity, "polymorphed_cessation") then
    if not M.view then return nil end
    x, y = M.view[1], M.view[2]
  else
    x, y = EntityGetTransform(slot.entity)
    if M.view then
      local dx, dy = x - M.view[1], y - M.view[2]
      local d2 = dx * dx + dy * dy
      if d2 > POLY_CAM_MIN2 and d2 < POLY_CAM_MAX2 then
        x, y = M.view[1] + dx * POLY_CAM_EASE, M.view[2] + dy * POLY_CAM_EASE
      end
    end
  end
  M.view = { x, y }
  return x, y
end

-- The aiming reticle (a crosshair sprite on the player) is for gamepad aiming. Noita sets its
-- visibility on every "player_unit" entity when the pause state changes (FUN_006f4d10:
-- visible = not paused and a gamepad is active), which happens before our slots exist, so a
-- slot keeps the sprite's default (visible); with the mouse vanilla never shows it.
function M.update_reticle()
  local slot = M.local_slot()
  if not slot or not EntityGetIsAlive(slot.entity) then return end
  local reticle = EntityGetFirstComponentIncludingDisabled(slot.entity, "SpriteComponent", "aiming_reticle")
  if reticle then ComponentSetValue2(reticle, "visible", GameGetIsGamepadConnected()) end
end

-- Real input of this client, as captured by the engine into the proxy's ControlsComponent.
-- The client runtime normally updates the proxy with this frame's input before
-- OnWorldPreUpdate (earlyinput.rs: presses stamped for the coming update, the mouse mapped to
-- the world through this client's render camera). Without that update the proxy holds the
-- last update's input, whose mouse went through the shared simulation camera (another
-- player's view on every client but the first); then `mouse` ({x, y}: the pointer under the
-- render camera, taken after the last update) replaces it.
function M.capture_local(mouse)
  if not M.proxy_controls then return Input.NEUTRAL end
  local now = GameGetFrameNum()
  local fresh = NO.lib.no_input_fresh() == 1
  local s = Input.capture_from(M.proxy_controls, fresh and Input.press_frame(now) or now)
  if not fresh and mouse then s.mx, s.my = mouse[1], mouse[2] end
  -- With a gamepad the proxy's aim is Noita's stick aim; with the mouse the aim follows from
  -- the mouse position when the input is applied (Input.apply), as in Noita's ControlsSystem.
  s.gamepad = GameGetIsGamepadConnected()
  -- The Inventory key is Noita's inventory screen (M.gui_update), local only. The world
  -- keeps running for the other players while it is open (vanilla pauses it); our player
  -- does nothing meanwhile, as in vanilla, and the mouse works the screen.
  s.buttons.Inventory = false
  if M.gui_open() then s.buttons, s.count_r, s.count_l = {}, 0, 0 end
  s.ops = NO.gui_take_ops()
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

-- Inventory slots of a player, assigned identically on every client (in vanilla Noita's
-- inventory GUI assigns them; here it runs on this client only and its own slot writes are
-- undone, see M.gui_update). Quick inventory: wands and other items each
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

local function gui_component()
  local slot = M.local_slot()
  if not slot or not M.proxy_controls or not EntityGetIsAlive(slot.entity) then return nil end
  return EntityGetFirstComponentIncludingDisabled(slot.entity, "InventoryGuiComponent")
end

-- Whether one of Noita's inventory screens (the inventory, or the "replace which item"
-- screen of a full-inventory pickup) is open on this client.
function M.gui_open()
  local gui = gui_component()
  return gui ~= nil and (M.swap_open or ComponentGetValue2(gui, "mActive"))
end

-- Run Noita's inventory GUI (HUD, quick bar, tooltips, inventory screen) for this client's
-- own player, after the simulation window. Its inventory edits do not happen here: the
-- client runtime records them as ops, sent with the next input and applied by every client
-- in the tick (apply_tick). It reads this client's buttons straight from its input, like
-- vanilla; the client runtime sets them on the player's ControlsComponent for the call and
-- undoes the GUI's direct writes of simulation fields after it (invgui.rs gui_update).
function M.gui_update()
  local gui = gui_component()
  if not gui then return end
  -- A full-inventory pickup by our player opens the replace screen instead (in vanilla the
  -- world pauses meanwhile and the inventory GUI does not run).
  M.swap_open = NO.lib.no_gui_swap_update() == 1
  if M.swap_open then return end
  NO.lib.no_gui_update()
end

local bit = require("bit")
local function u32le(v)
  v = bit.tobit(v)
  return string.char(bit.band(v, 255), bit.band(bit.rshift(v, 8), 255), bit.band(bit.rshift(v, 16), 255), bit.band(bit.rshift(v, 24), 255))
end

-- The scripted test bot (files/bot.lua) cannot use the GUI; it states what it would do
-- there and gets the same ops the GUI records for it (crates/client/src/invgui.rs):
-- `select = k` (number key k: equip the quick-bar item of key k) and
-- `swap = { wand = n, a = i, b = j }` (drag spell slot i of the n-th wand onto slot j).
function M.bot_ops(s)
  local slot = M.local_slot()
  local quick = slot and EntityGetIsAlive(slot.entity) and child_named(slot.entity, "inventory_quick")
  if not quick then return s end
  local ops = {}
  local function slot_item(container, keep, x)
    for _, it in ipairs(items_of(container, keep)) do
      if ComponentGetValue2(it.ic, "inventory_slot") == x then return it.e end
    end
  end
  if s.select then
    local k = s.select
    local item = k <= 4 and slot_item(quick, is_wand, k - 1) or slot_item(quick, not_wand, k - 5)
    if item then ops[#ops + 1] = string.char(1) .. u32le(item) .. string.char(1) .. u32le(1) end
  end
  if s.swap then
    local wand = slot_item(quick, is_wand, s.swap.wand - 1)
    local a = wand and slot_item(wand, is_card, s.swap.a)
    local b = wand and slot_item(wand, is_card, s.swap.b)
    if a and b then
      -- The GUI's swap: take both, put each into the other's slot.
      ops[#ops + 1] = string.char(2) .. u32le(a) .. string.char(2) .. u32le(b)
      ops[#ops + 1] = string.char(3) .. u32le(wand) .. u32le(a) .. u32le(s.swap.b) .. u32le(0) .. u32le(0) .. string.char(0)
      ops[#ops + 1] = string.char(3) .. u32le(wand) .. u32le(b) .. u32le(s.swap.a) .. u32le(0) .. u32le(0) .. string.char(0)
    end
  end
  s.ops = table.concat(ops)
  return s
end

-- A dead player's body: invulnerable, no longer a "player" for the simulation, and never
-- polymorphed (it would become a creature nobody controls).
local function make_spectator(e)
  local dm = EntityGetFirstComponentIncludingDisabled(e, "DamageModelComponent")
  if dm then
    ComponentSetValue2(dm, "hp", 0.04)
    ComponentSetValue2(dm, "invincibility_frames", 2000000000)
  end
  EntityRemoveTag(e, "player_unit")
  EntityRemoveTag(e, "hittable")
  EntityRemoveTag(e, "mortal")
  EntityAddTag(e, "noita_online_dead")
  EntityAddTag(e, "polymorphable_NOT")
end

-- A player whose HP reached zero becomes an invulnerable, input-less spectator; one whose
-- entity is gone (e.g. its polymorphed creature died in the frame it was made) becomes a
-- spectator without a body.
local function check_death(slot)
  if slot.dead then return end
  if EntityGetIsAlive(slot.entity) then
    local dm = EntityGetFirstComponentIncludingDisabled(slot.entity, "DamageModelComponent")
    if not dm or ComponentGetValue2(dm, "hp") > 0 then return end
    make_spectator(slot.entity)
  end
  slot.dead = true
  if M.log then M.log(string.format("slot %d died: entity %d frame %d", slot.id, slot.entity, GameGetFrameNum())) end
  if slot.id == M.local_id then
    GamePrint("You died - spectating")
    register_slots()
  end
end

-- Follow a slot to its new entity: Noita polymorphed the player into a creature or restored
-- it (slots.rs records the swap inside the engine's polymorph code, identically on every
-- client). The creature is set up like a slot: our ControlsComponent to drive (vanilla's
-- input system would write this client's own input into an enabled one), no death by
-- itself (check_death), and a dead slot stays a spectator.
local function retarget(slot, e)
  if M.log then
    M.log(string.format("slot %d follows its player: entity %d -> %d (%s) frame %d", slot.id, slot.entity, e,
      EntityGetFilename(e) or "?", GameGetFrameNum()))
  end
  slot.entity = e
  slot.controls = EntityGetFirstComponentIncludingDisabled(e, "ControlsComponent")
  slot.inv_sig = nil
  slot.frozen = nil
  if not EntityGetIsAlive(e) then return end
  EntityAddTag(e, "noita_online_player")
  if slot.controls then ComponentSetValue2(slot.controls, "enabled", false) end
  local dm = EntityGetFirstComponentIncludingDisabled(e, "DamageModelComponent")
  if dm then ComponentSetValue2(dm, "wait_for_kill_flag_on_death", true) end
  if slot.dead then make_spectator(e) end
  if slot.id == M.local_id then
    NO.lib.no_gui_set_local(e)
    if not slot.dead then LocalPrints.set_local(e) end
  end
end

local function follow_slots()
  local ids = NO.slots_get()
  for i, slot in ipairs(M.slots) do
    local e = ids[i]
    if e and e ~= 0 and e ~= slot.entity then retarget(slot, e) end
  end
end

-- Apply one tick: { [player_id] = input bytes }. Slots absent from the tick have left.
function M.apply_tick(frame, tick)
  follow_slots()
  for _, slot in ipairs(M.slots) do
    local bytes = tick[slot.id]
    check_death(slot)
    if bytes == nil or slot.dead then
      if not slot.frozen then
        slot.frozen = true
        -- Departed or dead player: neutral input from now on, identically on every client.
        if slot.controls and EntityGetIsAlive(slot.entity) then
          Input.apply(slot.controls, Input.NEUTRAL, frame, slot.entity)
          slot.prev = Input.NEUTRAL
        end
      end
    elseif slot.controls then
      normalize_inventory(slot)
      local state = Input.decode(bytes)
      Input.apply(slot.controls, state, frame, slot.entity)
      if #state.ops > 0 then NO.lib.no_gui_apply(slot.entity, state.ops, #state.ops) end
      slot.prev = state
    end
  end
end

return M
