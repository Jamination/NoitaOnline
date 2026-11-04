-- Damage log of a player slot (LuaComponent script_damage_received on every slot, every
-- client, players.lua). It only records the damage in a world global, identically on every
-- client; the mod drains it at the end of the simulation window and writes it to the client
-- log when NOITA_ONLINE_DEBUG=1 (init.lua). One line per damage:
--   frame entity amount(HP) message responsible-file projectile-file fatal hp-at-callback(HP)
local KEY = "noita_online_damage"

function damage_received(damage, message, entity_thats_responsible, is_fatal, projectile_thats_responsible)
  local me = GetUpdatedEntityID()
  local dm = EntityGetFirstComponentIncludingDisabled(me, "DamageModelComponent")
  local hp = dm and ComponentGetValue2(dm, "hp") or 0
  local function file(e)
    if e == nil or e == 0 or not EntityGetIsAlive(e) then return "-" end
    local f = EntityGetFilename(e)
    return (f ~= nil and f ~= "") and f or ("#" .. e)
  end
  local line = string.format("%d %d %.3f %s %s %s %s %.3f", GameGetFrameNum(), me, damage * 25,
    (message ~= nil and message ~= "") and message:gsub("%s", "_") or "-",
    file(entity_thats_responsible), file(projectile_thats_responsible), tostring(is_fatal), hp * 25)
  GlobalsSetValue(KEY, GlobalsGetValue(KEY, "") .. line .. "\n")
end
