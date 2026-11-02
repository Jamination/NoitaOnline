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

local bit = require("bit")

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

-- HUD of this client's own player. Noita's HUD (InventoryGuiComponent) runs on no player
-- slot because it edits the inventory from local input (files/players.lua), so the mod draws
-- the quick inventory and the health/flight/mana/reload bars itself, with Noita's sprites.
local hud = nil
local png_of = {}
local function sprite_png(file)
  if not file or file == "" then return nil end
  if file:sub(-4) ~= ".xml" then return file end
  local png = png_of[file]
  if png == nil then
    png = (ModTextFileGetContent(file) or ""):match('filename="([^"]+)"') or false
    png_of[file] = png
  end
  return png or nil
end

local function item_png(item)
  local ability = EntityGetFirstComponentIncludingDisabled(item, "AbilityComponent")
  if ability and EntityHasTag(item, "wand") then return sprite_png(ComponentGetValue2(ability, "sprite_file")) end
  local ic = EntityGetFirstComponentIncludingDisabled(item, "ItemComponent")
  local ui = ic and ComponentGetValue2(ic, "ui_sprite") or ""
  if ui ~= "" then return sprite_png(ui) end
  local sprite = EntityGetFirstComponentIncludingDisabled(item, "SpriteComponent")
  return sprite and sprite_png(ComponentGetValue2(sprite, "image_file"))
end

local function draw_hud(player)
  hud = hud or GuiCreate()
  GuiStartFrame(hud)
  local id = 1000
  local function nid() id = id + 1 return id end
  local sw = GuiGetScreenDimensions(hud)
  local inv = EntityGetFirstComponentIncludingDisabled(player, "Inventory2Component")
  local active = inv and ComponentGetValue2(inv, "mActiveItem") or 0
  -- Quick inventory (Noita's layout): wands in boxes 1-4, other items in boxes 5-8.
  local pitch, bx, by = 20, 20, 45
  local at = {}
  local quick = child_named(player, "inventory_quick")
  for _, item in ipairs(quick and EntityGetAllChildren(quick) or {}) do
    local x = slot_x(item)
    if x and x >= 0 and x < 4 then at[x + (EntityHasTag(item, "wand") and 0 or 4)] = item end
  end
  for i = 0, 7 do
    local x = bx + i * pitch
    local item = at[i]
    if item and item == active then
      GuiImage(hud, nid(), x - 2, by - 2, "data/ui_gfx/inventory/quick_inventory_box_hovered.png", 1, 1)
    else
      GuiImage(hud, nid(), x, by, "data/ui_gfx/inventory/quick_inventory_box.png", 1, 1)
    end
    local png = item and item_png(item)
    if png then
      if EntityHasTag(item, "potion") then
        local c = GameGetPotionColorUint(item)
        if c and c ~= 0 then
          GuiColorSetForNextWidget(hud, bit.band(c, 255) / 255, bit.band(bit.rshift(c, 8), 255) / 255, bit.band(bit.rshift(c, 16), 255) / 255, 1)
        end
        GuiImage(hud, nid(), x + 2, by + 2, png, 1, 1)
      else
        GuiImage(hud, nid(), x + 3, by + 6, png, 1, 1)
      end
    end
  end
  -- Bars (top right): health, flight, mana and reload of the active wand, then gold.
  local bar_x, bar_w, icon_x, text_x = sw - 82, 42, sw - 37, sw - 29
  local function bar(y, frac, color, h, icon)
    GuiImage(hud, nid(), bar_x, y, "data/ui_gfx/hud/colors_bar_bg.png", 1, bar_w / 2, h / 2)
    frac = math.max(0, math.min(1, frac))
    if frac > 0 then GuiImage(hud, nid(), bar_x, y, color, 1, bar_w * frac / 2, h / 2) end
    if icon then GuiImage(hud, nid(), icon_x, y + h / 2 - 3, icon, 1, 0.75) end
  end
  local dm = EntityGetFirstComponentIncludingDisabled(player, "DamageModelComponent")
  if dm then
    local hp, max_hp = ComponentGetValue2(dm, "hp"), ComponentGetValue2(dm, "max_hp")
    bar(68, max_hp > 0 and hp / max_hp or 0, "data/ui_gfx/hud/colors_health_bar.png", 5, "data/ui_gfx/hud/health.png")
    GuiText(hud, text_x, 66, string.format("%d", math.floor(hp * 25 + 0.5)))
  end
  local cd = EntityGetFirstComponentIncludingDisabled(player, "CharacterDataComponent")
  if cd then
    local fly_max = ComponentGetValue2(cd, "fly_time_max")
    bar(78, fly_max > 0 and ComponentGetValue2(cd, "mFlyingTimeLeft") / fly_max or 0, "data/ui_gfx/hud/colors_flying_bar.png", 3, "data/ui_gfx/hud/jetpack.png")
  end
  local ability = active ~= 0 and EntityGetFirstComponentIncludingDisabled(active, "AbilityComponent")
  if ability and EntityHasTag(active, "wand") then
    local mana_max = ComponentGetValue2(ability, "mana_max")
    bar(86, mana_max > 0 and ComponentGetValue2(ability, "mana") / mana_max or 0, "data/ui_gfx/hud/colors_mana_bar.png", 3, "data/ui_gfx/hud/mana.png")
    local reload = ComponentObjectGetValue2(ability, "gun_config", "reload_time") or 0
    local left = ComponentGetValue2(ability, "mReloadNextFrameUsable") - GameGetFrameNum()
    if reload > 0 and left > 0 then bar(92, left / reload, "data/ui_gfx/hud/colors_reload_bar.png", 2, nil) end
  end
  local wallet = EntityGetFirstComponentIncludingDisabled(player, "WalletComponent")
  if wallet then
    GuiImage(hud, nid(), icon_x, 96, "data/ui_gfx/hud/money.png", 1, 0.75)
    GuiText(hud, text_x, 94, tostring(ComponentGetValue2(wallet, "money")))
  end
end

local function draw_editor(player)
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
  -- Wand rows below the quick-inventory bar; spell inventory in the bottom-right corner.
  local sw, sh = GuiGetScreenDimensions(gui)
  local step = 18
  local x0, y0 = 20, 84
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

function M.draw(player)
  if not player or not EntityGetIsAlive(player) then return end
  draw_hud(player)
  if M.open then draw_editor(player) end
end

return M
