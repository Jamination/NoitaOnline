-- Respawn picker: opened by the game-over menu's respawn button (vanilla's "New game",
-- crates/client/src/respawn.rs) while this client's player is dead and others live. Lists
-- the living players by name in their robe colour; a click chooses where to respawn.
-- Presentation only (outside the simulation window); the choice travels as a lockstep op.
local M = {}

local gui = nil
local shown = false

local function rgb(c)
  return math.floor(c / 65536) % 256 / 255, math.floor(c / 256) % 256 / 255, c % 256 / 255
end

-- choices: { {id=, name=, colour=RGB}, ... } to show the picker, nil to hide it. Returns the
-- chosen player id, false if the picker was closed, nil otherwise.
function M.update(choices)
  if not choices then
    if shown then
      GuiStartFrame(gui)
      shown = false
    end
    return nil
  end
  gui = gui or GuiCreate()
  GuiStartFrame(gui)
  shown = true
  local w, h = GuiGetScreenDimensions(gui)
  -- Left of the game-over screen's title and stats panel, below the inventory bar.
  local x, y = w * 0.03, h * 0.3
  local result = nil
  GuiZSetForNextWidget(gui, -100)
  GuiText(gui, x, y, "Respawn at:")
  for i, c in ipairs(choices) do
    local r, g, b = rgb(c.colour)
    GuiColorSetForNextWidget(gui, r, g, b, 1)
    GuiZSetForNextWidget(gui, -100)
    if GuiButton(gui, 100 + i, x + 10, y + 4 + 12 * i, "> " .. c.name) then result = c.id end
  end
  GuiZSetForNextWidget(gui, -100)
  if GuiButton(gui, 99, x, y + 12 * (#choices + 1) + 8, "Cancel") then result = false end
  return result
end

return M
