-- Robe colours: every session player slot wears its own robe colour, the same on every client.
--
-- The first slot is vanilla's player (data/entities/player.xml, purple robe). Every other slot
-- k gets a copy of the player entity whose robe images are recoloured at mod init:
--   * sprites (data/enemies_gfx/): the body (player.xml), its Lukki-mutation variant
--     (player_lukky.xml), the arm (player_arm.xml) and the unlockable hat's sprites
--     (player_hat2.xml, player_hat2_shadow.xml, which draw robe-coloured pixels);
--   * the death ragdoll's part images (data/ragdolls/player/);
--   * the cape (VerletPhysicsComponent cloth_color / cloth_color_edge, an override inside the
--     cape's Base like vanilla's crypt wizards use).
-- Only pixels of the robe's three purple shades (and the arm's anti-aliased near-copies of
-- them) change; skin, belt, eyes, hood shadow, outlines and alpha stay. The copies are virtual
-- files (ModImageMakeEditable / ModTextFileSetContent) under mods/noita_online/robes/<k>/,
-- referenced only by slot k's entity file; the sprite xmls are vanilla's with the image path
-- swapped, so animations, hotspots and offsets stay vanilla's.
--
-- Visual only: the images keep vanilla's alpha (sprite shapes, ragdoll body shapes) and every
-- copied sprite keeps vanilla's stain UV map (data/generated/sprite_uv_maps/, looked up by the
-- image path), so stains, ragdoll physics and the cape's physics are vanilla's. All clients run
-- the same mod, so they build the same files.
local bit = require("bit")

local M = {}

