--!nonstrict
-- Refresh quota before the read-only status snapshot. Expired session windows
-- disappear from stale cache entries even when cost collection is up to date.
-- Timers and panel open use --if-stale (network only past the cache TTL); only an
-- explicit refresh forces it, so the bar does not hammer undocumented quota APIs.
local M = {}
local QUOTA_TIMEOUT_MS = 60000

function M.fetch(binary, complete, force)
  local function read_status()
    local started = noctalia.runAsync({ binary, "status" }, complete)
    if not started then
      complete({ exitCode = -1, stderr = "não iniciou: " .. binary })
    end
    return started
  end

  local mode = force and "--refresh" or "--if-stale"
  local started = noctalia.runAsync({ binary, "quota", mode }, function(_result)
    -- A provider failure is retained in the cache with its last good data and
    -- error code; status renders that instead of hiding every other provider.
    read_status()
  end, QUOTA_TIMEOUT_MS)
  if not started then
    return read_status()
  end
  return true
end

return M
