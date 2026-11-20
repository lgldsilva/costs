# costs no Noctalia

Widget de barra (`lgldsilva/costs:bar`): gasto do mês via `costs status`
(read-only JSON snapshot, after `costs quota --if-stale`; the panel refresh button forces `--refresh`). Clique abre o painel de detalhes
(`lgldsilva/costs:details`): gasto por provedor, barras de quota (sessão,
semana, mês), ordem de uso, botões Sincronizar e Relatório. Botão direito no
widget abre o relatório do mês em HTML no navegador (`costs report --format
html --open`); a tabela de texto continua no `costs report` do terminal.

## Instalar

```bash
cargo build --release
cp target/release/costs ~/.local/bin/costs
costs install-service --apply       # sync 1h + backup diário (systemd)
# systemctl --user enable --now ... (o comando acima imprime as linhas)

mkdir -p ~/.local/share/noctalia/plugins
ln -s ~/Projetos/costs/noctalia/costs ~/.local/share/noctalia/plugins/costs
noctalia msg plugins enable lgldsilva/costs
```

Adicionar na barra (`~/.config/noctalia/noctalia/*.toml`):

```toml
[widget.costs]
type = "lgldsilva/costs:bar"

[bar.default]
end = ["costs", "tray", ...]
```

Ou pelo picker de widgets, após habilitar o plugin.

## Configuração do costs nesta máquina

Cost history is collected by systemd or the Sync button. Quotas are refreshed
on each widget update (five minutes by default), on panel open/refresh and after
sync. Provider failures retain cached data with an explicit error. No Linux o
`costs` usa `COSTS_HOME` (tudo junto: `config.toml`, `store.sqlite`, ...) ou
os diretórios XDG. O host local desta máquina precisa existir no
`config.toml` (`kind = "local"`); os demais via `ssh`, como no exemplo do
repo. Sem banco, o widget mostra `costs?` com o motivo no tooltip.

## Usage recommendations

The panel shows the shared ranking rationale for each provider: monthly or weekly
allowance, an expiring extra reset, or manual redemption needed after exhaustion.
OpenCode Go is paced against its monthly total. Auxiliary limits still block use;
reset credits are never redeemed automatically. See the repository README for the
scheduling heuristic and its limits.

## Manual sync

The panel runs `costs sync` through `noctalia.runStream`, not captured
`runAsync` (Noctalia 5.3 defaults to a five-second timeout, capped at sixty
seconds). Collection may legitimately exceed both limits. A framed exit code
re-enables the button, reports genuine failures and refreshes the displayed data.
Noctalia cancels the stream when the panel runtime is unloaded or reloaded.

The same handler is available over IPC while the panel is open:

```bash
noctalia msg panel-open lgldsilva/costs:details
noctalia msg plugin lgldsilva/costs:details all sync
```

## Regression tests

The scripts use the Lua-compatible subset of Luau. Tests mock Noctalia's host
APIs and execute real shell commands for slow syncs and quoted binary paths:

```bash
# From the repository root (Lua 5.4+):
lua noctalia/costs/tests/sync_test.lua
lua noctalia/costs/tests/status_test.lua
bash menubar/tests/refresh_test.sh
noctalia plugins lint noctalia/costs
```

## Layout hierarchy (SwiftBar and Noctalia)

Both surfaces lead with **API-estimated consumption**, not an invoice, then a
single “Usar agora” recommendation. Currency is neutral; quota colors encode
pacing only. The oldest collection age is explicitly an aggregate minimum:
it does not imply that every host or quota is frozen.

SwiftBar moves the explanation and ranked alternatives into native submenus.
Provider details stay one level deep: used quota and reset, elapsed-time / pace,
reset credits and recommendation rationale, then machine/harness API estimates.
Prose wraps at 56 characters; machine labels have no artificial column padding.
The OS owns menu fonts, material and transparency; no appearance settings change.

Noctalia mirrors the CostsPreview (macOS) panel layout: a title bar, a
two-metric totals row (API estimate and subscriptions), a compact provider
grid with usage tooltips and "use now" ranks, and one provider card per
selection. The card keeps the macOS structure — monogram badge with brand
color, status/error subtitle, "USAR AGORA #rank", plan row, staleness pill,
error banner, quota windows with used-percent progress bars, balance,
session/weekly reset credits with expiry groups, exhausted-window notice,
ranking rationale, API/subscription metrics and a machine/harness breakdown
(expanded by default, collapsible). Provider selection uses clickable tiles
(closures, plugin_api 9+); the named string handlers (`onSelect*`, onSync,
onRefresh) stay exported for IPC. Custom provider cards remain visible below
the inspector. This keeps
full machine details reachable without silently truncating them or expanding
every supported provider at once.
The card separates balance, errors, reset credits, quota usage/reset and
visible pacing. Elapsed time and projection come from the shared status window
`text`; the separate `hint` remains the action recommendation, not a substitute
for pacing data. Progress bars show **used**, matching the numeric label.
Colors use theme role tokens (`tertiary`, `error`, `outline`) for status
accents and hex brand colors for provider badges, so light and dark themes
both stay readable; provider badges use the same monograms and colors
as the macOS `ProviderCatalog`. All controls use existing Noctalia primitives
and native fonts; no new framework API or global theme is required. The
existing refresh, sync and report actions are retained (the report now opens
as a self-contained HTML page in the browser). This source
change does not install or deploy the plugin.
