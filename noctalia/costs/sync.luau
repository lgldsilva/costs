--!nonstrict
-- Noctalia 5.3 limits captured runAsync calls to 5s (60s maximum).
-- A sync may legitimately take longer; stream it and frame the real exit code.
local M = {}
local EXIT_MARKER = "__COSTS_SYNC_EXIT__"
local DETAIL_LIMIT = 200

local function shell_quote(value)
  return "'" .. string.gsub(value, "'", "'\\''") .. "'"
end

function M.start(binary, complete)
  local detail = ""
  local finished = false
  local command = shell_quote(binary) .. " sync 2>&1; code=$?; printf '\\n"
    .. EXIT_MARKER .. "%s\\n' \"$code\""
  local started = noctalia.runStream(command, function(line)
    if finished then
      return
    end
    local code = string.match(line, "^" .. EXIT_MARKER .. "(%d+)$")
    if code ~= nil then
      finished = true
      complete({ exitCode = tonumber(code), stderr = detail ~= "" and detail or ("exit " .. code) })
    elseif line ~= "" and not string.match(line, "^ok%s") then
      -- Keep diagnostics, not successful host rows; bound memory and notification size.
      detail = string.sub(detail .. line .. "\n", 1, DETAIL_LIMIT)
    end
  end)
  if not started then
    complete({ exitCode = -1, stderr = "não iniciou: " .. binary })
  end
  return started
end

return M
