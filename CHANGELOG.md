# Changelog

## Unreleased

- CostsPreview como item de barra nativo: refresh automático das cotas a cada 5 min
  (o título da barra fica fresco sem abrir o painel; antes só atualizava ao abrir o
  popover) e o app carrega `~/.config/agentic-api-keys.env` no ambiente dos `costs`
  que ele executa — o mesmo contrato que o `costs-env.sh` dava ao SwiftBar/launchd.
- Cobertura dos caminhos de datas e exportação do relatório: presets de calendário
  (`--period`, `--last-days`) e ambiguidade agora têm regressão no CLI; destino
  privado do dashboard (`default_report_path`) e recriação do diretório de reports
  com permissão 0700 têm teste; falha de escrita não deixa arquivo temporário;
  payload de planos valida `quota.cost_map` (string ou lista) e badges de
  confiança (OpenCode Go inconclusivo).
- Browser smoke sobre exportação real (`tests/real-dashboard-smoke.cjs`): gera o
  dashboard com o binário de verdade a partir de um snapshot do store e valida no
  Chromium filtros combináveis, drilldown diário, picos, planos, frescor e
  viewports 1200/390, garantindo documento offline sem recursos externos.
  Mantido fora do CI; requer instalação própria do Playwright via `NODE_PATH`.
- `costs quota --if-stale`: só vai à rede para o que passou do TTL do cache. Timer e
  abertura do painel do Noctalia, ciclo de 5 min do SwiftBar e abertura do app macOS
  usam `--if-stale`; só os botões de atualizar forçam `--refresh` (antes cada um disso
  mandava 8–13 requisições autenticadas a endpoints não documentados). O painel do
  Noctalia mantém os dados na tela durante a atualização e depois de uma falha.
- Créditos de reset do z.ai: o fluxo não documentado roda no máximo a cada 6 h
  (`COSTS_ZAI_RESETS=0` desliga). Codex: login expirado aparece como erro em vez de um
  rollout de até 8 dias mostrado como atual.
- pi: `cost.total = 0` com tokens não conta mais como custo informado (planos como o
  qwen-token-plan escondiam o gasto equivalente).
- `costs sync` e `costs backup` têm instância única (lock do kernel); o timeout da
  coleta remota mata a árvore de processos; units do systemd ganham
  `TimeoutStartSec=15min`, `Nice=10` e `IOSchedulingClass=idle`, e a coleta remota roda
  com `nice`/`ionice`.
- Sync não regrava mais custo por ruído de ponto flutuante, NULL ou tabela de preços do
  dia: com os mesmos tokens o custo gravado só muda para preencher um NULL ou subir de
  base (`estimated` → `reported`); reprecificar o histórico fica com `costs repricing`
  (que agora pula linhas sem mudança). Tentativa de encolher um dia é registrada uma vez
  só. No banco real, o `usage_history` caiu de ~56 linhas de ruído por sync para só o
  crescimento real. Índice `runs(host, agent, ts)` tira a consulta de frescor do
  dashboard do custo quadrático.
- `prices.json`: com um catálogo fora do ar, os modelos que dependiam dele mantêm o
  preço do arquivo anterior em vez de virar NULL por um dia; preço `NaN`/`inf` é
  descartado (antes gerava um arquivo ilegível que travava todo sync); o sync regenera
  um `prices.json` ilegível e mostra os avisos da atualização.
- claude (quota): o token é procurado no `~/.claude/.credentials.json` **e** no item
  "Claude Code-credentials" do Keychain, e vale o que estiver válido — o access token do
  arquivo expira em ~8h e um `/login` pode renovar só o Keychain, o que deixava a quota
  presa em "login expirado" mesmo com login feito.
- visibilidade: provedores de custo fora do `cost_map` (ex.: Moonshot via OpenCode,
  Meta, Google, Alibaba) ganham a seção "Outros provedores" no menu e o campo
  `other_providers` do status, com custo e tokens de cada um. Uso gratuito
  (`cost_basis='free'`, ex.: modelos `:free`/`-free` do OpenCode) entra como "Bônus
  free · N M tokens sem custo" com os modelos por volume, e no campo `free_usage`
  do status — antes esse consumo só aparecia somado no total.
