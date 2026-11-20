# Costs Prévia (macOS) — backlog a partir da versão Noctalia

Features implementadas no plugin Noctalia (`noctalia/costs/`) que o app
Swift ainda não tem. Em ordem de valor percebido; cada item diz onde a
versão Noctalia o resolve.

## UI

1. **Rank de uso no seletor de providers** — os tiles mostram `#1 … #N`
   (ordem do `use_order` do `costs status`) junto ao nome. Noctalia:
   `panel.luau` (`provider_grid`, `rank_of`).
2. **Ocultar/mostrar providers pela interface** — cada card tem um botão
   👁‍🗨 que esconde o provider (estado persistido entre aberturas) e um
   botão "Mostrar N providers ocultos" restaura. Catálogo fixo do app não
   permite remoção. Noctalia: `panel.luau` (`hide_provider`, `onShow`,
   estado `hidden_providers`).
3. **Persistência do provider selecionado** — reabrir o painel volta no
   provider que estava selecionado. (O PreviewModel já mantém o
   `selectedID` em memória; falta sobreviver a relançamentos.) Noctalia:
   estado `selected_provider`.
4. **Barra: círculos de ritmo por tipo de janela** — além do gauge, os
   círculos coloridos (mês/semana/5h) do provider recomendado acompanham o
   gasto na barra. Noctalia: `widget.luau` (`dots_for`).
5. **Ações scriptáveis** — `sync`, `report`, `hide`, `show` via IPC de
   plugin; no macOS o equivalente seria um URL scheme (`costspreview://`)
   ou subcomandos do binário. Noctalia: `onIpc`.

## Providers novos (kimi plan, qwen coding, fireworks, …)

Novos provedores exigem coletor de quota no Rust (`costs-core/quota/`) e
entrada no catálogo; a UI dos dois lados já mostra ids desconhecidos com
fallback genérico. Backlog Rust (comum):

- [ ] Coletor `quota/kimi.rs` (plano Kimi — token OAuth + janelas)
- [ ] Coletor `quota/qwen.rs` (Qwen Coding plan)
- [ ] Coletor `quota/fireworks.rs` (créditos/saldo Fireworks)
- [ ] `provider_name`/catálogo com nome, monograma e cor de marca para os
      novos ids (menubar.rs, CostsPreview `ProviderCatalog`, Noctalia
      `CATALOG`)

Reaproveitamento: o contrato é o JSON de `costs status` (`providers[]`
com `windows[]`, `resets`, `agents`); uma vez o coletor preenchendo esse
contrato, nenhum dos três front-ends muda.
