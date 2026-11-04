-- Next run: when every player is dead at the same time the game is over for everyone. Each
-- client shows vanilla's game over; its "New game" starts a new game in this process, whose
-- init.lua then joins the session's next run (a new world from the server's next seed, the
-- same players, everyone spawning together as at the session start). The client runtime
-- keeps its connection and patches and resets everything that belonged to the old world
-- (crates/client/src/nextrun.rs). A new game while the run goes on (pause menu) still leaves
-- the session.
local NO = dofile_once("mods/noita_online/files/native.lua")

local M = {}
M.log = print

-- Every player is dead at frame `frame`: call inside the simulation window, at the same
-- frame on every client. This client's next new game is the next run.
function M.all_dead(frame)
  if NO.lib.no_run_over_frame() >= 0 then return end
  NO.lib.no_run_over(frame)
  M.over_frame = frame
  M.log("run over: every player dead at frame " .. frame)
end

-- Whether the current run is over (every player died).
function M.over()
  return NO.lib.no_run_over_frame() >= 0
end

-- Scripted runs: press vanilla's "New game" as a player would (the game-over menu's button,
-- then the game-mode screen's button for the current mode). Outside the simulation window.
function M.press_new_game()
  local ok = NO.lib.no_press_new_game() == 1
  M.log("new game pressed at frame " .. GameGetFrameNum() .. (ok and "" or (" (refused: " .. NO.last_error() .. ")")))
  return ok
end

-- Test scenario "restart" (server --scenario restart; the bot scenario's players and world
-- events): in the first run ("restart<N>": in each of the first N runs) every player counts as
-- dead at RESTART_FRAME (a stand-in for the real all-dead path, players.lua), every client
-- shows the game over and its bot presses New game 120 + 180 * (player id) frames later, so
-- the clients leave the old world at different frames while the others keep simulating it.
M.RESTART_FRAME = 1200

-- The number of runs a restart scenario ends, nil for another scenario.
function M.restarts(scenario)
  local n = scenario:match("^restart(%d*)$")
  if not n then return nil end
  return tonumber(n) or 1
end

-- Inside the window (every client, in tick order).
function M.restart_events(f, run, restarts)
  if run < restarts and f == M.RESTART_FRAME then
    NO.lib.no_game_over_shared()
    M.all_dead(f)
  end
end

-- Outside the window (after it closed), this client's bot.
local game_over_shown, pressed = false, false
function M.restart_post(f, run, restarts, player)
  if run >= restarts or not M.over_frame then return end
  if not game_over_shown then
    game_over_shown = true
    GameTriggerGameOver()
  end
  if not pressed and f >= M.over_frame + 120 + 180 * player then
    pressed = true
    M.press_new_game()
  end
end

return M
