--!nonstrict
-- Widget de barra do costs: círculos de ritmo por janela do provider
-- recomendado (mês/semana/5h, como os anéis do Costs Prévia no macOS) +
-- gasto estimado. Árvore declarativa: setGlyph/setText deixam de funcionar
-- depois do primeiro barWidget.render.

local snapshot = require("./snapshot.luau")
local costs_bin = noctalia.getConfig("costs_bin")
if costs_bin == nil or costs_bin == "" then
  costs_bin = "costs"
end
local refresh_ms = (noctalia.getConfig("refresh_minutes") or 5) * 60 * 1000
if refresh_ms < 60 * 1000 then
  refresh_ms = 60 * 1000
end

local NAME = {
  claude = "Claude",
  codex = "Codex",
  zai = "Z.ai",
  xai = "Grok",
  ["opencode-go"] = "OpenCode Go",
  deepseek = "DeepSeek",
  openrouter = "OpenRouter",
}

local MONOGRAM = {
  claude = "C",
  codex = "CX",
  zai = "Z",
  xai = "X",
  ["opencode-go"] = "OC",
  deepseek = "DS",
  openrouter = "OR",
}

local function display(id)
  return NAME[id] or id
end

-- Cor de ritmo: hex para não depender de roles opcionais do palette.
local PACE_DOT = {
  green = "#34D399",
  orange = "#F59E0B",
  red = "#EF4444",
  gray = "on_surface/0.35",
}

local PACE_ICON = {
  green = "🟢",
  orange = "🟠",
  red = "🔴",
  gray = "⚪",
}

-- Ordem dos anéis do Costs Prévia: mês, semana, sessão.
local KIND_ORDER = { month = 1, week = 2, session = 3 }

local function legacy_kind(label)
  local l = (label or ""):lower()
  if l == "5h" or l == "session" or l == "sessão" or l == "sessao" then
    return "session"
  elseif l == "sem" or l == "week" or l == "weekly" or l == "semanal" or l == "7d" or l == "mcp" then
    return "week"
  elseif l == "mês" or l == "mes" or l == "month" or l == "monthly" or l == "30d" then
    return "month"
  end
  return nil
end

-- Um círculo por tipo distinto de janela do provider recomendado, na ordem
-- mês → semana → sessão; sem recomendação, o pior de cada tipo entre todos.
local function collect_dots(source)
  local seen = {}
  local dots = {}
  for _, w in ipairs(source["windows"] or {}) do
    local kind = w["kind"] or legacy_kind(w["label"])
    if kind ~= nil and seen[kind] == nil then
      seen[kind] = true
      table.insert(dots, {
        kind = kind,
        color = w["color"] or "gray",
        used = w["used_pct"] or 0,
        reset = w["reset"],
      })
    end
  end
  table.sort(dots, function(a, b)
    return (KIND_ORDER[a.kind] or 9) < (KIND_ORDER[b.kind] or 9)
  end)
  return dots
end

local function dots_for(data)
  local order = data["use_order"]
  local target = order ~= nil and order[1] or nil
  local source = nil
  for _, p in ipairs(data["providers"] or {}) do
    if p["id"] == target then
      source = p
      break
    end
  end
  if source ~= nil then
    return collect_dots(source), target
  end
  -- Sem recomendação: pior cor de cada tipo de janela entre todos.
  local rank = { red = 1, orange = 2, green = 3, gray = 4 }
  local worst = {}
  for _, p in ipairs(data["providers"] or {}) do
    for _, w in ipairs(p["windows"] or {}) do
      local kind = w["kind"] or legacy_kind(w["label"])
      if kind ~= nil then
        local cur = worst[kind]
        if cur == nil or (rank[w["color"] or "gray"] or 5) < (rank[cur.color] or 5) then
          worst[kind] = { kind = kind, color = w["color"] or "gray", used = w["used_pct"] or 0, reset = w["reset"] }
        end
      end
    end
  end
  local dots = {}
  for _, d in pairs(worst) do
    table.insert(dots, d)
  end
  table.sort(dots, function(a, b)
    return (KIND_ORDER[a.kind] or 9) < (KIND_ORDER[b.kind] or 9)
  end)
  return dots, target
end

local function money(v)
  return string.format("US$ %.2f", v or 0)
end

-- Gauge em SVG renderizado pelo librsvg do Noctalia: anéis concêntricos
-- (mês, semana, sessão) com o arco proporcional ao uso e a sigla do
-- provider no centro — o mesmo desenho do status item do Costs Prévia.
local RINGS = {
  { radius = 26.0, width = 4.0 },
  { radius = 19.5, width = 3.4 },
  { radius = 13.5, width = 2.8 },
}

local function ring_svg(radius, width, frac, color)
  local track = string.format(
    '<circle cx="32" cy="32" r="%.1f" fill="none" stroke="#8A8F98" stroke-opacity="0.28" stroke-width="%.1f"/>',
    radius, width)
  if frac <= 0.005 then
    return track
  end
  local circ = 2 * math.pi * radius
  local dash = frac * circ
  return track
    .. string.format(
      '<circle cx="32" cy="32" r="%.1f" fill="none" stroke="%s" stroke-width="%.1f" '
        .. 'stroke-dasharray="%.2f %.2f" transform="rotate(-90 32 32)"/>',
      radius, color, width, dash, circ)
end

