-- Runs a tempo-studio session script under plain Lua 5.4 against a stub of the REAPER API.
-- This checks the generated Lua parses, runs, takes its fallbacks and writes a valid status file.
-- It cannot check what real REAPER does with these calls (that needs REAPER).
--
--   lua5.4 tests/lua/reaper_stub.lua <session.lua> [full|minimal|nolimit|failing]
--
-- full     every API function present, all FX available
-- minimal  only the core track calls: every optional call must be skipped, nothing may crash
-- nolimit  ReaLimit is not installed and InsertMedia does nothing (media plan B must take over)
-- failing  GetTrack returns nil (the per-track pcall must catch it)

local script, mode = arg[1], arg[2] or "full"
assert(script, "usage: reaper_stub.lua <session.lua> [mode]")

local calls = {}
local function called(name) calls[#calls + 1] = name end

-- Parameter models: name, normalised -> value, value -> display text. All monotonic, all
-- non-linear, so the script has to search for the right normalised value.
local function hz_text(v) if v >= 1000 then return string.format("%.2fk", v / 1000) end return string.format("%.1f", v) end
local MODELS = {
  ReaEQ = {
    { "Freq-Low Shelf", function(n) return 20 * 1000 ^ n end, hz_text },
    { "Gain-Low Shelf", function(n) return -24 + 48 * n end, function(v) return string.format("%.1f", v) end },
  },
  ReaComp = {
    { "Thresh", function(n) return 20 * math.log(n + 1e-9, 10) end, function(v) return string.format("%.1f", v) end },
    { "Ratio", function(n) return 1 + 99 * n ^ 2 end, function(v) return string.format("%.1f:1", v) end },
    { "Attack", function(n) return 500 * n ^ 3 end, function(v) return string.format("%.1f", v) end },
    { "Release", function(n) return 5000 * n ^ 2 end, function(v) return string.format("%.0f", v) end },
  },
  ReaLimit = {
    { "Threshold", function(n) return -20 + 20 * n end, function(v) return string.format("%.1f", v) end },
    { "Brickwall ceiling", function(n) return -20 + 20 * n end, function(v) return string.format("%.1f dB", v) end },
  },
}

local project = { tracks = {}, info = {}, sinfo = {}, saved = nil, master = { name = "MASTER", fx = {} } }
local edit_cursor, last_touched = 0, nil

local function new_fx(kind)
  local fx = { kind = kind, norm = {}, enabled = true, config = {} }
  for i = 1, #MODELS[kind] do fx.norm[i] = 0.5 end
  return fx
end

local function file_exists(p)
  local f = io.open(p, "rb")
  if f then f:close() return true end
  return false
end

local R = {}
function R.GetAppVersion() called("GetAppVersion") return "7.22/stub" end
function R.Main_OnCommand(id, flag)
  called("Main_OnCommand:" .. id)
  if id == 40914 then
    for _, t in ipairs(project.tracks) do if t.selected then last_touched = t end end
  elseif id == 42230 then
    local dir, pat = project.sinfo.RENDER_FILE, project.sinfo.RENDER_PATTERN
    local f = io.open(dir .. "/" .. pat .. ".wav", "wb")
    if f then f:write("RIFF stub render") f:close() end
  end
end
function R.CountTracks(p) return #project.tracks end
function R.InsertTrackAtIndex(idx, defaults)
  called("InsertTrackAtIndex")
  table.insert(project.tracks, idx + 1, { name = "", vol = 1, items = {}, fx = {}, selected = false })
end
function R.GetTrack(p, idx)
  if mode == "failing" then return nil end
  return project.tracks[idx + 1]
end
function R.GetSetMediaTrackInfo_String(tr, key, val, set)
  assert(key == "P_NAME", "unexpected key " .. key)
  if set then tr.name = val end
  return true, tr.name
end
function R.SetMediaTrackInfo_Value(tr, key, v) assert(key == "D_VOL") tr.vol = v return true end
function R.SetOnlyTrackSelected(tr) for _, t in ipairs(project.tracks) do t.selected = (t == tr) end end
function R.SetEditCurPos(t, a, b) edit_cursor = t end
function R.InsertMedia(file, m)
  called("InsertMedia")
  if mode == "nolimit" then return 0 end -- does nothing: plan B must add the item
  assert(m == 0)
  if last_touched then last_touched.items[#last_touched.items + 1] = { file = file, pos = edit_cursor, vol = 1 } end
  return 1
end
function R.CountTrackMediaItems(tr) return #tr.items end
function R.GetTrackMediaItem(tr, i) return tr.items[i + 1] end
function R.SetMediaItemInfo_Value(item, key, v) item[key] = v return true end
function R.AddMediaItemToTrack(tr) local it = {} tr.items[#tr.items + 1] = it called("AddMediaItemToTrack") return it end
function R.AddTakeToMediaItem(item) local tk = {} item.take = tk return tk end
function R.PCM_Source_CreateFromFile(f) return { file = f } end
function R.SetMediaItemTake_Source(tk, src) tk.src = src return true end
function R.GetMediaSourceLength(src) return 3.0, false end
function R.TrackFX_AddByName(tr, name, rec, inst)
  called("TrackFX_AddByName:" .. name)
  local kind = name:match("^(Rea%a+)$")
  if not kind or not MODELS[kind] then return -1 end
  if mode == "nolimit" and kind == "ReaLimit" then return -1 end
  tr.fx[#tr.fx + 1] = new_fx(kind)
  return #tr.fx - 1
end
function R.TrackFX_GetNumParams(tr, fx) return #MODELS[tr.fx[fx + 1].kind] end
function R.TrackFX_GetParamName(tr, fx, p, buf) return true, MODELS[tr.fx[fx + 1].kind][p + 1][1] end
function R.TrackFX_SetParamNormalized(tr, fx, p, v) tr.fx[fx + 1].norm[p + 1] = v return true end
function R.TrackFX_GetFormattedParamValue(tr, fx, p, buf)
  local f = tr.fx[fx + 1]
  local m = MODELS[f.kind][p + 1]
  return true, m[3](m[2](f.norm[p + 1]))
end
function R.TrackFX_SetNamedConfigParm(tr, fx, key, val) tr.fx[fx + 1].config[key] = val return true end
function R.TrackFX_SetEnabled(tr, fx, on) tr.fx[fx + 1].enabled = on end
function R.GetMasterTrack(p) return project.master end
function R.GetSetProjectInfo_String(p, key, val, set)
  if set then project.sinfo[key] = val end
  return true, project.sinfo[key] or ""
end
function R.GetSetProjectInfo(p, key, val, set) if set then project.info[key] = val end return project.info[key] or 0 end
function R.Main_SaveProjectEx(p, path, opt)
  local f = assert(io.open(path, "w"))
  f:write("<REAPER_PROJECT 0.1 \"7.22/stub\" 1\n>\n")
  f:close()
  project.saved = path
end
function R.file_exists(p) return file_exists(p) end
function R.PreventUIRefresh() end
function R.Undo_BeginBlock() end
function R.Undo_EndBlock() end
function R.TrackList_AdjustWindows() end
function R.UpdateArrange() end

if mode == "minimal" then
  reaper = {
    InsertTrackAtIndex = R.InsertTrackAtIndex, GetTrack = R.GetTrack,
    GetSetMediaTrackInfo_String = R.GetSetMediaTrackInfo_String, SetMediaTrackInfo_Value = R.SetMediaTrackInfo_Value,
  }
else
  reaper = R
end

-- Find the status path the script will write (it is in the tag line).
local status_path
for line in io.lines(script) do
  local j = line:match("^%-%- tempo%-studio%-session: (.*)$")
  if j then status_path = j:match('"status":"(.-)"') break end
end
assert(status_path, "no tempo-studio-session tag line")
status_path = status_path:gsub("\\\\", "\\")

dofile(script)

local f = assert(io.open(status_path, "r"), "the script wrote no status file at " .. status_path)
local status = f:read("a")
f:close()
print("status: " .. status)

-- Checks that hold in every mode.
assert(status:find('"done":true', 1, true), "status not done")
print(string.format("mode=%s tracks=%d", mode, #project.tracks))

if mode == "full" then
  assert(status:find('"ok":true', 1, true), "not ok")
  assert(project.saved, "project not saved")
  for _, t in ipairs(project.tracks) do
    assert(#t.items == 1, "track " .. t.name .. " has no media")
    for _, fx in ipairs(t.fx) do
      assert(fx.enabled, t.name .. ": " .. fx.kind .. " was bypassed")
      local m = MODELS[fx.kind]
      for i, prm in ipairs(m) do print(string.format("  %s %s %s = %s", t.name, fx.kind, prm[1], prm[3](prm[2](fx.norm[i])))) end
      if fx.kind == "ReaEQ" then assert(fx.config.BANDTYPE0 == "4", "ReaEQ band not high-pass") end
    end
  end
  assert(#project.master.fx == 1 and project.master.fx[1].kind == "ReaLimit", "no ReaLimit on master")
  local lim = project.master.fx[1]
  print(string.format("  master ReaLimit ceiling = %s", MODELS.ReaLimit[2][3](MODELS.ReaLimit[2][2](lim.norm[2]))))
  assert(project.sinfo.RENDER_FORMAT == "ZXZhdxgAAQ==", "render format not WAV 24")
  assert(project.info.RENDER_BOUNDSFLAG == 1 and project.info.RENDER_SETTINGS == 0, "render bounds/settings")
  print("  RENDER_FILE=" .. project.sinfo.RENDER_FILE .. " RENDER_PATTERN=" .. project.sinfo.RENDER_PATTERN)
  print("  RENDER_SRATE=" .. tostring(project.info.RENDER_SRATE) .. " PROJECT_SRATE=" .. tostring(project.info.PROJECT_SRATE))
elseif mode == "minimal" then
  assert(status:find('"ok":true', 1, true), "the script must survive a bare API")
  assert(#project.tracks > 0, "tracks still made")
  assert(status:find("TrackFX_AddByName is not available", 1, true) or not status:find('"fx_missing":%[%]'), "FX gaps not reported")
elseif mode == "nolimit" then
  assert(status:find('"ReaLimit"', 1, true), "missing ReaLimit not reported in fx_missing")
  for _, t in ipairs(project.tracks) do assert(#t.items == 1, "plan B media insert did not run for " .. t.name) end
  local plan_b = 0
  for _, c in ipairs(calls) do if c == "AddMediaItemToTrack" then plan_b = plan_b + 1 end end
  assert(plan_b == #project.tracks, "AddMediaItemToTrack not used")
elseif mode == "failing" then
  assert(status:find("GetTrack returned no track", 1, true), "track failure not reported")
end
print("stub run OK (" .. mode .. ")")
