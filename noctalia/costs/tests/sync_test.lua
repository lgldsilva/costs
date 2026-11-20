-- Run from the repository root: lua noctalia/costs/tests/sync_test.lua
-- The plugin uses the Lua-compatible subset of Luau; mock only host/UI APIs.
local plugin_dir = arg[1] or "noctalia/costs"

local function quote(value)
  return "'" .. value:gsub("'", "'\\''") .. "'"
end

local function harness(options)
  options = options or {}
  local h = { errors = {}, streams = {}, status_reads = 0 }
  local env = setmetatable({}, { __index = _G })
  env.ui = setmetatable({}, { __index = function()
    return function(props, children) return { props = props, children = children } end
  end })
  env.panel = { render = function(tree) h.tree = tree end }
  env.noctalia = {
    getConfig = function() return options.binary or "costs" end,
    notifyError = function(title, detail) table.insert(h.errors, { title, detail }) end,
    json = { decode = function() return { providers = {} } end },
    state = { get = function() return nil end, set = function() end },
    runAsync = function(argv, callback)
      if argv[2] == "sync" then
        -- Noctalia 5.3 defaults captured processes to a five-second timeout.
        callback({ exitCode = -1, timedOut = true, stdout = "", stderr = "" })
      elseif argv[2] == "quota" then
        callback({ exitCode = 0, stdout = "" })
      else
        assert(argv[2] == "status")
        h.status_reads = h.status_reads + 1
        callback({ exitCode = 0, stdout = "{}" })
      end
      return true
    end,
    runStream = function(command, callback)
      table.insert(h.streams, { command = command, callback = callback })
      if options.reject then return false end
      if options.execute then
        local pipe = assert(io.popen(command, "r"))
        for line in pipe:lines() do callback(line) end
        assert(pipe:close())
      end
      return true
    end,
  }
  env.require = function(path)
    return assert(loadfile(plugin_dir .. "/" .. path, "t", env))()
  end
  assert(loadfile(plugin_dir .. "/panel.luau", "t", env))()
  h.sync = env.onSync
  h.ipc = env.onIpc
  h.open = env.onOpen
  return h
end

local function done(stream, code)
  stream.callback("__COSTS_SYNC_EXIT__" .. code)
end

local function rendered_texts(h)
  local texts = {}
  local function walk(node)
    if type(node) ~= "table" then return end
    if node.props ~= nil and type(node.props.text) == "string" then
      table.insert(texts, node.props.text)
    end
    for _, child in ipairs(node.children or {}) do walk(child) end
  end
  walk(h.tree)
  return table.concat(texts, "|")
end

local tests = {}

function tests.sync_is_not_subject_to_the_captured_process_timeout()
  local h = harness()
  h.sync()
  assert(#h.streams == 1, "sync must use streaming, not runAsync's 5s timeout")
  assert(#h.errors == 0, "long sync must not produce a false failure")
  assert(h.status_reads == 0, "wait for the exit marker before refreshing")
  done(h.streams[1], 0)
  done(h.streams[1], 0)
  assert(h.status_reads == 1, "completion must be delivered only once")
end

function tests.duplicate_clicks_are_ignored_until_completion()
  local h = harness()
  h.sync()
  h.sync()
  assert(#h.streams == 1)
  done(h.streams[1], 0)
  h.sync()
  assert(#h.streams == 2, "button must become available again")
end

function tests.successful_sync_confirms_on_the_button()
  local h = harness()
  h.sync()
  done(h.streams[1], 0)
  local joined = rendered_texts(h)
  assert(joined:find("Sincronizado ✓", 1, true), "a successful sync must confirm on the button")
  assert(not joined:find("Sincronizando", 1, true), "the busy label must be gone")
end

function tests.failed_sync_does_not_confirm()
  local h = harness()
  h.sync()
  done(h.streams[1], 1)
  assert(#h.errors == 1)
  assert(not rendered_texts(h):find("Sincronizado ✓", 1, true), "a failed sync must not claim success")
end

function tests.reopening_the_panel_clears_the_confirmation()
  local h = harness()
  h.sync()
  done(h.streams[1], 0)
  h.open()
  assert(not rendered_texts(h):find("Sincronizado ✓", 1, true), "a fresh open starts neutral")
end

function tests.nonzero_exit_shows_the_failure_and_refreshes()
  local h = harness()
  h.sync()
  h.streams[1].callback("ERRO  mac  claude  connection lost")
  for _ = 1, 40 do h.streams[1].callback("ok    lab-node1  pi  10 rows") end
  done(h.streams[1], 1)
  assert(#h.errors == 1)
  assert(h.errors[1][2]:find("connection lost", 1, true), "retain the real error, not success lines")
  assert(h.status_reads == 1)
end

function tests.launch_failure_does_not_leave_the_button_busy()
  local h = harness({ reject = true })
  h.sync()
  assert(#h.errors == 1)
  h.sync()
  assert(#h.streams == 2, "a rejected launch must allow retry")
end

function tests.marker_is_exact_and_diagnostics_are_bounded()
  local h = harness()
  h.sync()
  h.streams[1].callback("log prefix __COSTS_SYNC_EXIT__0")
  assert(h.status_reads == 0)
  h.streams[1].callback(string.rep("x", 10000))
  done(h.streams[1], 2)
  assert(#h.errors[1][2] <= 200)
end

function tests.real_shell_handles_slow_sync_and_literal_binary_path()
  local path = os.tmpname()
  os.remove(path)
  path = path .. " space ' ; dollar $ costs"
  local file = assert(io.open(path, "w"))
  file:write("#!/bin/sh\n[ \"$1\" = sync ] || exit 9\nsleep 6\nprintf 'ok slow sync'\n")
  file:close()
  assert(os.execute("chmod 700 " .. quote(path)))
  local ok, err = pcall(function()
    local h = harness({ binary = path, execute = true })
    h.sync()
    assert(#h.errors == 0)
    assert(h.status_reads == 1, "slow sync must finish even without a final newline")
  end)
  os.remove(path)
  if not ok then error(err) end
end

function tests.ipc_uses_the_same_sync_handler_as_the_button()
  local h = harness()
  h.ipc("ignored")
  assert(#h.streams == 0)
  h.ipc("sync")
  assert(#h.streams == 1)
  done(h.streams[1], 0)
  assert(h.status_reads == 1)
end

function tests.real_shell_preserves_missing_executable_exit_code()
  local h = harness({ binary = "/no/such/costs", execute = true })
  h.sync()
  assert(#h.errors == 1)
  assert(h.status_reads == 1, "an actual shell failure must still complete")
end

local names = {
  "sync_is_not_subject_to_the_captured_process_timeout",
  "duplicate_clicks_are_ignored_until_completion",
  "successful_sync_confirms_on_the_button",
  "failed_sync_does_not_confirm",
  "reopening_the_panel_clears_the_confirmation",
  "nonzero_exit_shows_the_failure_and_refreshes",
  "launch_failure_does_not_leave_the_button_busy",
  "marker_is_exact_and_diagnostics_are_bounded",
  "real_shell_handles_slow_sync_and_literal_binary_path",
  "ipc_uses_the_same_sync_handler_as_the_button",
  "real_shell_preserves_missing_executable_exit_code",
}
for _, name in ipairs(names) do
  tests[name]()
  print("PASS " .. name)
end
print(string.format("%d plugin sync tests passed", #names))