local function build_gauge_svg(dots, monogram)
  local parts = {
    '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" width="64" height="64">',
  }
  for i, ring in ipairs(RINGS) do
    local dot = dots[i]
    if dot ~= nil then
      parts[#parts + 1] = ring_svg(ring.radius, ring.width,
        math.max(0, math.min(100, dot.used or 0)) / 100, PACE_DOT[dot.color] or PACE_DOT.gray)
    else
      parts[#parts + 1] = ring_svg(ring.radius, ring.width, 0, PACE_DOT.gray)
    end
  end
  local label = monogram or "?"
  local font = #label > 1 and 11.5 or 15.5
  local y = #label > 1 and 36 or 37.5
  parts[#parts + 1] = string.format(
    '<text x="32" y="%.1f" text-anchor="middle" font-family="sans-serif" '
      .. 'font-size="%.1f" font-weight="700" fill="#E6E9EF">%s</text>',
    y, font, label)
  parts[#parts + 1] = "</svg>"
  return table.concat(parts)
end

local GAUGE_PATH = (noctalia.pluginDataDir ~= nil and noctalia.pluginDataDir() or "/tmp")
  .. "/costs-gauge.svg"
local last_svg = nil

local function show_error_tree(detail)
  barWidget.render(ui.row({ gap = 6, align = "center" }, {
    ui.glyph({ name = "alert-circle", size = 14, color = "error" }),
    ui.label({ text = "costs?", fontSize = 12, color = "on_surface_variant" }),
  }))
  barWidget.setTooltip({
    { key = "costs status falhou", value = string.sub(detail or "?", 1, 200) },
    { key = "binário", value = costs_bin },
  })
end

local function provider_cost(p)
  if p["subscription_usd_month"] ~= nil then
    return "assinatura " .. money(p["subscription_usd_month"]) .. "/mês"
  elseif p["cost_usd"] ~= nil then
    return "API estimada " .. money(p["cost_usd"])
  end
  return "custo n/d"
end

-- Uma linha por provedor (custo + saldo) e uma mínima por janela.
local function window_short(w)
  local used = w["used_pct"] or 0
  return (PACE_ICON[w["color"]] or "") .. " " .. string.format("%.0f", used) .. "% usado · renova " .. (w["reset"] or "-")
end

local function provider_rows(data, rows)
  for _, p in ipairs(data["providers"] or {}) do
    local stale = ""
    if p["stale"] == true then
      stale = " ~"
    end
    local value = provider_cost(p)
    if p["balance"] ~= nil then
      value = value .. " · saldo " .. p["balance"]
    end
    table.insert(rows, { key = display(p["id"]) .. stale, value = value })
    if p["exhausted_reset"] ~= nil then
      table.insert(rows, { key = "  ⏳", value = "esgotado · volta em " .. p["exhausted_reset"] })
    end
    for _, w in ipairs(p["windows"] or {}) do
      table.insert(rows, { key = "  " .. (w["label"] or "?"), value = window_short(w) })
    end
  end
end

local function render(data)
  local spend = data["spend_usd"] or 0
  local age = data["sync_age"] or "?"
  local mark = ""
  if data["stale"] == true then
    mark = " ~"
  end
  local dots, target = dots_for(data)
  local svg = build_gauge_svg(dots, target ~= nil and MONOGRAM[target] or "?")
  if svg ~= last_svg then
    noctalia.writeFile(GAUGE_PATH, svg)
    last_svg = svg
  end
  barWidget.render(ui.row({ gap = 0, align = "center" }, {
    ui.image({ key = "gauge", path = GAUGE_PATH, width = 21, height = 21 }),
  }))

  local rows = {}
  table.insert(rows, { key = "Mês " .. (data["month"] or "?"), value = "≈ " .. money(spend) })
  if target ~= nil then
    table.insert(rows, { key = "Usar agora", value = display(target) })
  end
  if #dots > 0 then
    local dot_text = {}
    for _, d in ipairs(dots) do
      table.insert(dot_text, (PACE_ICON[d.color] or "") .. " " .. d.kind .. " " .. string.format("%.0f", d.used) .. "%")
    end
    table.insert(rows, { key = "Cotas (recomendado)", value = table.concat(dot_text, " · ") })
  end
  local order = data["use_order"]
  if order ~= nil and #order > 1 then
    local rest = {}
    for i = 2, #order do
      table.insert(rest, display(order[i]))
    end
    table.insert(rows, { key = "Próximos", value = table.concat(rest, " › ") })
  end
  provider_rows(data, rows)
  local subs = data["subscriptions_usd_month"] or 0
  if subs > 0 then
    table.insert(rows, { key = "Assinaturas", value = money(subs) .. "/mês" })
  end
  table.insert(rows, { key = "Coleta mais antiga", value = age .. mark })
  barWidget.setTooltip(rows)
end

function update()
  noctalia.setUpdateInterval(refresh_ms)
  snapshot.fetch(costs_bin, function(res)
    if res.exitCode ~= 0 then
      local detail = res.stderr
      if detail == nil or detail == "" then
        detail = res.stdout
      end
      show_error_tree(detail)
      return
    end
    noctalia.state.set("status_json", res.stdout)
    local data, err = noctalia.json.decode(res.stdout)
    if data == nil then
      show_error_tree(err)
      return
    end
    render(data)
  end)
end

function onClick()
  noctalia.togglePanel("lgldsilva/costs:details")
end

function onRightClick()
  noctalia.runAsync({ costs_bin, "report", "--format", "html", "--open" }, function(res)
    if res.exitCode ~= 0 then
      noctalia.notifyError("costs report falhou", res.stderr or res.stdout or "?")
    end
  end)
end
