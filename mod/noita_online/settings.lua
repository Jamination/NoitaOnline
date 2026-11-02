dofile("data/scripts/lib/mod_settings.lua")

local mod_id = "noita_online"
mod_settings_version = 1
mod_settings = {
  {
    id = "server",
    ui_name = "Server address",
    ui_description = "host:port of the Noita Online server (e.g. 203.0.113.5:41773). Empty = disabled.",
    value_default = "",
    text_max_length = 64,
    allowed_characters = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-:[]",
    scope = MOD_SETTING_SCOPE_NEW_GAME,
  },
  {
    id = "name",
    ui_name = "Player name",
    ui_description = "Name shown to other players.",
    value_default = "wizard",
    text_max_length = 32,
    allowed_characters = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-",
    scope = MOD_SETTING_SCOPE_NEW_GAME,
  },
}

function ModSettingsUpdate(init_scope)
  mod_settings_update(mod_id, mod_settings, init_scope)
end

function ModSettingsGuiCount()
  return mod_settings_gui_count(mod_id, mod_settings)
end

function ModSettingsGui(gui, in_main_menu)
  mod_settings_gui(mod_id, mod_settings, gui, in_main_menu)
end
