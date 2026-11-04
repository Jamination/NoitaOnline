-- Test scenario "progress" (server --scenario progress): the bot scenario plus checks of the
-- session's persistent progress (the host's save, crates/client/src/progress.rs), inside the
-- simulation window on every client. Each step logs what the simulation sees ("progress
-- f=..." lines in client_<id>.log):
--   f=300  the host's unlocks, death_count and all-time orbs, as the world generation saw them;
--   f=600  AddFlagPersistent of a flag no save has (it must land in every player's own save);
--   f=900  apparitions next to the first player (SpawnApparition): spawned at once (it carries
--          a random one of the host's bones wands), and biome_scripts.lua's way (spawns once
--          the save has enough bones, drawing the RNG on the bones count).
local M = {}

M.EARNED_FLAG = "noita_online_test_earned"

-- The entity's descendants: name and whether a wand, depth-first.
local function names(e, out)
  out = out or {}
  for _, c in ipairs(EntityGetAllChildren(e) or {}) do
    out[#out + 1] = EntityGetName(c) .. (EntityHasTag(c, "wand") and "(wand)" or "")
    names(c, out)
  end
  return table.concat(out, ",")
end

function M.events(f, players, log)
  if f == 300 then
    log(string.format("progress f=%d card_unlocked_nuke=%s intro_has_played=%s %s=%s death_count=%s orbs_all_time=%d",
      f, tostring(HasFlagPersistent("card_unlocked_nuke")), tostring(HasFlagPersistent("intro_has_played")),
      M.EARNED_FLAG, tostring(HasFlagPersistent(M.EARNED_FLAG)), tostring(StatsGlobalGetValue("death_count")),
      GameGetOrbCountAllTime()))
  elseif f == 600 then
    local new = AddFlagPersistent(M.EARNED_FLAG)
    log(string.format("progress f=%d AddFlagPersistent(%s) new=%s has=%s", f, M.EARNED_FLAG, tostring(new),
      tostring(HasFlagPersistent(M.EARNED_FLAG))))
  elseif f == 900 then
    local p = players[1]
    if p and p.entity and EntityGetIsAlive(p.entity) then
      local x, y = EntityGetTransform(p.entity)
      local state, e = SpawnApparition(x + 80, y - 30, 0, true)
      log(string.format("progress f=%d SpawnApparition(now) state=%s entity=%s descendants=%s", f, tostring(state),
        tostring(e), (e and e ~= 0) and names(e) or ""))
      for i = 1, 3 do
        state, e = SpawnApparition(x - 80, y - 30, i)
        log(string.format("progress f=%d SpawnApparition(level %d) state=%s entity=%s", f, i, tostring(state), tostring(e)))
      end
    end
  end
end

return M
