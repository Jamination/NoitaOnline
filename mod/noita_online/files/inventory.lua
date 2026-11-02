-- Lockstep-safe wand editing.
--
-- Noita's own inventory screen edits entities immediately on the local client only, which
-- would desynchronise the lockstep simulation, so it is never opened for player slots (the
-- Inventory button is not transmitted). Instead this client draws its own editor for its
-- own player; clicking spells produces MOVE commands that travel in the player's input and
-- are applied by every client in the same simulation frame.
--
-- Containers: 0 = spell inventory (inventory_full), 1..4 = wands in quick-slot order.
local M = {}

local gui = nil
M.open = false
local selected = nil -- { c = container, s = slot }
local pending = {}   -- commands to send with the next input

local sprite_of = nil
local function action_sprite(action_id)
  if not sprite_of then
    sprite_of = {}
    dofile_once("data/scripts/gun/gun_actions.lua")
    for _, a in ipairs(actions or {}) do sprite_of[a.id] = a.sprite end
  end
  return sprite_of[action_id] or "data/ui_gfx/gun_actions/unidentified.png"
end

local function child_named(e, name)
  for _, c in ipairs(EntityGetAllChildren(e) or {}) do
    if EntityGetName(c) == name then return c end
  end
end

local function slot_x(item)
  local ic = EntityGetFirstComponentIncludingDisabled(item, "ItemComponent")
  if not ic then return nil end
  return ComponentGetValue2(ic, "inventory_slot")
end

-- Wands of player `e` in quick-slot order.
local function wands(e)
  local quick = child_named(e, "inventory_quick")
  local out = {}
  for _, item in ipairs(quick and EntityGetAllChildren(quick) or {}) do
    if EntityHasTag(item, "wand") then out[#out + 1] = item end
  end
  table.sort(out, function(a, b) return (slot_x(a) or 0) < (slot_x(b) or 0) end)
  return out
end

-- Container entity and its capacity.
local function container(e, c)
  if c == 0 then
    return child_named(e, "inventory_full"), 16
  end
  local w = wands(e)[c]
  if not w then return nil, 0 end
  local ability = EntityGetFirstComponentIncludingDisabled(w, "AbilityComponent")
  local cap = ability and ComponentObjectGetValue2(ability, "gun_config", "deck_capacity") or 0
  return w, cap
end

-- Spell card entity at slot `s` of container entity `ce` (nil if empty).
local function card_at(ce, s)
  for _, card in ipairs(EntityGetAllChildren(ce) or {}) do
    if EntityGetFirstComponentIncludingDisabled(card, "ItemActionComponent") and slot_x(card) == s then
      return card
    end
  end
end

local function place(card, ce, s)
  EntityRemoveFromParent(card)
  EntityAddChild(ce, card)
  local ic = EntityGetFirstComponentIncludingDisabled(card, "ItemComponent")
  if ic then ComponentSetValue2(ic, "inventory_slot", s, 0) end
  EntitySetComponentsWithTagEnabled(card, "enabled_in_world", false)
end

-- Deterministic application (every client, same frame): move/swap a spell.
function M.apply(player, cmd)
  local sce, scap = container(player, cmd.sc)
  local dce, dcap = container(player, cmd.dc)
  if not sce or not dce or cmd.ss >= scap or cmd.ds >= dcap then return end
  local a = card_at(sce, cmd.ss)
  if not a then return end
  local b = card_at(dce, cmd.ds)
  place(a, dce, cmd.ds)
  if b and b ~= a then place(b, sce, cmd.ss) end
  local inv = EntityGetFirstComponentIncludingDisabled(player, "Inventory2Component")
  if inv then ComponentSetValue2(inv, "mForceRefresh", true) end
end

-- Commands queued by the local editor (or the test bot) since the last input.
function M.take_pending()
  local p = pending
  pending = {}
  return p
end

function M.queue(cmd)
  pending[#pending + 1] = cmd
end

-- Local UI (presentation only, outside the simulation): toggled by the Inventory key.
function M.toggle()
  M.open = not M.open
  selected = nil
end

function M.draw(player)
  if not M.open or not player or not EntityGetIsAlive(player) then return end
  gui = gui or GuiCreate()
  GuiStartFrame(gui)
  local id = 1
  local function slot_button(c, s, ce, x, y)
    local card = ce and card_at(ce, s)
    local sprite = "data/ui_gfx/inventory/full_inventory_box.png"
    if card then
      local iac = EntityGetFirstComponentIncludingDisabled(card, "ItemActionComponent")
      sprite = action_sprite(iac and ComponentGetValue2(iac, "action_id") or "")
    end
    if selected and selected.c == c and selected.s == s then GuiColorSetForNextWidget(gui, 1, 1, 0.3, 1) end
    local clicked = GuiImageButton(gui, id, x, y, "", sprite)
    id = id + 1
    if clicked then
      if not selected then
        if card then selected = { c = c, s = s } end
      else
        if selected.c ~= c or selected.s ~= s then
          M.queue({ sc = selected.c, ss = selected.s, dc = c, ds = s })
        end
        selected = nil
      end
    end
  end
  -- Wand rows below Noita's quick-inventory bar; spell inventory in the bottom-right corner.
  local sw, sh = GuiGetScreenDimensions(gui)
  local step = 18
  local x0, y0 = 10, 84
  GuiText(gui, x0, y0 - 12, "Wands - click a spell, then a slot (Inventory key closes)")
  local row = 0
  for c = 1, 4 do
    local ce, cap = container(player, c)
    if ce then
      for s = 0, cap - 1 do slot_button(c, s, ce, x0 + s * step, y0 + row * step) end
      row = row + 1
    end
  end
  local ce = container(player, 0)
  local sx, sy = sw - 8 * step - 6, sh - 2 * step - 6
  GuiText(gui, sx, sy - 12, "Spells")
  for s = 0, 15 do slot_button(0, s, ce, sx + (s % 8) * step, sy + math.floor(s / 8) * step) end
end

return M
