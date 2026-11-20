--!nonstrict
-- Painel do costs: espelha o layout do Costs Prévia (macOS) nos primitivos
-- declarativos do Noctalia — tiles de provedor com badge de marca, card com
-- ícone por linha (plano, saldo, resets, esgotado) e métricas de valor.

local sync = require("./sync.luau")
local snapshot = require("./snapshot.luau")
local costs_bin = noctalia.getConfig("costs_bin")
if costs_bin == nil or costs_bin == "" then
  costs_bin = "costs"
end

local data = nil
local loading = true
local syncing = false
local sync_feedback = false
local error_msg = nil
local selected_provider = nil
local agents_open = {}
local hidden = {}
local render -- forward declaration: closures below call it
local select_provider -- forward declaration: grid tiles close over it

-- Providers hidden from the grid/card, persisted in plugin state.
local function persist_hidden()
  local list = {}
  for id in pairs(hidden) do
    table.insert(list, id)
  end
  noctalia.state.set("hidden_providers", noctalia.json.encode(list))
end

local function load_hidden()
  hidden = {}
  local raw = noctalia.state.get("hidden_providers")
  if raw == nil or raw == "" then
    return
  end
  local list = noctalia.json.decode(raw)
  if type(list) == "table" then
    for _, id in ipairs(list) do
      hidden[id] = true
    end
  end
end

local function hide_provider(id)
  if id == nil then
    return
  end
  hidden[id] = true
  persist_hidden()
  if selected_provider == id then
    selected_provider = nil
  end
  render()
end

-- IPC-facing: hide the currently selected provider, restore all.
function onHide()
  hide_provider(selected_provider)
end

function onShow()
  hidden = {}
  persist_hidden()
  render()
end

-- Same named-string callback API as onRefresh/onSync stays exported for IPC;
-- the grid tiles and the agents toggle use closures (plugin_api >= 9).
local PROVIDER_HANDLERS = {
  ["claude"] = "onSelectClaude",
  ["codex"] = "onSelectCodex",
  ["xai"] = "onSelectXai",
  ["zai"] = "onSelectZai",
  ["deepseek"] = "onSelectDeepseek",
  ["openrouter"] = "onSelectOpenrouter",
  ["opencode-go"] = "onSelectOpencodeGo",
}

-- Mesmas cores de marca e monogramas do ProviderCatalog do Costs Prévia.
local CATALOG = {
  ["claude"] = { name = "Claude", monogram = "C", color = "#D97857" },
  ["codex"] = { name = "Codex", monogram = "CX", color = "#0FA37F" },
  ["zai"] = { name = "Z.ai", monogram = "Z", color = "#3B5CE6" },
  ["xai"] = { name = "Grok", monogram = "X", color = "#26262B" },
  ["opencode-go"] = { name = "OpenCode Go", monogram = "OC", color = "#6E57CF" },
  ["deepseek"] = { name = "DeepSeek", monogram = "DS", color = "#4D6BFF" },
  ["openrouter"] = { name = "OpenRouter", monogram = "OR", color = "#66738C" },
}

local SECONDARY = "on_surface_variant"