- antigravity: novo provedor de quota `antigravity` que lê o payload gravado pelo
  statusline do ai-standards no cache compartilhado (`antigravity-payload.json`,
  sem rede e sem email). Os buckets vêm por família de modelo — `gemini-*` para
  Gemini e `3p-*` para Claude/GPT de terceiros — e as duas quotas aparecem com o
  prefixo da família (`gem sem`, `3p sem`); o `plan_tier` do payload vai para
  `plan`. A cota local nasce no terminal onde o Antigravity roda e o costs a
  publica no SwiftBar/Noctalia.
- claude (quota): o plano contratado agora é detectado no `/api/oauth/profile`
  (`rate_limit_tier`: Pro, Max 5x, Max 20x) e vai para o `plan` do entry — best
  effort, falha do profile não derruba as janelas de uso.
- assinaturas: o plano contratado agora é descoberto dinamicamente pela quota. O
  rótulo detectado e o estado da credencial decidem a assinatura de cada provedor:
  plano pago detectado ou credencial válida em provedor só-assinatura (OpenCode Go)
  = ativa; plano gratuito = refutada; **provedor só-assinatura com credencial
  recusada (401/403) = `lapsed`** (não renovada, sai do total); sem rótulo e com
  login expirado = mantém o valor do config. O preço vem do `[[subscriptions]]`
  com filtro de plano (`plan = "max 5x"`, override específico), senão da tabela de
  preços públicos (Codex plus US$20/pro US$200; Claude Pro US$20/Max 5x
  US$100/Max 20x US$200; SuperGrok US$30; OpenCode Go US$10), senão do config
  default — então uma troca de plano (ex.: Claude Pro → Max 5x) atualiza o valor
  sozinha em vez de propagar o preço velho; rótulo pago fora da tabela em provedor
  tabelado (ex.: Codex `go`) fica "ativo (preço n/d)" em vez de arrastar o default
  do config. Planos pagos sem entrada no config aparecem sozinhos. O status ganha `subscription_state`
  (`active`/`free`/`lapsed`/`unverified`) e `subscription_source`
  (`config`/`detected`); o SwiftBar lista descobertas como "plano X detectado",
  não renovadas como "assinatura inativa" e mostra o plano no título do provedor.
- opencode: o coletor lê só os últimos 60 dias (`COSTS_OPENCODE_LOOKBACK_DAYS`, `0` = tudo)
  pelo `time_created` indexado, e só os campos de uso (`json_extract` no sqlite) em vez do
  JSON inteiro de cada mensagem. On an HDD-backed home directory (667 MB sample) a full scan
  travava o `opencode serve` além do heartbeat de 45s e o cliente reiniciava o serviço em
  loop; a coleta caiu de 1,24 s para 0,14 s e de 175 MB para 14 MB de RAM.
- `costs collect` segura um lock por host (também no modo `--context` do sync): coleta
  presa em I/O não acumula cópias a cada tick do timer; a concorrente sai com código 3.
- Add offline HTML analytics with combinable exact provider/harness/machine/model
  filters, calendar and custom date ranges, daily charts, and separate day/week
  cost and token peaks. Show scoped estimated plan economics, missing prices and
  per-agent freshness; export reports atomically to a private location when opened.
- Redesign SwiftBar and Noctalia menus for compact, readable usage inspection;
  add a secure monthly HTML action and gate dashboard regressions in CI.
- Security: update locked rustls-webpki from 0.103.15 to 0.103.16 to fix
  distrusted-intermediate TLS validation (RUSTSEC-2026-0382).

- Exclude failed quota queries from usage recommendations while retaining last
  good values for display. An expired Mac xAI login must not put stale quota data
  at the top of the order.

