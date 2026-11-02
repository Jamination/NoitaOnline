-- Appended to data/scripts/lib/utilities.lua in a session (init.lua, ModLuaFileAppend).
-- Vanilla is_in_camera_bounds tests a point against the camera (GameGetCameraBounds). Its
-- users (gravity fields, the moon, physics fungi, swirling/scattering particles, portal
-- effects) only act near the camera, which inside the lockstep window is player 1's. The
-- client runtime answers for every player's view instead: GameGetCameraBounds called with
-- arguments is answered natively (crates/client/src/camreaders.rs, `lua_camera_bounds`),
-- near any player, as each player's own camera would; outside the window it is vanilla's test.
local vanilla_is_in_camera_bounds = is_in_camera_bounds

function is_in_camera_bounds(x, y, padding)
  local inside = GameGetCameraBounds(x, y, padding)
  if type(inside) == "boolean" then return inside end
  return vanilla_is_in_camera_bounds(x, y, padding) -- patch switched off (experiments)
end

-- The same test against the view of the player at (px, py) only (the radar perk marks the
-- enemies on its holder's screen).
function noita_online_in_camera_bounds_of(px, py, x, y, padding)
  local inside = GameGetCameraBounds(px, py, x, y, padding)
  if type(inside) == "boolean" then return inside end
  return vanilla_is_in_camera_bounds(x, y, padding)
end