-- Cores de ritmo do status.rs como hex: legíveis nos dois temas, sem
-- depender de roles opcionais do palette.
local PACE_FILL = {
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

local function catalog(id)
  return CATALOG[id] or { name = id, monogram = (id:sub(1, 2)):upper(), color = "#66738C" }
end

local function money(v)
  if type(v) ~= "number" then
    return "US$ n/d"
  end
  return string.format("US$ %.2f", v)
end

local function tokens_text(tok)
  if tok == nil or tok == 0 then
    return ""
  end
  return string.format(" · %.1f M tokens", tok / 1000000)
end

local function provider_cost(p)
  if p["subscription_usd_month"] ~= nil then
    return "assinatura " .. money(p["subscription_usd_month"]) .. "/mês"
  elseif p["cost_usd"] ~= nil then
    return "API estimada " .. money(p["cost_usd"])
  end
  return "API estimada n/d"
end

-- Keep the shared elapsed-time/projection text distinct from the action hint.
-- status.rs emits both: text comes from window_lines, hint from window_hint.
local function pacing_text(w)
  return (w["text"] or ""):match(" · (.-) · ⏱") or "sem projeção"
end

-- Selo com as iniciais do provedor, igual ao ProviderBadge do macOS.
local function badge(id, size, font_size)
  local c = catalog(id)
  return ui.row({
    key = "badge-" .. id,
    width = size,
    height = size,
    fill = c.color,
    radius = math.max(6, math.floor(size * 0.32)),
    align = "center",
    justify = "center",
  }, {
    ui.label({ text = c.monogram, fontSize = font_size, fontWeight = "bold", color = "#FFFFFF" }),
  })
end

local function metric(title, value, detail)
  return ui.column({ gap = 2, flexGrow = 1 }, {
    ui.label({ text = title:upper(), fontSize = 8, fontWeight = "semibold", color = SECONDARY }),
    ui.label({ text = value, fontSize = 18, fontWeight = "semibold", maxLines = 1 }),
    ui.label({ text = detail, fontSize = 10, color = SECONDARY }),
  })
end

-- Linha de detalhe com ícone à esquerda e valor à direita (padrão do macOS).
local function detail_row(glyph_name, title, value, tip, value_color)
  return ui.row({
    key = "row-" .. title,
    gap = 8,
    align = "center",
    tooltip = tip,
  }, {
    ui.glyph({ name = glyph_name, size = 13, color = SECONDARY }),
    ui.label({ text = title, fontSize = 11, color = SECONDARY }),
    ui.spacer({ flexGrow = 1 }),
    ui.label({ text = value, fontSize = 11, fontWeight = "medium", color = value_color }),
  })
end

-- Porta o SnapshotPresentation.resetRows: grupos de vencimento quando o
-- provedor publica expiry_groups; senão contagem + próximo vencimento.
local function reset_rows(credits, fallback_title)
  local rows = {}
  if credits == nil then
    return rows
  end
  local available = credits["available"] or 0
  if available <= 0 then
    return rows
  end
  local groups = credits["expiry_groups"]
  if groups ~= nil then
    for _, g in ipairs(groups) do
      if (g["available"] or 0) > 0 then
        table.insert(rows, { title = fallback_title, value = g["available"] .. " · vence " .. (g["expires_at_raw"] or "?") })
      end
    end
    return rows
  end
  local suffix = ""
  if credits["next_expiry"] ~= nil then
    suffix = " · vence " .. credits["next_expiry"]
  end
  table.insert(rows, { title = fallback_title, value = available .. suffix })
  return rows
end

-- Used quota, reset time, pacing and action each have a separate visible line.
local function window_row(w)
  local used = w["used_pct"] or 0
  local used_clamped = math.max(0, math.min(100, used))
  local when = "Renova em " .. (w["reset"] or "-")
  local epoch = tonumber(w["resets_at"] or 0) or 0
  if epoch > 0 then
    local ok, abs = pcall(noctalia.formatTime, "%a %d/%m %H:%M", math.floor(epoch))
    if ok and abs then
      when = when .. " · " .. abs
    end
  end
  local tip = (w["hint"] or "") .. " · " .. string.format("%.0f", used) .. "% usado"
  return ui.column({ key = "win-" .. (w["label"] or "?"), gap = 4, tooltip = tip }, {
    ui.row({ align = "center", justify = "space_between" }, {
      ui.label({ text = (w["label"] or "?"):upper(), fontSize = 10, fontWeight = "semibold", color = SECONDARY }),
      ui.label({ text = string.format("%.0f", used) .. "% usado", fontSize = 13, fontWeight = "bold" }),
    }),
    ui.progress({
      progress = used_clamped / 100,
      fill = PACE_FILL[w["color"]] or PACE_FILL.gray,
      track = "on_surface/0.14",
      radius = 8,
      height = 7,
    }),
    ui.label({ text = when, fontSize = 11, color = SECONDARY }),
    ui.label({ text = "Ritmo · " .. pacing_text(w), fontSize = 11 }),
    ui.label({ text = w["hint"] or "", fontSize = 11 }),
  })
end

local function agents_section(p)
  local section = {}
  local open = agents_open[p["id"]] ~= false
  table.insert(section, ui.button({
    text = "Detalhe por máquina e ferramenta",
    glyph = open and "chevron-down" or "chevron-right",
    fontSize = 10,
    variant = "ghost",
    tooltip = "API estimada por máquina/harness de " .. catalog(p["id"]).name,
    onClick = function()
      agents_open[p["id"]] = not open
      render()
    end,
  }))
  if open and p["agents"] ~= nil then
    for _, a in ipairs(p["agents"]) do
      table.insert(section, ui.row({ key = "agent-" .. (a["label"] or "?"), gap = 8, align = "center" }, {
        ui.label({ text = a["label"] or "?", fontSize = 10, flexGrow = 1 }),
        ui.label({ text = money(a["usd"]), fontSize = 10, fontFamily = "monospace", color = SECONDARY }),
        ui.label({ text = tokens_text(a["tokens"]):gsub("^ · ", "· ", 1), fontSize = 9, color = SECONDARY }),
      }))
    end
  end
  return ui.column({ gap = 7 }, section)
end

local function provider_card(p, rank)
  local windows = p["windows"] or {}
  local children = {}

  -- Cabeçalho: badge de marca, nome, subtítulo de estado e rank de uso.
  local subtitle = p["updated"]
  if p["ok"] == false then
    subtitle = p["error_text"] or "Não foi possível consultar a cota"
  end
  if subtitle == nil then
    subtitle = provider_cost(p)
  end
  local header = {
    badge(p["id"], 38, 14),
    ui.column({ gap = 2 }, {
      ui.label({ text = catalog(p["id"]).name, fontSize = 17, fontWeight = "semibold" }),
      ui.label({ text = subtitle, fontSize = 10, color = SECONDARY }),
    }),
    ui.spacer({ flexGrow = 1 }),
  }
  if rank ~= nil then
    table.insert(header, ui.column({ gap = 2, align = "end" }, {
      ui.label({ text = "USAR AGORA", fontSize = 8, fontWeight = "semibold", color = SECONDARY }),
      ui.label({ text = "#" .. rank, fontSize = 14, fontWeight = "semibold", color = "primary" }),
    }))
  end
  table.insert(header, ui.button({
    glyph = "eye-off",
    variant = "ghost",
    tooltip = "Ocultar " .. catalog(p["id"]).name .. " do painel (restaurar com o botão da grade)",
    onClick = function() hide_provider(p["id"]) end,
  }))
  table.insert(children, ui.row({ key = "card-header", gap = 10, align = "center" }, header))

  table.insert(children, ui.separator({ spacing = 2, color = "outline", opacity = 0.18 }))

  if type(p["plan"]) == "string" and p["plan"]:match("%S") then
    table.insert(children, detail_row("certificate", "Plano detectado", p["plan"]))
  end
  if p["stale"] == true then
    table.insert(children, ui.row({
      fill = "tertiary/0.10",
      radius = 8,
      padding = 8,
      align = "center",
      gap = 7,
    }, {
      ui.glyph({ name = "clock", size = 13, color = "tertiary" }),
      ui.label({ text = "Cota desatualizada · último valor conhecido", fontSize = 11, color = "tertiary", maxLines = 2 }),
    }))
  end
  if p["error"] ~= nil then
    local no_data = #windows == 0
    local message = p["error_hint"] or p["error_text"] or p["error"] or "A consulta falhou; os últimos valores permanecem visíveis."
    if not no_data then
      message = message .. " · mostrando último valor"
    end
    table.insert(children, ui.row({
      fill = "error/0.10",
      radius = 9,
      padding = 10,
      align = "center",
      gap = 8,
    }, {
      ui.glyph({ name = "alert-circle", size = 14, color = "error" }),
      ui.label({ text = message, fontSize = 12, color = "error", maxLines = 2 }),
    }))
  end

  if #windows == 0 then
    table.insert(children, ui.row({
      fill = "on_surface/0.05",
      radius = 9,
      padding = 10,
      align = "center",
      gap = 8,
    }, {
      ui.glyph({ name = "info-circle", size = 14, color = SECONDARY }),
      ui.column({ gap = 2 }, {
        ui.label({ text = "Sem janela de cota", fontSize = 12, fontWeight = "medium" }),
        ui.label({ text = "Este provedor não publicou limites no resumo.", fontSize = 10, color = SECONDARY }),
      }),
    }))
  else
    for i, w in ipairs(windows) do
      if i > 1 then
        table.insert(children, ui.separator({ spacing = 8 }))
      end
      table.insert(children, window_row(w))
    end
  end

  if p["balance"] ~= nil then
    table.insert(children, detail_row("credit-card", "Saldo", p["balance"], "Saldo pré-pago disponível"))
  end
  for _, row in ipairs(reset_rows(p["resets"], "Resets de sessão (5h)")) do
    table.insert(children, detail_row("rotate-clockwise", row.title, row.value,
      "Zera a janela na hora (resgate no app do provedor)"))
  end
  for _, row in ipairs(reset_rows(p["resets_week"], "Resets semanais")) do
    table.insert(children, detail_row("rotate-clockwise", row.title, row.value))
  end
  if p["exhausted_reset"] ~= nil then
    table.insert(children, detail_row("hourglass", "Janela esgotada", "volta em " .. p["exhausted_reset"]))
  end
  if p["priority_hint"] ~= nil then
    table.insert(children, ui.label({ text = p["priority_hint"], fontSize = 10, color = SECONDARY }))
  end

  table.insert(children, ui.row({ key = "card-metrics", gap = 0, paddingV = 2 }, {
    metric("API · estimativa", money(p["cost_usd"]), "neste mês"),
    ui.box({ fill = "outline/0.5", radius = 2, width = 1, height = 32 }),
    metric("Assinatura", money(p["subscription_usd_month"]), "mensal"),
  }))

  if p["agents"] ~= nil and #p["agents"] > 0 then
    table.insert(children, agents_section(p))
  end

  return ui.column({
    key = "card-" .. p["id"],
    gap = 12,
    padding = 12,
    fill = "surface_variant/0.58",
    radius = 14,
  }, children)
end

-- Grade compacta de tiles: badge de marca + nome, selecionável como no macOS.
local function provider_grid(providers, rank_of)
  local cells = {}
  for _, p in ipairs(providers) do
    local id = p["id"]
    local c = catalog(id)
    local summary = {}
    for _, w in ipairs(p["windows"] or {}) do
      table.insert(summary, (PACE_ICON[w["color"]] or "") .. string.format(" %.0f%%", w["used_pct"] or 0))
    end
    local tooltip = id .. " · " .. provider_cost(p)
    if #summary > 0 then
      tooltip = tooltip .. " · " .. table.concat(summary, " · ")
    end
    if p["stale"] then
      tooltip = tooltip .. " · desatualizado"
    end
    local chosen = id == selected_provider
    if PROVIDER_HANDLERS[id] ~= nil then
      table.insert(cells, ui.column({
        key = "pick-" .. id,
        onClick = function() select_provider(id) end,
        tooltip = tooltip,
        flexGrow = 1,
        align = "center",
        gap = 3,
        paddingV = 5,
        radius = 9,
        fill = chosen and "primary/0.12" or "on_surface/0.03",
        border = chosen and "primary/0.45" or "outline/0.15",
        borderWidth = 1,
      }, {
        badge(id, 22, 9),
        ui.label({
          text = c.name .. (rank_of[id] ~= nil and ("  #" .. rank_of[id]) or ""),
          fontSize = 10,
          fontWeight = chosen and "bold" or "medium",
          maxLines = 1,
        }),
      }))
    else
      -- Custom providers remain readable without inventing a callback API.
      table.insert(cells, ui.column({
        key = "pick-" .. id,
        tooltip = tooltip,
        flexGrow = 1,
        align = "center",
        gap = 3,
        paddingV = 5,
      }, {
        badge(id, 22, 9),
        ui.label({ text = c.name .. " · abaixo", fontSize = 10, color = SECONDARY, maxLines = 1 }),
      }))
    end
  end
  local rows = {}
  for i = 1, #cells, 4 do
    local row = {}
    for j = i, math.min(i + 3, #cells) do
      table.insert(row, cells[j])
    end
    table.insert(rows, ui.row({ key = "grid-row-" .. i, gap = 6 }, row))
  end
  return ui.column({ key = "provider-grid", gap = 6 }, rows)
end

function render()
  -- Com dados (cache do snapshot), o refresh não apaga a tela: só o rodapé avisa.
  if loading and data == nil then
    panel.render(ui.column({ gap = 10, padding = 16 }, {
      ui.label({ text = "Costs", fontSize = 16, fontWeight = "semibold" }),
      ui.label({ text = syncing and "Sincronizando…" or "Carregando…" }),
    }))
    return
  end
  if data == nil then
    panel.render(ui.column({ gap = 10, padding = 16 }, {
      ui.label({ text = "Costs", fontSize = 16, fontWeight = "semibold" }),
      ui.label({ text = error_msg or "sem dados" }),
      ui.button({ text = "Tentar de novo", onClick = "onRefresh" }),
    }))
    return
  end

  local age = data["sync_age"] or "?"
  local stale = data["stale"] == true
  local hidden_count = 0
  for _ in pairs(hidden) do
    hidden_count = hidden_count + 1
  end
  local providers = {}
  for _, p in ipairs(data["providers"] or {}) do
    if hidden[p["id"]] ~= true then
      table.insert(providers, p)
    end
  end

  local rank_of = {}
  local order = data["use_order"]
  if order ~= nil then
    for rank, id in ipairs(order) do
      rank_of[id] = rank
    end
  end

  local selected = nil
  for _, p in ipairs(providers) do
    if p["id"] == selected_provider then
      selected = p
    end
  end
  if selected == nil then
    for _, p in ipairs(providers) do
      if PROVIDER_HANDLERS[p["id"]] ~= nil then
        selected = p
        selected_provider = p["id"]
        break
      end
    end
  end

  local children = {}

  -- Titlebar: identidade do painel + atualização manual de cotas.
  table.insert(children, ui.row({ key = "titlebar", gap = 10, align = "center" }, {
    ui.row({ width = 36, height = 36, fill = "primary/0.15", radius = 11, align = "center", justify = "center" }, {
      ui.glyph({ name = "activity", size = 19, color = "primary" }),
    }),
    ui.column({ gap = 1 }, {
      ui.label({ text = "Costs", fontSize = 17, fontWeight = "bold" }),
      ui.label({ text = "Cotas e custos, em um só lugar", fontSize = 11, color = SECONDARY }),
    }),
    ui.spacer({ flexGrow = 1 }),
    ui.button({
      glyph = "refresh",
      variant = "ghost",
      tooltip = "Atualizar cotas e reler o resumo",
      onClick = "onRefresh",
    }),
  }))

  -- Totais no topo: só a comparação de API (mês corrente × fechado); a
  -- assinatura de cada provedor segue nos cards. Títulos com acento vão
  -- pré-maiúsculados: string.upper do Luau só cobre ASCII.
  local totals = {
    metric("API · ESTE MÊS", money(data["spend_usd"]), (data["month"] or "Mês") .. " · estimativa, não é pagamento"),
  }
  if data["prev_month_usd"] ~= nil then
    table.insert(totals, ui.box({ fill = "outline/0.5", radius = 2, width = 1, height = 28 }))
    table.insert(totals, metric("API · MÊS PASSADO", money(data["prev_month_usd"]), (data["prev_month"] or "—") .. " · mês fechado"))
  end
  table.insert(children, ui.row({ key = "totals", gap = 0, paddingV = 4 }, totals))
  if order ~= nil and #order > 0 then
    table.insert(children, ui.row({ key = "use-now", tooltip = "Provedores em ordem de prioridade: " .. table.concat(order, " › ") .. ". Detalhes explicam o ritmo." }, {
      ui.label({ text = "Usar agora · " .. catalog(order[1]).name, fontSize = 11, fontWeight = "medium" }),
    }))
  end

  table.insert(children, provider_grid(providers, rank_of))
  if hidden_count > 0 then
    local names = {}
    for id in pairs(hidden) do
      table.insert(names, catalog(id).name)
    end
    table.sort(names)
    table.insert(children, ui.row({ key = "hidden-row", gap = 6, align = "center" }, {
      ui.button({
        text = hidden_count == 1 and "Mostrar 1 provider oculto" or ("Mostrar " .. hidden_count .. " providers ocultos"),
        glyph = "eye",
        variant = "ghost",
        tooltip = "Restaurar: " .. table.concat(names, ", "),
        onClick = "onShow",
      }),
    }))
  end
  table.insert(children, ui.separator({ spacing = 2, color = "outline", opacity = 0.18 }))

  -- Selection lives outside the scrolling detail area: the grid above stays
  -- visible while long machine details scroll below.
  local list = {}
  if selected ~= nil then
    table.insert(list, provider_card(selected, rank_of[selected["id"]]))
  end
  for _, p in ipairs(providers) do
    if PROVIDER_HANDLERS[p["id"]] == nil then
      if #list > 0 then
        table.insert(list, ui.separator({ spacing = 6 }))
      end
      table.insert(list, provider_card(p, nil))
    end
  end
  table.insert(children, ui.scroll({ key = "details", flexGrow = 1, gap = 10 }, list))

  table.insert(children, ui.row({ key = "actions", gap = 8 }, {
    ui.button({
      text = syncing and "Sincronizando…" or (sync_feedback and "Sincronizado ✓" or "Sincronizar custos"),
      variant = "primary",
      onClick = "onSync",
      enabled = not syncing,
      flexGrow = 1,
    }),
    ui.button({ text = "Abrir relatório completo", onClick = "onReport", flexGrow = 1 }),
  }))

  local footer = {
    ui.glyph({ name = "device-desktop", size = 10, color = SECONDARY }),
    ui.label({ text = "Custos sincronizados · " .. age .. (stale and " ~" or ""), fontSize = 9, color = SECONDARY }),
  }
  if stale then
    table.insert(footer, ui.spacer({ flexGrow = 1 }))
    table.insert(footer, ui.label({ text = "SINCRONIZAÇÃO ANTIGA", fontSize = 9, fontWeight = "medium", color = "error" }))
  end
  if loading then
    table.insert(footer, ui.spacer({ flexGrow = 1 }))
    table.insert(footer, ui.label({ text = "atualizando…", fontSize = 9, color = SECONDARY }))
  elseif error_msg ~= nil then
    table.insert(footer, ui.spacer({ flexGrow = 1 }))
    table.insert(footer, ui.label({ text = "falha ao atualizar: " .. string.sub(error_msg, 1, 60), fontSize = 9, color = "error" }))
  end
  table.insert(children, ui.row({ key = "footer", gap = 5, align = "center" }, footer))

  panel.render(ui.column({ flexGrow = 1, gap = 8, padding = 14 }, children))
end

local in_flight = false

-- `force`: só o botão de atualizar vai à rede sempre; abertura e pós-sync usam o TTL.
local function fetch(force)
  if in_flight then
    return
  end
  in_flight = true
  loading = true
  error_msg = nil
  render()
  snapshot.fetch(costs_bin, function(res)
    in_flight = false
    loading = false
    if res.exitCode ~= 0 then
      local detail = res.stderr
      if detail == nil or detail == "" then
        detail = res.stdout
      end
      if detail == nil or detail == "" then
        detail = "?"
      end
      -- mantém o último dado bom na tela; o erro aparece no rodapé
      error_msg = string.sub(detail, 1, 200)
      sync_feedback = false
      render()
      return
    end
    local parsed, err = noctalia.json.decode(res.stdout)
    if parsed == nil then
      error_msg = string.sub(err or "?", 1, 200)
      sync_feedback = false
    else
      data = parsed
      error_msg = nil
    end
    render()
  end, force)
end

function onOpen(context)
  load_hidden()
  sync_feedback = false
  selected_provider = noctalia.state.get("selected_provider")
  local cached = noctalia.state.get("status_json")
  if cached ~= nil and cached ~= "" then
    local parsed = noctalia.json.decode(cached)
    if parsed ~= nil and parsed["providers"] ~= nil then
      data = parsed
      loading = false
      error_msg = nil
      render()
    end
  end
  fetch(false)
end

function onRefresh()
  if not syncing then
    fetch(true)
  end
end

function onSync()
  if syncing then
    return
  end
  syncing = true
  sync_feedback = false
  render()
  sync.start(costs_bin, function(res)
    syncing = false
    if res.exitCode ~= 0 then
      noctalia.notifyError("costs sync falhou", res.stderr)
    else
      sync_feedback = true
    end
    fetch(false)
  end)
end

function onIpc(event)
  if event == "sync" then
    onSync()
  elseif event == "report" then
    onReport()
  elseif event == "hide" then
    onHide()
  elseif event == "show" then
    onShow()
  end
end

function onReport()
  -- O HTML abre no navegador: o comando grava o arquivo e chama o opener.
  noctalia.runAsync({ costs_bin, "report", "--format", "html", "--open" }, function(res)
    panel.close()
    if res.exitCode ~= 0 then
      noctalia.notifyError("costs report falhou", res.stderr or res.stdout or "?")
    end
  end)
end

function select_provider(id)
  selected_provider = id
  noctalia.state.set("selected_provider", id)
  render()
end

function onSelectClaude() select_provider("claude") end
function onSelectCodex() select_provider("codex") end
function onSelectXai() select_provider("xai") end
function onSelectZai() select_provider("zai") end
function onSelectDeepseek() select_provider("deepseek") end
function onSelectOpenrouter() select_provider("openrouter") end
function onSelectOpencodeGo() select_provider("opencode-go") end