- Usage order now prioritizes monthly total allowances before weekly ones (OpenCode
  Go), while exhausted auxiliary windows still block recommendations. Available
  reset credits advance the consumption deadline when they expire sooner than the
  natural reset; reserve count breaks ties without inventing extra quota. Ignore
  known expired credits in both ranking and display. Noctalia and SwiftBar explain
  the priority basis and flag manual redemption for exhausted providers.

- Fix OpenCode Go quota after API key rotation: a rejected environment key no longer
  masks a working `opencode-go` key in Pi/OpenCode. Try distinct configured keys in
  the existing priority order, only after authentication rejection; never use a
  generic Zen key or rewrite credential files. Identify usage requests explicitly
  to avoid Cloudflare error 1010, and classify that client block as HTTP rather
  than invalid credentials.

- `costs sync` não fica mais pendurado até 15 min num host que aceita conexão e não
  responde (notebook que dormiu no meio da coleta, rota preta no segundo salto do
  ProxyJump, mux ControlMaster parado): o ssh ganha `ServerAliveInterval=5` com
  `ServerAliveCountMax=3` (aborta em ~15s sem resposta do host) e o teto por coleta
  cai de 900s para 180s (a coleta mais pesada observada leva ~90s).
- Fix Noctalia's Sync button killing a healthy collection after five seconds:
  Noctalia 5.3 defaults captured `runAsync` calls to a five-second timeout, capped
  at sixty seconds even with an override. The panel now uses `runStream` with an
  explicit exit marker, preserving real failures and allowing retries. Binary paths
  are shell-quoted, diagnostics are bounded, and plugin regression tests run in CI.
- Restore session usage on Noctalia and SwiftBar: refresh quotas before rendering
  the read-only snapshot. Previously both interfaces only read stale quota caches,
  so expired five-hour windows disappeared even after a successful cost sync.
  Authentication failures remain visible with last good data, rather than zero usage.

- Preço oficial do fornecedor como fonte principal: `costs prices` baixa a tabela do
  LiteLLM e usa só as entradas do fornecedor oficial; o OpenRouter fica de fallback para o
  que não tem preço oficial. O preço do modelo no OpenRouter oscilava com o roteamento
  (GLM-5.3 entre US$ 0,08 e US$ 0,70/M no mesmo dia), e Mac e CachyOS calculavam diferente.
  `resolved.<modelo>.source` registra a origem.
- `costs sync` atualiza o `prices.json` quando ele tem mais de um dia (best-effort).
- Erros de cota específicos: `nocred` (token não encontrado), `expired` (login
  expirado), `nokey` (API key ausente), `auth` (401/403, credencial recusada), além de
  `http` e `parse`. `costs quota`, `costs status` (`error_text`, `error_icon`,
  `error_hint`), o painel do Noctalia e o menu do SwiftBar mostram o texto e como
  resolver (ex.: "faça login: grok login").
- Consumo por provedor passa a quebrar por máquina: `machine-a / pi / xai = US$ …`.
- Créditos de reset da cota ("Limit Reset Credits"): Codex via
  `wham/rate-limit-reset-credits` e Claude via bloco `cedar_ember` do `oauth/usage`
  (ambos não documentados, best-effort). Aparecem em `costs status` (`resets`), no painel
  do Noctalia e no menu do SwiftBar com a quantidade e o vencimento mais próximo.
  Só leitura: o resgate continua no app do provedor.
- Grok: `inputTokens` do xAI já inclui o cache lido; o coletor agora separa a entrada do
  cache (como no zcode). Antes o cache era cobrado duas vezes na estimativa: o
  `grok-4.7-build` de outubro saía ~US$ 700 em vez de ~US$ 80.
- Custo informado pelo próprio agente (`costUsdTicks` do Grok, custo gravado pelo
  opencode/pi) ganha a base `reported`, e `costs repricing --all` não o sobrescreve mais
  (antes trocava o custo real por estimativa de tabela).
- Protocolo de `costs collect` sobe para v3 e a versão para 0.3.1: atualize o binário
  dos hosts ssh junto.
