-- Entity state digest for cross-client verification: every entity's id and exact transform
-- (except this client's inert input proxy), plus tracked positions of players and enemies.
local M = {}

local bit = require("bit")
local ffi = require("ffi")

-- Exact f32 bits of a Lua number (Noita transforms are f32).
local f2u = ffi.new("union { float f; uint32_t u; }")
local function fbits(v)
  f2u.f = v
  return f2u.u
end

local function mix(h, v)
  h = bit.bxor(h, v)
  h = bit.tobit(h * 0x01000193)
  return bit.bxor(h, bit.rshift(h, 15))
end

-- Inventory of a player: active item, and every item in it (quick bar, spell inventory,
-- cards in wands) with its container and slot. Item moves inside the inventory change no
-- transform, so the transform digest alone would not see them.
local function mix_inventory(h, e)
  local inv = EntityGetFirstComponentIncludingDisabled(e, "Inventory2Component")
  if inv then h = mix(h, ComponentGetValue2(inv, "mActiveItem")) end
  local function item(it)
    local ic = EntityGetFirstComponentIncludingDisabled(it, "ItemComponent")
    h = mix(h, it)
    h = mix(h, EntityGetParent(it))
    if ic then
      local x, y = ComponentGetValue2(ic, "inventory_slot")
      h = mix(h, x)
      h = mix(h, y)
      h = mix(h, ComponentGetValue2(ic, "uses_remaining"))
    end
  end
  for _, c in ipairs(EntityGetAllChildren(e) or {}) do
    local name = EntityGetName(c)
    if name == "inventory_quick" or name == "inventory_full" then
      for _, it in ipairs(EntityGetAllChildren(c) or {}) do
        item(it)
        for _, card in ipairs(EntityGetAllChildren(it) or {}) do item(card) end
      end
    end
  end
  return h
end

-- all_positions: also report every other entity (kind 2), for pinpointing divergences.
function M.collect(proxy, all_positions)
  local ents = EntityGetInRadius(0, 0, 1e9)
  table.sort(ents)
  local h = 0x811c9dc5
  local positions = {}
  for _, e in ipairs(ents) do
    if e ~= proxy then
      local x, y, rot = EntityGetTransform(e)
      h = mix(h, e)
      h = mix(h, fbits(x))
      h = mix(h, fbits(y))
      h = mix(h, fbits(rot or 0))
      local kind = nil
      if EntityHasTag(e, "noita_online_player") then
        kind = 0
        h = mix_inventory(h, e)
      elseif EntityHasTag(e, "enemy") then
        kind = 1
      elseif all_positions then
        kind = 2
      end
      if kind then
        positions[#positions + 1] = { id = e, kind = kind, x = x, y = y }
      end
    end
  end
  -- Lua number in [0, 2^32) so FFI converts it to uint64 exactly.
  return (h % 0x100000000), positions, #ents
end

return M
