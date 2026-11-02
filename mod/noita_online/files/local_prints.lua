-- Player-scoped messages of vanilla Lua scripts, shown only to the affected player.
--
-- In vanilla the one player's pickups, perks, hearts, orbs, kantele/ocarina songs, fungal
-- shifts, etc. print a message (GamePrint / GamePrintImportant) for that player. In a session
-- every client runs those callbacks for every player (the simulation in them must run
-- everywhere), so every client printed every player's messages. The print statements are
-- wrapped (text patch of the vanilla script, same line, so line numbers stay) in a test of
-- the script's own subject entity against this client's player: the entity id kept in the
-- mod setting `noita_online.local_entity` (players.lua; 0 = none: spectating). Nothing else
-- in the scripts changes, so the simulation is the same on every client; the prints change
-- nothing in it. Outside a session the setting is "" and every message shows (vanilla).
local M = {}

M.SETTING = "noita_online.local_entity"

-- Prepended to line 1 of each patched script (no newline: line numbers stay vanilla's).
-- __no_local(e): e is this client's player. __no_among(list): this client's player is in
-- the list (vanilla's "a player is near" lists). __no_near_m(x, y, r): this client's player
-- is within Manhattan distance r (vanilla worm deflector test).
local HELPER = table.concat({
  "local function __no_me() return ModSettingGet(\"" .. M.SETTING .. "\") end",
  "local function __no_local(e) local s = __no_me() return s == nil or s == \"\" or s == e end",
  "local function __no_among(list) local s = __no_me() if s == nil or s == \"\" then return true end",
  " for _, p in ipairs(list or {}) do if p == s then return true end end return false end",
  "local function __no_near_m(x, y, r) local s = __no_me() if s == nil or s == \"\" then return true end",
  " if s == 0 or not EntityGetIsAlive(s) then return false end local px, py = EntityGetTransform(s)",
  " return math.abs(px - x) + math.abs(py - y) < r end ",
}, " ")

-- { file, { { unique text of the statement line, condition, expected count }, ... } }.
-- The statement is the whole line (apart from indentation); it becomes
-- `if <condition> then <statement> end`.
local SITES = {
  { "data/scripts/animals/longleg_pet.lua", {
    { 'GamePrint( "$ui_longleg_love_msg1" )', "__no_local(entity_who_interacted)" },
    { 'GamePrint( "$ui_longleg_love_msg2" )', "__no_local(entity_who_interacted)" },
  } },
  { "data/scripts/buildings/chest_steel.lua", { { 'GamePrintImportant( "$log_chest"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/buildings/endcrystal.lua", { { 'GamePrintImportant( "$log_endcrystal"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/essences/essence_pickup.lua", { { 'GamePrintImportant( GameTextGet( "$log_pickedup_perk"', "__no_local(entity_who_picked)" } } },
  -- death() of the essence remover: vanilla prints when the player lost essences
  { "data/scripts/essences/away.lua", {
    { "local doit = false", nil, 1, "local doit = false local __no_mine = false" },
    { "doit = true", nil, 1, "doit = true if __no_local(v) then __no_mine = true end" },
    { 'GamePrintImportant( "$log_curse_secret"', "__no_mine" },
  } },
  { "data/scripts/items/chest_random.lua", { { 'GamePrintImportant( "$log_chest"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/items/chest_random_super.lua", { { 'GamePrintImportant( "$log_chest"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/items/utility_box.lua", { { 'GamePrintImportant( "$log_chest"', "__no_local(entity_who_picked)" } } },
  { "data/biome_impl/static_tile/chest_darkness.lua", { { 'GamePrintImportant( "$log_chest"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/items/coin.lua", { { "GamePrint( money )", "__no_local(entity_who_picked)" } } },
  { "data/scripts/items/heart.lua", { { 'GamePrintImportant( "$log_heart"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/items/heart_better.lua", { { 'GamePrintImportant( "$log_heart_better"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/items/heart_evil.lua", { { 'GamePrintImportant( "$log_heart"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/items/heart_fullhp.lua", { { 'GamePrintImportant( "$log_heart_fullhp"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/items/heart_fullhp_temple.lua", { { 'GamePrintImportant( "$log_heart_fullhp_temple"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/items/orb_pickup.lua", {
    { 'GameGiveAchievement( "ALL_ORBS" )', "__no_local(entity_who_picked)" },
    { "GamePrintImportant( message_title, message_desc )", "__no_local(entity_who_picked)" },
  } },
  { "data/scripts/items/spell_refresh.lua", { { 'GamePrintImportant( "$itemtitle_spell_refresh"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/magic/greed_curse/greed_curse_pickup.lua", { { 'GamePrintImportant( "$log_curse"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/particles/wand_pickup.lua", { { 'GamePrintImportant( "$itemtitle_wand_pickup"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/perks/perk.lua", { { 'GamePrintImportant( GameTextGet( "$log_pickedup_perk"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/perks/perk_list.lua", { { 'GamePrintImportant( "$log_always_cast_failed"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/perks/adventurer.lua", { { 'GamePrint( "$log_adventurer" )', "__no_local(player_id)" } } },
  { "data/entities/animals/boss_centipede/rewards/gold_reward.lua", { { 'GamePrintImportant( "$log_golden_statue"', "__no_local(entity_who_picked)" } } },
  { "data/scripts/magic/kantele.lua", {
    { 'GamePrintImportant( "$log_kantele"', "__no_local(player_id)", 3 },
    { 'GamePrint( "$log_kantele_done" )', "__no_local(player_id)", 3 },
    { 'GamePrintImportant( "$log_alchemist_key_first"', "__no_local(player_id)" },
    { 'GamePrintImportant( "$log_alchemist_key_second"', "__no_local(player_id)" },
    { 'GamePrint( "$log_alchemist_key_invalid" )', "__no_local(player_id)" },
  } },
  { "data/scripts/magic/ocarina.lua", {
    { 'GamePrintImportant( "$log_ocarina"', "__no_local(player_id)", 3 },
    { 'GamePrint( "$log_ocarina_done" )', "__no_local(player_id)", 3 },
    { 'GamePrintImportant( "$log_alchemist_key_first"', "__no_local(player_id)" },
    { 'GamePrintImportant( "$log_alchemist_key_second"', "__no_local(player_id)" },
    { 'GamePrint( "$log_alchemist_key_invalid" )', "__no_local(player_id)" },
  } },
  { "data/scripts/magic/fungal_shift.lua", {
    { "GamePrint( log_msg )", "__no_local(entity)" },
    { "GamePrintImportant( random_from_array( log_messages )", "__no_local(entity)" },
  } },
  -- one message per player in the loop; vanilla's one player saw one
  { "data/scripts/projectiles/all_spells_launch_slow.lua", { { 'GamePrintImportant( "$log_all_spells"', "__no_local(v)" } } },
  { "data/scripts/magic/null_room/check.lua", { { 'GamePrintImportant( message, "" )', "__no_local(entity_id)" } } },
  -- per-player curse / perk entities (children of the player)
  { "data/scripts/magic/greed_curse/check_biome.lua", {
    { 'GamePrintImportant( "$log_greed_curse_away_short"', "__no_local(EntityGetRootEntity(entity_id))" },
    { 'GamePrint( "$log_greed_curse_return" )', "__no_local(EntityGetRootEntity(entity_id))" },
  } },
  { "data/scripts/perks/homunculus_check_biome.lua", { { 'GamePrint( "$log_homunculus" )', "__no_local(EntityGetRootEntity(entity_id))", 2 } } },
  { "data/entities/animals/boss_wizard/debuff.lua", { { 'GamePrint( "$boss_wizard_"', "__no_local(root_entity)" } } },
  -- "a player is near": this client's player is near
  { "data/scripts/buildings/worm_deflector_death.lua", { { 'GamePrintImportant( "$log_worm_deflector_death"', "__no_near_m(x, y, 240)" } } },
  { "data/scripts/items/die_roll.lua", { { 'GamePrint( "$item_die_roll" )', "__no_among(players)" } } },
  { "data/scripts/items/die_status.lua", { { "GamePrint( textprint )", "__no_among(players)", 2 } } },
  { "data/scripts/items/greed_die_status.lua", { { "GamePrint( textprint )", "__no_among(players)", 2 } } },
}

-- Rewrite the lines of `text` whose statement starts with `find`: wrapped in `if cond`, or
-- replaced by `repl`. Returns the new text and the number of lines changed.
local function patch_lines(text, find, cond, repl)
  local out, n = {}, 0
  for line in (text .. "\n"):gmatch("(.-)\r?\n") do
    local indent, stmt = line:match("^(%s*)(.-)%s*$")
    if stmt:sub(1, #find) == find and (repl or stmt:sub(-1) == ")") then
      n = n + 1
      if repl then
        line = indent .. repl .. stmt:sub(#find + 1)
      else
        line = indent .. "if " .. cond .. " then " .. stmt .. " end"
      end
    end
    out[#out + 1] = line
  end
  return table.concat(out, "\n"), n
end

-- Patch every site (mod init, before any script runs). Reports sites not found as expected
-- (another mod changed the script, or a different game build).
function M.install(log)
  for _, file in ipairs(SITES) do
    local path, sites = file[1], file[2]
    local text = ModTextFileGetContent(path)
    if not text or text == "" then
      log("local_prints: " .. path .. " not found")
    else
      local ok = true
      for _, s in ipairs(sites) do
        local find, cond, expected, repl = s[1], s[2], s[3] or 1, s[4]
        local n
        text, n = patch_lines(text, find, cond, repl)
        if n ~= expected then
          ok = false
          log(string.format("local_prints: %s: %d of %d lines of '%s'", path, n, expected, find))
        end
      end
      if ok then ModTextFileSetContent(path, HELPER .. text) end
    end
  end
end

-- This client's player for the patched scripts: entity id, 0 = none (spectating), nil =
-- every player (no session).
function M.set_local(entity)
  ModSettingSet(M.SETTING, entity or "")
end

return M
