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

M.slots = {}          -- array of { id = player_id, index = slot position (1-based), name =, entity = eid, controls = cid, prev = state, dead =, left = }
M.by_id = {}
M.proxy = nil
M.proxy_controls = nil
M.local_id = nil
-- Respawn penalty (server options --respawn-gold-loss, --respawn-item-loss, sent in Start)
-- and the session seed (the lost item's draw).
M.params = { gold_loss = 0.5, item_loss = 1, seed = 1 }
-- Called (inside the simulation window, every client) when no player is alive any more.
M.on_all_dead = nil

local DAMAGE_LOG = "mods/noita_online/files/damage_log.lua"
-- The lockstep input op of a dead player choosing where to respawn: this byte, then the
-- chosen (living) player's id. Handled here (a dead player's ops never reach the inventory
-- GUI's op player, crates/client/src/invgui.rs).
local RESPAWN_OP = 0xF0

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
-- our own slot (none while we are dead) and the proxy; and the patched vanilla scripts
-- (local_prints.lua) our own player's entity.
local function register_slots()
  local ents, local_index = {}, -1
  for i, slot in ipairs(M.slots) do
    ents[i] = slot.entity
    if slot.id == M.local_id and not slot.dead then local_index = i - 1 end
  end
  NO.slots_set(ents, local_index, M.proxy or 0)
  LocalPrints.set_local(local_index >= 0 and ents[local_index + 1] or 0)
end

-- Every entity a slot drives (its player, a creature it is polymorphed into, a respawned
-- player), identically on every client: inputs come from the ticks (ControlsComponent
-- disabled and written by the mod); a dying player stays at 0 HP until the mod has stored it
-- for its respawn (`wait_for_kill_flag_on_death`, see `die`), then dies vanilla's death; and
-- every damage it takes is recorded for the debug log (files/damage_log.lua).
local function adopt(slot, e)
  EntityAddTag(e, "noita_online_player")
  slot.controls = EntityGetFirstComponentIncludingDisabled(e, "ControlsComponent")
  if slot.controls then ComponentSetValue2(slot.controls, "enabled", false) end
  local dm = EntityGetFirstComponentIncludingDisabled(e, "DamageModelComponent")
  if dm then ComponentSetValue2(dm, "wait_for_kill_flag_on_death", true) end
  for _, c in ipairs(EntityGetComponentIncludingDisabled(e, "LuaComponent") or {}) do
    if ComponentGetValue2(c, "script_damage_received") == DAMAGE_LOG then return end
  end
  EntityAddComponent2(e, "LuaComponent", { script_damage_received = DAMAGE_LOG, execute_every_n_frame = -1 })
end

-- Presentation of our own player: the camera follows only this client's own slot, and
-- Noita's own inventory GUI (InventoryGuiComponent) stays enabled on every slot, so the
-- simulation sees the same components everywhere, but the engine never runs it for a slot:
-- this client runs it for its own player outside the simulation window (M.gui_update), and
-- the client runtime turns its inventory edits into input ops.
local function own_view(slot, e)
  local shooter = EntityGetFirstComponentIncludingDisabled(e, "PlatformShooterPlayerComponent")
  if shooter then ComponentSetValue2(shooter, "center_camera_on_this_entity", slot.id == M.local_id) end
  if slot.id == M.local_id then NO.lib.no_gui_set_local(e) end
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
    local slot = { id = id, index = i, name = NO.player_name(i - 1) or ("p" .. id), entity = e, prev = Input.NEUTRAL }
    adopt(slot, e)
    own_view(slot, e)
    M.slots[#M.slots + 1] = slot
    M.by_id[id] = slot
  end
  register_slots()
end

-- Solo baseline (no networking): the engine's own player is slot 0, driven by applied
-- inputs exactly like a lockstep slot, and it dies like one (with nobody to respawn at:
-- vanilla's game over).
function M.setup_solo(player)
  M.local_id = 0
  M.solo = true
  local slot = { id = 0, index = 1, name = "p0", entity = player, prev = Input.NEUTRAL }
  adopt(slot, player)
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

-- Whether this client's own player is dead (waiting to respawn, or game over).
function M.local_dead()
  local slot = M.local_slot()
  return slot ~= nil and slot.dead == true
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
-- While our player is dead the view stays where it was when it died, as vanilla's game over
-- leaves the camera (the simulation window puts the camera elsewhere every frame).
function M.local_camera_target()
  local slot = M.local_slot()
  if slot and slot.dead then
    if M.view then return M.view[1], M.view[2] end
    return nil
  end
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
  -- A dead player does nothing but choose where to respawn (M.choose_respawn).
  if M.local_dead() then return M.dead_input() end
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
-- `swap = { wand = n, a = i, b = j }` (drag spell slot i of the n-th wand onto slot j);
-- dead, `respawn = player id` (the respawn picker's choice).
function M.bot_ops(s)
  if M.local_dead() then
    if s.respawn then M.choose_respawn(s.respawn) end
    return M.dead_input()
  end
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

-- ---------------------------------------------------------------------------------------
-- Death and respawn.
--
-- A slot whose HP reached 0 (the engine holds it there: `wait_for_kill_flag_on_death`) is
-- stored by the client runtime with the engine's own entity serializer (the one polymorph
-- uses to keep the player: with its children, i.e. perks, effects, wands, spells, items),
-- then dies vanilla's death in this frame's update (`kill_now`: death effects, sound,
-- ragdoll in its robe colour, which stays in the world). A player that dies as a creature is
-- the record its polymorph keeps. Its client shows vanilla's game over; its "New game"
-- button opens the respawn picker instead (crates/client/src/respawn.rs), whose choice is a
-- lockstep op (RESPAWN_OP): in that tick every client loads the stored player at the chosen
-- player's position, at full HP, minus the penalty. With nobody left alive it is vanilla's
-- game over for everyone (M.on_all_dead).

-- The player a polymorph keeps (base64 `mSerializedData` of its polymorph effect), for the
-- creature `e`.
local function polymorph_record(e)
  local function scan(ent)
    for _, c in ipairs(EntityGetComponentIncludingDisabled(ent, "GameEffectComponent") or {}) do
      local data = ComponentGetValue2(c, "mSerializedData")
      if type(data) == "string" and data ~= "" then return data end
    end
  end
  local data = scan(e)
  if data then return data end
  for _, child in ipairs(EntityGetAllChildren(e) or {}) do
    data = scan(child)
    if data then return data end
  end
end

local function alive(slot)
  return not slot.dead and not slot.left and EntityGetIsAlive(slot.entity)
end

local function die(slot, frame)
  local e = slot.entity
  local x, y = slot.last_x or 0, slot.last_y or 0
  local size, kind = -1, "gone"
  if EntityGetIsAlive(e) then
    x, y = EntityGetTransform(e)
    local data = EntityHasTag(e, "polymorphed_player") and polymorph_record(e)
    if data then
      size, kind = NO.respawn_save_b64(slot.index - 1, data), "polymorph record"
    else
      size, kind = NO.lib.no_respawn_save(slot.index - 1, e), "player"
    end
    local dm = EntityGetFirstComponentIncludingDisabled(e, "DamageModelComponent")
    -- Our own player's death finishes this client's game-over stats (cause of death, gold,
    -- time), as vanilla's player death does (crates/client/src/respawn.rs).
    if slot.id == M.local_id then NO.lib.no_local_death(e) end
    if dm then ComponentSetValue2(dm, "kill_now", true) else EntityKill(e) end
  end
  slot.dead, slot.death_x, slot.death_y, slot.death_frame = true, x, y, frame
  slot.frozen, slot.inv_sig = nil, nil
  if M.log then
    M.log(string.format("slot %d died: entity %d (%s) at %.1f,%.1f frame %d; stored %s (%d bytes)", slot.id, e,
      EntityGetIsAlive(e) and EntityGetFilename(e) or "gone", x, y, frame, kind, size))
  end
  if slot.id == M.local_id then
    M.pending_game_over = true
    register_slots()
  else
    GamePrint(slot.name .. " died")
  end
end

local function check_death(slot, frame)
  if slot.dead then return end
  if EntityGetIsAlive(slot.entity) then
    local dm = EntityGetFirstComponentIncludingDisabled(slot.entity, "DamageModelComponent")
    if not dm or ComponentGetValue2(dm, "hp") > 0 then return end
  end
  die(slot, frame)
end

-- Park-Miller draws from the session seed, the frame and the player: the same everywhere,
-- and no engine RNG is touched.
local function draws(frame, id)
  local x = (M.params.seed % 2147483646 + frame * 7919 + id * 104729) % 2147483647
  if x == 0 then x = 1 end
  return function(n)
    x = (x * 48271) % 2147483647
    return x % n + 1
  end
end

local function item_label(e)
  local ability = EntityGetFirstComponentIncludingDisabled(e, "AbilityComponent")
  local name = ability and ComponentGetValue2(ability, "ui_name")
  if not name or name == "" then
    local ic = EntityGetFirstComponentIncludingDisabled(e, "ItemComponent")
    name = ic and ComponentGetValue2(ic, "item_name")
  end
  if not name or name == "" then return EntityGetFilename(e) or "?" end
  return GameTextGetTranslatedOrNot(name)
end

-- The respawn penalty: M.params.gold_loss of the gold (rounded down) and M.params.item_loss
-- random items of the quick inventory (wands, flasks, other items; never the last wand),
-- which are destroyed. Returns the gold and the items' names lost.
local function penalty(slot, e, frame)
  local gold = 0
  local wallet = EntityGetFirstComponentIncludingDisabled(e, "WalletComponent")
  if wallet then
    local money = ComponentGetValue2(wallet, "money")
    gold = math.floor(money * M.params.gold_loss)
    ComponentSetValue2(wallet, "money", money - gold)
    ComponentSetValue2(wallet, "mMoneyPrevFrame", money - gold)
  end
  local lost = {}
  local quick = child_named(e, "inventory_quick")
  local draw = draws(frame, slot.id)
  for _ = 1, M.params.item_loss do
    local items, wands = {}, 0
    for _, c in ipairs(quick and EntityGetAllChildren(quick) or {}) do
      if EntityGetFirstComponentIncludingDisabled(c, "ItemComponent") then
        items[#items + 1] = c
        if is_wand(c) then wands = wands + 1 end
      end
    end
    local candidates = {}
    for _, c in ipairs(items) do
      if wands > 1 or not is_wand(c) then candidates[#candidates + 1] = c end
    end
    if #candidates == 0 then break end
    local c = candidates[draw(#candidates)]
    lost[#lost + 1] = item_label(c)
    EntityRemoveFromParent(c)
    EntityKill(c)
  end
  return gold, lost
end

local function respawn(slot, target, frame)
  local x, y = EntityGetTransform(target.entity)
  local e = NO.lib.no_respawn_restore(slot.index - 1, x, y)
  local how = "stored player"
  if e == 0 or not EntityGetIsAlive(e) then
    -- Nothing stored (the player vanished without dying, e.g. its creature died in the frame
    -- it was made): a new player.
    e, how = EntityLoad(Robes.entity_file(slot.index), x, y), "new player"
  end
  slot.entity = e
  adopt(slot, e)
  local dm = EntityGetFirstComponentIncludingDisabled(e, "DamageModelComponent")
  if dm then
    ComponentSetValue2(dm, "kill_now", false)
    ComponentSetValue2(dm, "hp", ComponentGetValue2(dm, "max_hp"))
    ComponentSetValue2(dm, "air_in_lungs", ComponentGetValue2(dm, "air_in_lungs_max"))
  end
  slot.dead, slot.frozen, slot.inv_sig, slot.prev, slot.respawn_frame = false, nil, nil, Input.NEUTRAL, frame
  local gold, lost = penalty(slot, e, frame)
  own_view(slot, e)
  register_slots()
  if M.log then
    M.log(string.format("slot %d respawned: entity %d (%s) at player %d %.1f,%.1f frame %d; lost %d gold, items: %s",
      slot.id, e, how, target.id, x, y, frame, gold, table.concat(lost, ", ")))
  end
  if slot.id == M.local_id then
    M.pending_respawned = { at = target.name, gold = gold, lost = lost }
  else
    GamePrint(slot.name .. " respawned at " .. target.name)
  end
end

-- The living player a dead slot's input op chose to respawn at, if valid.
local function respawn_target(ops)
  if #ops < 2 or ops:byte(1) ~= RESPAWN_OP then return nil end
  local target = M.by_id[ops:byte(2)]
  return target and alive(target) and target or nil
end

-- Follow a slot to its new entity: Noita polymorphed the player into a creature or restored
-- it (slots.rs records the swap inside the engine's polymorph code, identically on every
-- client). The creature is set up like a slot (adopt).
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
  adopt(slot, e)
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
    if bytes == nil then slot.left = true end
    check_death(slot, frame)
    if slot.dead then
      local target = bytes and respawn_target(Input.decode(bytes).ops)
      if target and not M.all_dead then respawn(slot, target, frame) end
    elseif bytes == nil then
      if not slot.frozen then
        slot.frozen = true
        -- Departed player: neutral input from now on, identically on every client.
        if slot.controls and EntityGetIsAlive(slot.entity) then
          Input.apply(slot.controls, Input.NEUTRAL, frame, slot.entity)
          slot.prev = Input.NEUTRAL
        end
      end
    elseif slot.controls then
      normalize_inventory(slot)
      local state = Input.decode(bytes)
      Input.apply(slot.controls, state, frame, slot.entity)
      -- (a respawn choice sent while dead can arrive after the respawn: ignored)
      if #state.ops > 0 and state.ops:byte(1) ~= RESPAWN_OP then NO.lib.no_gui_apply(slot.entity, state.ops, #state.ops) end
      slot.prev = state
    end
    if not slot.dead and EntityGetIsAlive(slot.entity) then
      slot.last_x, slot.last_y = EntityGetTransform(slot.entity)
    end
  end
  if not M.all_dead and #M.slots > 0 then
    local living, present = 0, 0
    for _, slot in ipairs(M.slots) do
      if not slot.left then present = present + 1 end
      if alive(slot) then living = living + 1 end
    end
    if present > 0 and living == 0 then
      M.all_dead, M.all_dead_frame = true, frame
      M.pending_game_over = true
      NO.lib.no_game_over_shared()
      if M.log then M.log("every player is dead: game over at frame " .. frame) end
      if M.on_all_dead then M.on_all_dead(frame) end
    end
  end
end

-- ---------------------------------------------------------------------------------------
-- This client's dead player (outside the simulation window).

local Picker = dofile_once("mods/noita_online/files/respawn_picker.lua")
local respawn_choice = nil -- player id chosen in the picker, sent with the next input
local picker_open = false

-- The input of our dead player: nothing but the respawn choice, once.
function M.dead_input()
  local s = { buttons = {}, count_r = 0, count_l = 0, mx = 0, my = 0, ax = 0, ay = 0, ops = "" }
  if respawn_choice then
    s.ops = string.char(RESPAWN_OP, respawn_choice)
    respawn_choice = nil
  end
  return s
end

function M.choose_respawn(id)
  respawn_choice = id
end

-- The living players we can respawn at: { id, name, colour (RGB) }, slot order.
local function respawn_choices()
  local out = {}
  for _, slot in ipairs(M.slots) do
    if alive(slot) then
      out[#out + 1] = { id = slot.id, name = slot.name, colour = Robes.colour(slot.index) }
    end
  end
  return out
end

-- After the simulation window: vanilla's game over for our player's death (and for
-- everyone's), its respawn button and the picker, and the end of the game over on respawn.
function M.after_window()
  if M.pending_game_over then
    M.pending_game_over = false
    picker_open = false
    GameTriggerGameOver()
  end
  if M.pending_respawned then
    local r = M.pending_respawned
    M.pending_respawned = nil
    picker_open = false
    NO.lib.no_game_over_clear()
    local msg = "You respawned at " .. r.at
    if r.gold > 0 or #r.lost > 0 then
      local parts = {}
      if r.gold > 0 then parts[#parts + 1] = r.gold .. " gold" end
      for _, name in ipairs(r.lost) do parts[#parts + 1] = name end
      msg = msg .. "; lost " .. table.concat(parts, ", ")
    end
    GamePrint(msg)
  end
  local choices = M.local_dead() and not M.all_dead and respawn_choices() or {}
  NO.lib.no_respawn_mode(#choices > 0 and 1 or 0)
  if #choices == 0 then picker_open = false end
  if NO.lib.no_respawn_take_press() == 1 and #choices > 0 then picker_open = true end
  local chosen = Picker.update(picker_open and choices or nil)
  if chosen == false then
    picker_open = false
  elseif chosen then
    picker_open = false
    M.choose_respawn(chosen)
  end
end

return M