-- Vanilla's robe shades (RGB): light, mid (also the cape), dark.
local VANILLA = { 0x9b6f9a, 0x7f5476, 0x594354 }
-- A pixel belongs to a shade when every channel is within this distance of it (the arm's
-- anti-aliased pixels are off by up to 6; the Lukki legs' reds are 18+ away).
local NEAR = 12

-- Robe shades of slots 2.. (light, mid, dark); slot 1 is vanilla's purple. Slots beyond the
-- palette repeat it from the start.
M.PALETTE = {
  { name = "red",    0xd04848, 0xa03434, 0x6c2626 },
  { name = "green",  0x5ab44a, 0x3f8a36, 0x2b5e28 },
  { name = "blue",   0x5480dc, 0x3a5aae, 0x283e78 },
  { name = "yellow", 0xe6d040, 0xb4a02a, 0x786a1e },
  { name = "orange", 0xe88030, 0xb45e20, 0x7a4018 },
  { name = "cyan",   0x48d0d0, 0x2e9e9e, 0x206a6a },
  { name = "pink",   0xec5cbc, 0xb84290, 0x7c2c62 },
  { name = "white",  0xeaeaea, 0xb8b8c2, 0x7c7c8a },
}

local SPRITES = { "player", "player_lukky", "player_arm", "player_hat2", "player_hat2_shadow" }
local RAGDOLL_LIST = "data/ragdolls/player/filenames.txt"
local BASE = "data/entities/player_base.xml"
local PLAYER = "data/entities/player.xml"
local CAPE = '<Base file="data/entities/verlet_chains/cape/cape.xml">'

local files = {}   -- slot index (1-based) -> entity file

-- Entity file for the slot at (1-based) position `index` in the session's player list.
function M.entity_file(index)
  return files[index] or PLAYER
end

local function rgb(c) return bit.rshift(c, 16) % 256, bit.rshift(c, 8) % 256, c % 256 end

-- ABGR pixel with RGB `c` and alpha `a`, as a non-negative number.
local function abgr(a, r, g, b) return ((a * 256 + b) * 256 + g) * 256 + r end

-- The non-empty pixels of an image: { x, y, a, r, g, b, ... } (flat), plus its size.
local function read_pixels(path)
  local id, w, h = ModImageMakeEditable(path, 0, 0)
  if id == 0 or w == 0 then return nil end
  local px = {}
  for y = 0, h - 1 do
    for x = 0, w - 1 do
      local c = ModImageGetPixel(id, x, y)
      if c ~= 0 then
        local n = #px
        px[n + 1], px[n + 2] = x, y
        px[n + 3], px[n + 4], px[n + 5], px[n + 6] = bit.rshift(c, 24) % 256, c % 256, bit.rshift(c, 8) % 256, bit.rshift(c, 16) % 256
      end
    end
  end
  return { px = px, w = w, h = h }
end

local function write_pixels(path, img, map)
  local id = ModImageMakeEditable(path, img.w, img.h)
  if id == 0 then return false end
  local px = img.px
  for i = 1, #px, 6 do
    local r, g, b = px[i + 3], px[i + 4], px[i + 5]
    if map then r, g, b = map(r, g, b) end
    ModImageSetPixel(id, px[i], px[i + 1], abgr(px[i + 2], r, g, b))
  end
  return true
end

local function clamp(v) return v < 0 and 0 or (v > 255 and 255 or v) end

-- Pixel recolouring for the shades `to` (3 RGB values): a robe pixel moves by the offset of
-- its vanilla shade to the new shade; other pixels stay.
local function robe_map(to)
  local from, dst = {}, {}
  for i = 1, 3 do from[i] = { rgb(VANILLA[i]) } dst[i] = { rgb(to[i]) } end
  return function(r, g, b)
    for i = 1, 3 do
      local f = from[i]
      if math.abs(r - f[1]) <= NEAR and math.abs(g - f[2]) <= NEAR and math.abs(b - f[3]) <= NEAR then
        local d = dst[i]
        return clamp(d[1] + r - f[1]), clamp(d[2] + g - f[2]), clamp(d[3] + b - f[3])
      end
    end
    return r, g, b
  end
end

local function uv_map(png) return "data/generated/sprite_uv_maps/" .. png:gsub("/", ".") end

-- Replace exactly one occurrence of the plain text `old` in `text`.
local function replace_once(text, old, new, what, log)
  local i, j = text:find(old, 1, true)
  if not i or text:find(old, j + 1, true) then
    if log then log("robes: " .. what .. ": expected one '" .. old .. "'") end
    return text, false
  end
  return text:sub(1, i - 1) .. new .. text:sub(j + 1), true
end

-- Build the files of slots 2..count. Must run during mod init (init.lua's top level).
function M.install(count, log)
  files = {}
  if count < 2 then return end
  local t0 = os.clock()
  local sources, ragdoll_parts = {}, {}
  for _, s in ipairs(SPRITES) do
    local xml = "data/enemies_gfx/" .. s .. ".xml"
    local text = ModTextFileGetContent(xml)
    local png = text and text:match('%sfilename="([^"]+)"')
    local img = png and read_pixels(png)
    if not img then
      if log then log("robes: cannot read " .. xml) end
      return
    end
    local uv = ModImageDoesExist(uv_map(png)) and read_pixels(uv_map(png)) or nil
    sources[#sources + 1] = { xml = xml, text = text, png = png, img = img, uv = uv, name = s }
  end
  for line in (ModTextFileGetContent(RAGDOLL_LIST) or ""):gmatch("[^\r\n]+") do
    local path = line:match("^%s*(.-)%s*$")
    if path ~= "" then
      local img = read_pixels(path)
      if not img then
        if log then log("robes: cannot read " .. path) end
        return
      end
      ragdoll_parts[#ragdoll_parts + 1] = { path = path, img = img }
    end
  end
  local base_text = ModTextFileGetContent(BASE)
  local player_text = ModTextFileGetContent(PLAYER)

  for index = 2, count do
    local colour = M.PALETTE[(index - 2) % #M.PALETTE + 1]
    local map = robe_map(colour)
    local dir = "mods/noita_online/robes/" .. index .. "/"
    local base, ok, all = base_text, true, true
    for _, s in ipairs(sources) do
      local png = dir .. "gfx/" .. s.name .. ".png"
      write_pixels(png, s.img, map)
      if s.uv then write_pixels(uv_map(png), s.uv, nil) end
      local xml = dir .. "gfx/" .. s.name .. ".xml"
      local text = replace_once(s.text, 'filename="' .. s.png .. '"', 'filename="' .. png .. '"', s.xml, log)
      ModTextFileSetContent(xml, text)
      base, ok = replace_once(base, 'image_file="' .. s.xml .. '"', 'image_file="' .. xml .. '"', BASE, log)
      all = all and ok
    end
    local list = {}
    for _, part in ipairs(ragdoll_parts) do
      local png = dir .. "ragdoll/" .. part.path:match("[^/]+$")
      write_pixels(png, part.img, map)
      list[#list + 1] = png
    end
    ModTextFileSetContent(dir .. "ragdoll.txt", table.concat(list, "\n") .. "\n")
    base, ok = replace_once(base, 'ragdoll_filenames_file="' .. RAGDOLL_LIST .. '"', 'ragdoll_filenames_file="' .. dir .. 'ragdoll.txt"', BASE, log)
    all = all and ok
    base, ok = replace_once(base, CAPE, string.format(
      '%s<VerletPhysicsComponent cloth_color="0xFF%06X" cloth_color_edge="0xFF%06X"></VerletPhysicsComponent>',
      CAPE, colour[2], colour[1]), BASE, log)
    all = all and ok
    ModTextFileSetContent(dir .. "player_base.xml", base)
    local player = replace_once(player_text, '"' .. BASE .. '"', '"' .. dir .. 'player_base.xml"', PLAYER, log)
    ModTextFileSetContent(dir .. "player.xml", player)
    files[index] = dir .. "player.xml"
    if log and not all then log("robes: slot " .. index .. " (" .. colour.name .. ") only partly recoloured") end
  end
  if log then log(string.format("robes: %d slot colours built in %.2f s", count - 1, os.clock() - t0)) end
end

return M
