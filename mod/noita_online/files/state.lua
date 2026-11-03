-- Entity state digest for cross-client verification: every entity's id and exact transform
-- (except this client's inert input proxy), plus tracked positions of players and enemies.
local M = {}

local bit = require("bit")
local ffi = require("ffi")
local NO = dofile_once("mods/noita_online/files/native.lua")

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

-- The entities come from the client runtime (no_entity_snapshot: what EntityGetInRadius(0, 0,
-- 1e9), EntityGetTransform and EntityHasTag return, read natively in one call, sorted by id).
local recs, recs_cap = nil, 0

-- all_positions: also report every other entity (kind 2), for pinpointing divergences.
function M.collect(proxy, all_positions)
  local n = NO.lib.no_entity_snapshot(proxy or 0, recs, recs_cap)
  if n > recs_cap then
    recs_cap = n + 1024
    recs = ffi.new("no_entity_rec_t[?]", recs_cap)
    n = NO.lib.no_entity_snapshot(proxy or 0, recs, recs_cap)
  end
  local h = 0x811c9dc5
  local positions = {}
  for i = 0, n - 1 do
    local r = recs[i]
    local e = r.id
    h = mix(h, e)
    h = mix(h, fbits(r.x))
    h = mix(h, fbits(r.y))
    h = mix(h, fbits(r.rot))
    local kind = r.kind
    if kind == 0 then
      h = mix_inventory(h, e)
    elseif kind == 2 and not all_positions then
      kind = nil
    end
    if kind then
      positions[#positions + 1] = { id = e, kind = kind, x = r.x, y = r.y }
    end
  end
  -- Lua number in [0, 2^32) so FFI converts it to uint64 exactly.
  return (h % 0x100000000), positions
end

return M
