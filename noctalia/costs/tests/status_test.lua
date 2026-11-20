-- Run from the repository root: lua noctalia/costs/tests/status_test.lua
local plugin_dir = arg[1] or "noctalia/costs"

local function harness(entry, options)
  options = options or {}
  local h = { calls = {}, argvs = {}, labels = {}, terminals = {}, state_store = {}, modes = {} }
  local noop = function() end
  local env = setmetatable({}, { __index = _G })
  env.panel = { render = function(tree) h.tree = tree end, close = function() h.closed = true end }
  env.ui = setmetatable({}, { __index = function(_, kind)
    return function(props, children)
      if kind == "label" then table.insert(h.labels, props.text) end
      return { kind = kind, props = props, children = children }
    end
  end })
  env.barWidget = { setGlyph = noop, setText = noop, setTooltip = noop, render = noop, clearTooltip = noop }
  env.noctalia = {
    getConfig = function(key) return key == "costs_bin" and "costs" or 5 end,
    setUpdateInterval = noop,
    state = {
      get = function(key) return h.state_store[key] end,
      set = function(key, value) h.state_store[key] = value end,
    },
    runInTerminal = function(cmd) table.insert(h.terminals, cmd) return true end,
    pluginDataDir = function() return "/tmp/costs-test-data" end,
    writeFile = function(path, content)
      h.written = { path = path, content = content }
      return true
    end,
    json = {
      encode = function(list)
        local parts = {}
        for _, v in ipairs(list) do parts[#parts + 1] = '"' .. tostring(v) .. '"' end
        return "[" .. table.concat(parts, ",") .. "]"
      end,
      decode = function(text)
        if type(text) == "string" and text:sub(1, 1) == "[" then
          local list = {}
          for id in text:gmatch('"([^"]+)"') do table.insert(list, id) end
          return list
        end
        local out = { providers = options.providers or {}, use_order = options.use_order }
        if type(options.snapshot) == "table" then
          for k, v in pairs(options.snapshot) do out[k] = v end
        end
        return out
      end,
    },
    runAsync = function(argv, callback, timeout)
      table.insert(h.calls, argv[2])
      table.insert(h.argvs, argv)
      if argv[2] == "report" then
        callback({ exitCode = 0, stdout = "", stderr = "" })
        return true
      end
      if argv[1] == "/bin/sh" then
        -- onReport's delayed panel close (no-op command).
        callback({ exitCode = 0 })
        return true
      end
      if argv[2] == "quota" then
        assert(argv[3] == "--refresh" or argv[3] == "--if-stale", "quota mode: " .. tostring(argv[3]))
        table.insert(h.modes, argv[3])
        assert(timeout > 5000 and timeout <= 60000, "quota HTTP calls need an explicit budget")
        if options.reject_quota then return false end
        h.finish_quota = function()
          callback({ exitCode = options.quota_code or 0, stdout = "", stderr = "offline" })
        end
        if not options.defer then h.finish_quota() end
      else
        assert(argv[2] == "status")
        callback({ exitCode = options.status_code or 0, stdout = "{}", stderr = "boom" })
      end
      return true
    end,
  }
  env.require = function(path)
    return assert(loadfile(plugin_dir .. "/" .. path, "t", env))()
  end
  assert(loadfile(plugin_dir .. "/" .. entry .. ".luau", "t", env))()
  h.env = env
  h.update = entry == "widget" and env.update or env.onOpen
  return h
end

for _, entry in ipairs({ "widget", "panel" }) do
  local h = harness(entry, { defer = true })
  h.update()
  assert(h.calls[1] == "quota", entry .. " must refresh quota before reading status")
  assert(#h.calls == 1, "do not render the stale cache while refresh is pending")
  h.finish_quota()
  assert(table.concat(h.calls, ",") == "quota,status")
  print("PASS " .. entry .. " refreshes session windows before status")

  for _, options in ipairs({ { quota_code = 1 }, { reject_quota = true } }) do
    h = harness(entry, options)
    h.update()
    assert(table.concat(h.calls, ",") == "quota,status", "refresh failure must preserve cached display")
  end
  print("PASS " .. entry .. " falls back to cached data on refresh failure")
end
-- Timers and panel open respect the quota cache TTL; only the explicit button forces.
for _, entry in ipairs({ "widget", "panel" }) do
  local h = harness(entry)
  h.update()
  assert(h.modes[1] == "--if-stale", entry .. " automatic refresh must not force quota APIs")
end
do
  local h = harness("panel")
  h.update()
  h.env.onRefresh()
  assert(table.concat(h.modes, ",") == "--if-stale,--refresh", "refresh button forces: " .. table.concat(h.modes, ","))
  print("PASS automatic refresh uses --if-stale, the button forces --refresh")
end

local function tree_texts(node, out)
  out = out or {}
  if type(node) ~= "table" then return out end
  if node.kind == "label" and node.props then table.insert(out, node.props.text) end
  for _, child in ipairs(node.children or {}) do tree_texts(child, out) end
  return out
end
local function has_text(tree, needle)
  for _, t in ipairs(tree_texts(tree)) do
    if type(t) == "string" and t:find(needle, 1, true) then return true end
  end
  return false
end

do
  -- cached snapshot on open: a slow refresh shows "atualizando…" instead of wiping the panel
  local h = harness("panel", { defer = true })
  h.state_store.status_json = "{}"
  h.update()
  assert(not has_text(h.tree, "Carregando"), "cached data must stay visible while refreshing")
  assert(has_text(h.tree, "atualizando"), "refresh in progress is signalled in the footer")
  h.env.onRefresh()
  assert(#h.modes == 1, "no concurrent fetch while one is in flight")
  h.finish_quota()
  assert(not has_text(h.tree, "atualizando"))

  -- a failed refresh keeps the last good data and shows the error as a banner
  h = harness("panel", { status_code = 1 })
  h.state_store.status_json = "{}"
  h.update()
  assert(not has_text(h.tree, "Tentar de novo"), "failure must not replace good data")
  assert(has_text(h.tree, "falha ao atualizar: boom"), "failure is visible as a banner")
  print("PASS panel keeps cached data during refresh and after a failure")
end

local hint = "Ordem pela cota mensal"
local h = harness("panel", { providers = {{ id = "opencode-go", priority_hint = hint }} })
h.update()
local found = false
for _, text in ipairs(h.labels) do if text == hint then found = true end end
assert(found, "panel must show the ranking rationale from the shared policy")
print("PASS panel displays the ranking rationale")
print("8 session quota regression tests passed")

local agents = {}
for i = 1, 5 do agents[i] = { label = "machine-" .. i, usd = i, tokens = 1000000 } end
h = harness("panel", { providers = {
  { id = "claude", balance = "US$ 5", agents = agents, windows = {
    -- Fields emitted by status.rs: text is the shared pacing line;
    -- hint is an action recommendation, NOT elapsed time/projection.
    { label = "5h", used_pct = 72, reset = "2h30", color = "red",
      text = "🔴 5h   72% usado · 50% do tempo → 144% · ⏱ 2h30",
      hint = "acima do ritmo · pausa 1h44", wait = "1h44", spare_pct = 0 }
  } },
  { id = "codex", subscription_usd_month = 10, priority_hint = "Monthly allowance",
    balance = "US$ 9", error = "expired", error_text = "login expirado",
    resets = { available = 2, next_expiry = "3h" } },
} })
h.update()
local labels = table.concat(h.labels, "\n")
assert(labels:find("72%% usado"), "quota must explicitly label used percentage")
assert(labels:find("Renova em 2h30", 1, true), "reset is separate from pacing")

assert(labels:find("machine-5", 1, true), "never silently truncate machine details")
assert(labels:find("Saldo", 1, true) and labels:find("US$ 5", 1, true), "balance remains accessible")
local function find_tile(node, name, in_scroll)
  in_scroll = in_scroll or node.kind == "scroll"
  if node.props ~= nil and node.props.onClick ~= nil then
    local ok, hit = pcall(function()
      for _, child in ipairs(node.children or {}) do
        if child.kind == "label" and child.props.text and child.props.text:find(name, 1, true) then
          return true
        end
      end
      return false
    end)
    if ok and hit then
      return node, in_scroll
    end
  end
  for _, child in ipairs(node.children or {}) do
    local found, scroll = find_tile(child, name, in_scroll)
    if found then return found, scroll end
  end
end
local failures = {}
local function regression(name, test)
  local ok, err = pcall(test)
  if ok then print("PASS " .. name) else
    print("FAIL " .. name .. ": " .. err)
    table.insert(failures, name)
  end
end
regression("real status pacing remains visible separately from action hint", function()
  assert(labels:find("Ritmo · 50% do tempo → 144%", 1, true), "render shared window.text pacing, not fake window.hint")
  assert(labels:find("acima do ritmo · pausa 1h44", 1, true), "retain action recommendation separately")
end)
regression("direct provider selection remains outside scrolling details", function()
  local tile, in_scroll = find_tile(h.tree, "Codex")
  assert(tile, "provider summary must be directly selectable")
  assert(not in_scroll, "selection must not scroll away with long machine details")
  local click = type(tile.props.onClick) == "function" and tile.props.onClick
    or assert(h.env[tile.props.onClick], "named handler must exist")
  h.labels = {}
  click()
  local selected = table.concat(h.labels, "\n")
  for _, text in ipairs({ "Codex", "Monthly allowance", "Saldo", "US$ 9", "login expirado", "vence 3h" }) do
    assert(selected:find(text, 1, true), "one click must expose " .. text)
  end
  assert(not selected:find("machine-5", 1, true), "do not leave previous provider details expanded")
  local first, scrolled = find_tile(h.tree, "Claude")
  assert(first and not scrolled, "other selection stays accessible independently of details")
end)
assert(#failures == 0, table.concat(failures, ", "))
print("PASS compact inspector retains quotas, pacing, balances and every machine")

local NAME = {
  claude = "Claude", codex = "Codex", xai = "Grok", zai = "Z.ai",
  deepseek = "DeepSeek", openrouter = "OpenRouter", ["opencode-go"] = "OpenCode Go",
}
local supported = { "claude", "codex", "xai", "zai", "deepseek", "openrouter", "opencode-go" }
local providers = {}
for _, id in ipairs(supported) do providers[#providers + 1] = { id = id } end
local all = harness("panel", { providers = providers })
all.update()
for _, id in ipairs(supported) do
  local tile, in_scroll = find_tile(all.tree, NAME[id])
  assert(tile and not in_scroll, "direct selector missing for " .. id)
  all.labels = {}
  local click = type(tile.props.onClick) == "function" and tile.props.onClick
    or assert(all.env[tile.props.onClick], "handler missing for " .. id)
  click()
  assert(table.concat(all.labels, "\n"):find(NAME[id], 1, true), "wrong provider selected: " .. id)
end
print("PASS every supported provider has a working direct named handler")

local hr = harness("panel", { providers = {} })
hr.update()
hr.env.onReport()
assert(#hr.terminals == 0, "the html report must not open a terminal")
local report_argv = nil
for _, argv in ipairs(hr.argvs) do
  if argv[2] == "report" then report_argv = argv end
end
assert(report_argv ~= nil, "onReport must call costs report")
assert(table.concat(report_argv, " "):find("%-%-format html %-%-open", 1, false),
  "report must request the html page opened in the browser")
assert(hr.closed == true, "panel closes after the report runs")
print("PASS report opens as html in the browser, no terminal")

local function find_button_text(node, text, in_scroll)
  in_scroll = in_scroll or node.kind == "scroll"
  if node.kind == "button" and node.props.text and node.props.text:find(text, 1, true) then
    return node, in_scroll
  end
  for _, child in ipairs(node.children or {}) do
    local found, scroll = find_button_text(child, text, in_scroll)
    if found then return found, scroll end
  end
end

local hf = harness("panel", { providers = { { id = "codex" }, { id = "zai" } } })
hf.update()
local tile = assert(find_tile(hf.tree, "Codex"))
tile.props.onClick()
assert(hf.state_store["selected_provider"] == "codex", "selection must persist in plugin state")

local h2 = harness("panel", { providers = { { id = "codex" }, { id = "zai" } } })
h2.state_store["selected_provider"] = "codex"
h2.update()
assert(find_tile(h2.tree, "Codex") ~= nil, "restored selection must exist")
local codex_card = table.concat(h2.labels, "\n")
assert(codex_card:find("Codex", 1, true), "persisted selection must render the codex card")
print("PASS provider selection survives reopen")

hf.env.onHide()
assert(find_tile(hf.tree, "Codex") == nil, "hidden provider must leave the grid")
assert(find_button_text(hf.tree, "provider oculto") ~= nil, "a restore control must appear")
assert(hf.state_store["hidden_providers"]:find("codex", 1, true), "hidden list must persist")
hf.env.onShow()
assert(find_tile(hf.tree, "Codex") ~= nil, "onShow restores every provider")
print("PASS providers can be hidden and restored from the UI")

local hw = harness("widget", { providers = {
  { id = "codex", windows = {
    { label = "mês", kind = "month", used_pct = 30, color = "green", reset = "3d" },
    { label = "sem", kind = "week", used_pct = 70, color = "orange", reset = "2d" },
  } },
}, use_order = { "codex" } })
hw.update()
assert(hw.written ~= nil and hw.written.path:find("costs%-gauge%.svg", 1, false) ~= nil,
  "the gauge svg must be written to the plugin data dir")
local svg = hw.written.content
local circles = select(2, svg:gsub("<circle", "%0"))
assert(svg:find("<svg", 1, true) and circles >= 5, "gauge svg must contain track and arc circles")
assert((select(2, svg:gsub("stroke%-dasharray", "%0")) == 2), "two used arcs for two windows")
assert(svg:find("CX", 1, true), "recommended provider monogram sits in the center")
print("PASS bar gauge renders concentric ring arcs with the provider monogram")

do
  -- The closed previous month sits next to the current one in the totals row,
  -- so a month rollover never reads as lost data.
  local hp = harness("panel", {
    providers = {},
    snapshot = { spend_usd = 9.63, month = "2026-11", prev_month_usd = 2196.4, prev_month = "2026-10" },
  })
  hp.update()
  local texts = table.concat(hp.labels, "\n")
  assert(texts:find("US$ 2196.40", 1, true), "the closed previous month must show in the totals")
  assert(texts:find("API · MÊS PASSADO", 1, true), "the previous-month stat must be labeled as API")
  local hq = harness("panel", { providers = {} })
  hq.update()
  assert(not table.concat(hq.labels, "\n"):find("API · MÊS PASSADO", 1, true),
    "no previous-month stat when the binary does not provide it")
  print("PASS totals show the closed previous month when status provides it")
end