- `costs sync` deixou de substituir o dia inteiro: faz merge monotônico (dia nunca
  encolhe). Fonte menor (sessão arquivada/apagada, dia incompleto) mantém o valor gravado
  e registra a tentativa em `usage_history`. `costs sync --replace` aplica a correção de
  propósito, guardando o valor antigo no histórico.
- Schema v3: tabela append-only `usage_history` (ts, origem, valores, `kept`) registrando
  o que o sync, o `--replace` e o `repricing` viram. `costs history` lista as observações.
- `costs prices` deixa de depender de `[[price_map]]` manual: resolve todos os modelos do
  banco no catálogo do OpenRouter (slug exato, normalização de prefixo/sufixo, `.`↔`-`,
  irmão mais próximo do mesmo provedor) e grava `prices.json` indexado pelo nome canônico.
  `[[price_map]]` vira override explícito.
- Modelos com rota `-free`/`:free` passam a custar zero (antes ficavam sem preço e podiam
  receber preço de catálogo por engano). Preço `-1` de roteadores `auto` é descartado.
- `costs repricing [--month] [--all] [--apply]`: recalcula o custo de linhas já gravadas
  (dry-run por padrão, backup antes do apply, `allocated` intocado).
- `costs breakdown`: modelo sem preço deixou de sumir em silêncio; aparece em uma seção
  com tokens. Custo vindo de irmão aproximado é marcado com `*`.
- `costs doctor`: avisa quando o banco tem linhas sem preço, com tokens e dica do
  `repricing`.
- Protocolo de `costs collect` sobe para v2 (o `prices.json` mudou de chave; atualize o
  binário dos hosts ssh).

- Sem `ccusage` e sem Python: hosts `ssh` rodam o próprio `costs` instalado em
  `~/.local/bin/costs` via novo subcomando `costs collect` (uma chamada ssh por host;
  contexto com `models.toml` e `prices.json` por stdin, linhas por agente em JSON
  versionado no stdout). Hosts locais usam os coletores nativos para todos os agentes,
  inclusive o `grok`.
- Removidos: `remote/grok_collect.py`, o subcomando `grok-collect` (use
  `costs collect --agent grok`) e o suporte a `ccusage`. `source = "ccusage"` no
  `config.toml` agora é erro de configuração com instrução de migração.
- `costs doctor`: para hosts ssh confere `~/.local/bin/costs --version` (FALHA se
  ausente/inacessível, AVISO se a versão diferir da local); checagens de `ccusage` saíram.
- `costs breakdown`: a coluna `ccusage` passou a se chamar `coletado`.
- `costs status`: resumo em JSON (gasto do mês, idade da coleta, provedores com
  saldo, quotas e ordem de uso) para o widget do Noctalia. Só leitura: banco em
  modo readonly e quota só do cache, sem rede.
- Plugin de barra do Noctalia em `noctalia/costs` (`lgldsilva/costs:bar`): texto
  `≈ US$ X · idade` na barra, tooltip com provedores/saldos, clique sincroniza,
  botão direito abre o relatório no terminal.
- `costs-tray`: `Action::id/from_id` restritos a macOS/Windows (eram dead code no
  Linux e quebravam o clippy com `-D warnings`).

- Migração completa para Rust: o binário `costs` (workspace `crates/`) substitui o pacote
  Python (`costlib/`, `costs.py`, testes e `pyproject.toml` removidos). O coletor do Grok
  local agora é nativo (`grok-collect`), sem Python.
- `grok_collect.py` virou `remote/grok_collect.py`: ativo apenas para hosts remotos, enviado
  por `ssh ... python3 -` (o host remoto ainda não tem binário Rust).
- LaunchAgents (`costs` LaunchAgents) e o plugin SwiftBar chamam o binário Rust
  (`$COSTS_BIN`, `~/.local/bin/costs` ou `target/release/costs`); instância única por lock.
- Novos subcomandos: `grok-collect`, `migrate`, `install-service`, `uninstall-service`;
  app de bandeja `costs-tray`.
- CI: removido o job Python (ruff/pytest); gate `ci` depende de `rust` e `secrets-scan`.
