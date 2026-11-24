//! Resumo legível por máquina (`costs status`): JSON para o widget do Noctalia.
//!
//! Só leitura: banco em modo readonly (sem migração, sem criar arquivo) e quota
//! só do cache (sem rede). Qualquer falha vira erro de saída, nunca meio-JSON.

use std::error::Error;
use std::io::Write;

use costs_core::config::Config;
use costs_core::fmt::{
    STALE_AFTER_S, age_text, fmt_balance, fmt_duration, fmt_reset, quota_error_hint, quota_error_icon,
    quota_error_text, sync_age,
};
use costs_core::pace::{PaceColor, window_lines};
use costs_core::priority::{
    PriorityKey, ResetAvailability, credits_availability, priority_hint, reset_availability, use_priority,
};
use costs_core::store::Store;
use serde_json::{Value, json};
use std::collections::HashMap;

/// Contexto já resolvido pelo chamador.
pub struct StatusCtx<'a> {
    pub store: &'a Store,
    pub cfg: &'a Config,
    /// Uma entrada de quota por provedor de `cfg.quota.providers`, na mesma ordem.
    pub entries: &'a [Value],
    pub now: f64,
    pub month: &'a str,
}

/// Pares (host, agente) habilitados: a coleta mais antiga dentre eles data o resumo.
pub fn sync_pairs(cfg: &Config) -> Vec<(String, String)> {
    cfg.hosts
        .iter()
        .filter(|h| h.enabled)
        .flat_map(|h| {
            let agents = if h.agents.is_empty() { &cfg.agents } else { &h.agents };
            agents.iter().map(|a| (h.name.clone(), a.clone()))
        })
        .collect()
}

/// Cor do ritmo em texto estável para o widget.
fn color_name(color: PaceColor) -> &'static str {
    match color {
        PaceColor::Red => "red",
        PaceColor::Orange => "orange",
        PaceColor::Green => "green",
        PaceColor::Gray => "gray",
    }
}

/// Texto sugestivo da janela para o painel: diz o estado e o que fazer.
fn window_hint(color: PaceColor, used: f64, spare: Option<f64>, wait: Option<String>) -> String {
    match color {
        PaceColor::Green => match spare {
            Some(s) if s >= 25.0 => format!("ritmo saudável · dá pra usar mais {s:.0}%"),
            _ => "no ritmo".to_owned(),
        },
        PaceColor::Orange => format!("no limite · {used:.0}% já foi"),
        PaceColor::Red => match wait {
            Some(w) => format!("acima do ritmo · pausa {w}"),
            None => "esgotado · aguarde o reset".to_owned(),
        },
        PaceColor::Gray => format!("{used:.0}% usado"),
    }
}
/// Reset mais distante entre as janelas frescas zeradas: quanto falta para normalizar.
/// `None` quando nenhuma janela fresca está esgotada.
fn exhausted_reset(entry: &Value, now: f64) -> Option<String> {
    let empty = Vec::new();
    let latest = entry
        .get("windows")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
        .iter()
        .filter(|w| {
            let fresh = w.get("resets_at").and_then(Value::as_f64).is_none_or(|ra| ra > now);
            let empty_left = w.get("remaining_pct").and_then(Value::as_f64).is_some_and(|r| r <= 0.0);
            fresh && empty_left
        })
        .filter_map(|w| w.get("resets_at").and_then(Value::as_f64))
        .fold(0.0_f64, f64::max);
    if latest > now {
        Some(fmt_reset(Some(latest), now))
    } else {
        None
    }
}

/// Créditos que zeram a cota (Codex/Claude): disponíveis e o vencimento mais próximo.
/// Vencimentos já passados são descartados; sem créditos vira `null`.
pub(crate) fn reset_credits(entry: &Value, now: f64) -> Value {
    credits_json(entry.get("reset_credits"), reset_availability(entry, now), now)
}

/// Mesmo formato para os resets semanais (zai); presente só quando o campo existe.
pub(crate) fn week_reset_credits(entry: &Value, now: f64) -> Option<Value> {
    entry.get("week_reset_credits").map(|value| {
        credits_json(
            Some(value),
            credits_availability(entry.get("week_reset_credits"), now),
            now,
        )
    })
}

fn credits_json(source: Option<&Value>, credits: ResetAvailability, now: f64) -> Value {
    let available = credits.available;
    if available <= 0 {
        return Value::Null;
    }
    let mut out = json!({"available": available});
    if let Some(next_expiry) = credits.next_expiry {
        out["next_expiry"] = json!(fmt_reset(Some(next_expiry), now));
        out["next_expiry_at"] = json!(next_expiry);
    }
    if let Some(groups) = source.and_then(|v| v.get("expiry_groups")).and_then(Value::as_array) {
        out["expiry_groups"] = Value::Array(
            groups
                .iter()
                .filter_map(|g| {
                    let count = g.get("available")?.as_i64()?;
                    let raw = g.get("expires_at_raw")?.as_str()?;
                    (count > 0).then(|| json!({"available": count, "expires_at_raw": raw}))
                })
                .collect(),
        );
    }
    out
}

fn quota_stale(entry: &Value, now: f64) -> bool {
    let data_at = ["data_at", "fetched_at"]
        .iter()
        .filter_map(|k| entry.get(k).and_then(Value::as_f64))
        .find(|v| *v != 0.0);
    let old = data_at.is_some_and(|d| now - d > STALE_AFTER_S);
    entry.get("ok") == Some(&Value::Bool(false)) || old
}

/// Mês anterior a `month` ("YYYY-MM"), com virada de ano.
fn prev_month(month: &str) -> Option<String> {
    let (year, mon) = month.split_once('-')?;
    let year: i32 = year.parse().ok()?;
    let mon: u32 = mon.parse().ok()?;
    if !(1..=12).contains(&mon) {
        return None;
    }
    Some(if mon == 1 {
        format!("{}-12", year - 1)
    } else {
        format!("{year}-{:02}", mon - 1)
    })
}

/// Monta o objeto do `status`: espelha o SwiftBar (`menubar`), em JSON.
/// Um objeto por provedor de quota, na ordem do config: custo do mês (via
/// `cost_map`), saldo, janelas com ritmo (texto, cor, usado, sobra) e quebra
/// por harness. Mais totais, idade da coleta e ordem de uso.
pub fn snapshot(ctx: &StatusCtx<'_>) -> Result<Value, Box<dyn Error>> {
    let spend = ctx.store.month_spend(ctx.month)?;
    let prev = prev_month(ctx.month);
    let prev_spend = match &prev {
        Some(m) => ctx.store.month_spend(m)?,
        None => 0.0,
    };
    let oldest = ctx.store.oldest_ok_sync(&sync_pairs(ctx.cfg))?;
    let (age, stale) = sync_age(oldest.as_deref(), ctx.now);
    let resolve_sub = |provider: &str| {
        crate::subs::resolve(
            provider,
            crate::subs::entry_for(provider, &ctx.cfg.quota.providers, ctx.entries),
            &ctx.cfg.subscriptions,
        )
    };
    let mut providers = Vec::new();
    for (provider, entry) in ctx.cfg.quota.providers.iter().zip(ctx.entries) {
        let keys: Vec<&str> = ctx
            .cfg
            .quota
            .cost_map
            .get(provider)
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let (cost, by_agent) = if keys.is_empty() {
            (None, Vec::new())
        } else {
            let (total, rows) = ctx.store.spend_by_provider(&keys, ctx.month)?;
            (Some(total), rows)
        };
        let lines = window_lines(entry, ctx.now);
        let window_kinds: HashMap<&str, &str> = entry
            .get("windows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|window| Some((window.get("label")?.as_str()?, window.get("kind")?.as_str()?)))
            .collect();
        let color = lines
            .iter()
            .map(|w| w.color)
            .max_by_key(|c| c.rank())
            .unwrap_or(PaceColor::Gray);
        let balance = entry.get("balance").filter(|b| b.is_object());
        let error = entry
            .get("error")
            .and_then(Value::as_str)
            .filter(|_| entry.get("ok").and_then(Value::as_bool) != Some(true));
        providers.push(json!({
            "id": provider,
            "plan": entry.get("plan").and_then(Value::as_str)
                .map(str::trim).filter(|plan| !plan.is_empty()),
            "cost_usd": cost,
            "subscription_usd_month": resolve_sub(provider).and_then(|s| s.usd_month),
            "subscription_state": resolve_sub(provider).map(|s| s.state.as_str()),
            "subscription_source": resolve_sub(provider).and_then(|s| s.source()),
            "balance": balance.map(fmt_balance),
            "ok": entry.get("ok").and_then(Value::as_bool).unwrap_or(false),
            "error": error,
            "error_text": error.map(quota_error_text),
            "error_icon": error.map(quota_error_icon),
            "error_hint": error.and_then(|e| quota_error_hint(e, provider)),
            "stale": quota_stale(entry, ctx.now),
            "exhausted_reset": exhausted_reset(entry, ctx.now),
            "resets": reset_credits(entry, ctx.now),
            "priority_hint": priority_hint(entry, ctx.now),
            "updated": age_text(entry, ctx.now),
            "color": color_name(color),
            "windows": lines.iter().map(|w| json!({
                "label": w.label,
                "kind": window_kinds.get(w.label.as_str()).copied(),
                "text": w.text,
                "color": color_name(w.color),
                "used_pct": w.used,
                "spare_pct": w.spare,
                "reset": w.reset,
                "resets_at": w.resets_at,
                "wait": w.wait_s.map(fmt_duration),
                "hint": window_hint(w.color, w.used, w.spare, w.wait_s.map(fmt_duration)),
            })).collect::<Vec<_>>(),
            "agents": by_agent.iter().map(|(label, usd, tok)| json!({
                "label": label,
                "usd": usd,
                "tokens": tok,
            })).collect::<Vec<_>>(),
        }));
        if let Some(week) = week_reset_credits(entry, ctx.now)
            && let Some(obj) = providers.last_mut().and_then(Value::as_object_mut)
        {
            obj.insert("resets_week".into(), week);
        }
    }
    let mut order: Vec<(usize, PriorityKey)> = ctx
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| (i, use_priority(e, ctx.now, ctx.cfg.quota.horizon)))
        .collect();
    order.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    // Mesma ordem do SwiftBar: provedores ordenados pela prioridade de uso.
    let rank: HashMap<usize, usize> = order.iter().enumerate().map(|(pos, (i, _))| (*i, pos)).collect();
    let mut indexed: Vec<(usize, Value)> = providers.into_iter().enumerate().collect();
    indexed.sort_by_key(|(i, _)| rank.get(i).copied().unwrap_or(usize::MAX));
    let providers: Vec<Value> = indexed.into_iter().map(|(_, v)| v).collect();
    let use_order: Vec<&str> = order
        .iter()
        .filter(|(_, k)| k.0 <= 3)
        .map(|(i, _)| ctx.cfg.quota.providers[*i].as_str())
        .collect();
    let mapped: Vec<&str> = ctx.cfg.quota.cost_map.values().flatten().map(String::as_str).collect();
    let other_providers = ctx.store.other_provider_spend(&mapped, ctx.month)?;
    let free_models = ctx.store.free_usage(ctx.month)?;
    let free_tokens: i64 = free_models.iter().map(|(_, t)| *t).sum();
    let mut subscriptions_total = 0.0;
    for provider in &ctx.cfg.quota.providers {
        if let Some(s) = resolve_sub(provider) {
            subscriptions_total += s.counted_monthly();
        }
    }
    // Assinaturas sem provedor de quota (ou sem provedor) continuam valendo.
    for s in &ctx.cfg.subscriptions {
        let unmatched = s
            .provider
            .as_deref()
            .is_none_or(|p| !ctx.cfg.quota.providers.iter().any(|q| q == p));
        if unmatched {
            subscriptions_total += s.monthly_usd;
        }
    }
    Ok(json!({
        "month": ctx.month,
        "spend_usd": spend,
        "prev_month": prev,
        "prev_month_usd": prev_spend,
        "sync_age": age,
        "stale": stale,
        "subscriptions_usd_month": subscriptions_total,
        "providers": providers,
        "use_order": use_order,
        "free_usage": (free_tokens > 0).then(|| {
            json!({
                "tokens": free_tokens,
                "models": free_models.iter().take(5).map(|(m, t)| json!({
                    "model": m,
                    "tokens": t,
                })).collect::<Vec<_>>(),
            })
        }),
        "other_providers": other_providers.iter().map(|(p, usd, tok)| json!({
            "provider": p,
            "usd": usd,
            "tokens": tok,
        })).collect::<Vec<_>>(),
    }))
}

/// Escreve o JSON em uma linha.
pub fn render(out: &mut dyn Write, ctx: &StatusCtx) -> Result<(), Box<dyn Error>> {
    writeln!(out, "{}", snapshot(ctx)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use costs_core::config::parse_config;
    use costs_core::store::utc_now;
    use serde_json::json;
    use std::io::{Error as IoError, Result as IoResult};

    #[test]
    fn prev_month_walks_back_including_the_year_rollover() {
        assert_eq!(prev_month("2026-11").as_deref(), Some("2026-10"));
        assert_eq!(prev_month("2026-01").as_deref(), Some("2025-12"));
        assert_eq!(prev_month("2026-13"), None);
        assert_eq!(prev_month("sem-mês"), None);
    }

    const CFG: &str = r#"
[quota]
providers = ["deepseek", "zai"]
[quota.cost_map]
deepseek = "deepseek"
[[hosts]]
name = "mac"
[[subscriptions]]
name = "Plano X"
provider = "zai"
monthly_usd = 10.0
"#;

    fn snapshot_with(entries: Vec<Value>) -> Value {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.sqlite"), "", utc_now()).unwrap();
        let cfg = parse_config(CFG).unwrap();
        let ctx = StatusCtx {
            store: &store,
            cfg: &cfg,
            entries: &entries,
            now: 1000.0,
            month: "2026-10",
        };
        snapshot(&ctx).unwrap()
    }

    #[test]
    fn empty_db_has_zero_spend_and_stale_sync() {
        let v = snapshot_with(vec![json!({}), json!({})]);
        assert_eq!(v["month"], "2026-10");
        assert_eq!(v["spend_usd"], 0.0);
        assert_eq!(v["prev_month"], "2026-09");
        assert_eq!(v["prev_month_usd"], 0.0);
        assert_eq!(v["sync_age"], "nunca");
        assert_eq!(v["stale"], true);
        assert_eq!(v["subscriptions_usd_month"], 10.0);
        let providers = v["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0]["id"], "deepseek");
        assert_eq!(providers[0]["plan"], Value::Null);
        assert_eq!(providers[0]["cost_usd"], 0.0);
        assert!(providers[0]["agents"].as_array().unwrap().is_empty());
        assert_eq!(providers[1]["id"], "zai");
        assert_eq!(providers[1]["subscription_usd_month"], 10.0);
        assert!(v["use_order"].as_array().unwrap().is_empty());
    }

    fn snapshot_with_cfg(cfg: &str, entries: Vec<Value>) -> Value {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.sqlite"), "", utc_now()).unwrap();
        let cfg = parse_config(cfg).unwrap();
        let ctx = StatusCtx {
            store: &store,
            cfg: &cfg,
            entries: &entries,
            now: 1000.0,
            month: "2026-10",
        };
        snapshot(&ctx).unwrap()
    }

    fn provider_by_id<'a>(v: &'a Value, id: &str) -> &'a Value {
        v["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap()
    }

    #[test]
    fn free_plan_refutes_subscription_in_total() {
        let v = snapshot_with(vec![json!({}), json!({"plan": "free"})]);
        let zai = provider_by_id(&v, "zai");
        assert_eq!(zai["subscription_usd_month"], Value::Null);
        assert_eq!(zai["subscription_state"], "free");
        assert_eq!(v["subscriptions_usd_month"], 0.0);
    }

    #[test]
    fn detected_paid_plan_confirms_subscription() {
        let v = snapshot_with(vec![json!({}), json!({"plan": "pro"})]);
        let zai = provider_by_id(&v, "zai");
        assert_eq!(zai["subscription_usd_month"], 10.0);
        assert_eq!(zai["subscription_state"], "active");
        assert_eq!(zai["subscription_source"], "config");
        assert_eq!(v["subscriptions_usd_month"], 10.0);
    }

    #[test]
    fn paid_plan_without_config_is_discovered_with_public_price() {
        let cfg = r#"
[quota]
providers = ["codex", "claude"]
[[hosts]]
name = "mac"
"#;
        let v = snapshot_with_cfg(cfg, vec![json!({"plan": "plus"}), json!({"ok": true})]);
        let codex = provider_by_id(&v, "codex");
        assert_eq!(codex["subscription_state"], "active");
        assert_eq!(codex["subscription_source"], "detected");
        assert_eq!(codex["subscription_usd_month"], 20.0);
        // Claude autenticado sem assinatura no config: ativo, sem preço conhecido.
        let claude = provider_by_id(&v, "claude");
        assert_eq!(claude["subscription_state"], "active");
        assert_eq!(claude["subscription_source"], "detected");
        assert_eq!(claude["subscription_usd_month"], Value::Null);
        assert_eq!(v["subscriptions_usd_month"], 20.0);
    }

    #[test]
    fn plan_change_updates_price_from_public_table() {
        let cfg = r#"
[quota]
providers = ["claude"]
[[hosts]]
name = "mac"
[[subscriptions]]
name = "Claude"
provider = "claude"
monthly_usd = 20.0
"#;
        // Troca de Pro para Max 5x: o preço público detectado substitui o config velho.
        let v = snapshot_with_cfg(cfg, vec![json!({"plan": "Max 5x", "ok": true})]);
        let claude = provider_by_id(&v, "claude");
        assert_eq!(claude["subscription_state"], "active");
        assert_eq!(claude["subscription_source"], "detected");
        assert_eq!(claude["subscription_usd_month"], 100.0);
        assert_eq!(v["subscriptions_usd_month"], 100.0);
    }

    #[test]
    fn free_usage_and_other_providers_land_in_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.sqlite"), "", utc_now()).unwrap();
        let mut rows = vec![
            costs_core::model::Row {
                host: "mac".into(),
                agent: "opencode".into(),
                date: "2026-10-01".into(),
                model: "deepseek-v4.1-flash".into(),
                model_raw: "deepseek-v4.1-flash-free".into(),
                provider: "deepseek".into(),
                input: 2_000_000,
                output: 1_000_000,
                cache_read: 0,
                cache_write: 0,
                cost_usd: None,
                cost_basis: "free".into(),
            },
            costs_core::model::Row {
                host: "mac".into(),
                agent: "opencode".into(),
                date: "2026-10-02".into(),
                model: "kimi-k3".into(),
                model_raw: "kimi-k3".into(),
                provider: "moonshot".into(),
                input: 100_000,
                output: 200_000,
                cache_read: 0,
                cache_write: 0,
                cost_usd: Some(4.0),
                cost_basis: "reported".into(),
            },
        ];
        store.upsert_rows(&rows, "2026-10-02T00:00:00+00:00").unwrap();
        rows.clear();
        let _ = rows;
        let cfg = parse_config(CFG).unwrap();
        let entries = [json!({}), json!({})];
        let ctx = StatusCtx {
            store: &store,
            cfg: &cfg,
            entries: &entries,
            now: 1000.0,
            month: "2026-10",
        };
        let v = snapshot(&ctx).unwrap();
        let free = v["free_usage"].as_object().unwrap();
        assert_eq!(free["tokens"], 3_000_000);
        assert_eq!(free["models"][0]["model"], "deepseek-v4.1-flash");
        let others = v["other_providers"].as_array().unwrap();
        assert_eq!(others.len(), 1);
        assert_eq!(others[0]["provider"], "moonshot");
        assert_eq!(others[0]["usd"], 4.0);
    }

    #[test]
    fn free_plan_without_config_is_silent() {
        let cfg = r#"
[quota]
providers = ["codex"]
[[hosts]]
name = "mac"
"#;
        let v = snapshot_with_cfg(cfg, vec![json!({"plan": "free"})]);
        assert_eq!(provider_by_id(&v, "codex")["subscription_state"], "free");
        assert_eq!(v["subscriptions_usd_month"], 0.0);
    }

    #[test]
    fn missing_plan_keeps_subscription_unverified_and_counted() {
        let v = snapshot_with(vec![json!({}), json!({})]);
        let zai = provider_by_id(&v, "zai");
        assert_eq!(zai["subscription_usd_month"], 10.0);
        assert_eq!(zai["subscription_state"], "unverified");
        assert_eq!(v["subscriptions_usd_month"], 10.0);
        assert_eq!(provider_by_id(&v, "deepseek")["subscription_state"], Value::Null);
    }

    #[test]
    fn plan_is_trimmed_and_missing_blank_or_non_string_values_are_null() {
        let v = snapshot_with(vec![json!({"plan": "  Pro  "}), json!({"plan": "  "})]);
        let providers = v["providers"].as_array().unwrap();
        let deepseek = providers.iter().find(|p| p["id"] == "deepseek").unwrap();
        let zai = providers.iter().find(|p| p["id"] == "zai").unwrap();
        assert_eq!(deepseek["plan"], "Pro");
        assert_eq!(zai["plan"], Value::Null);

        for invalid in [json!({"plan": 12}), json!({"plan": false}), json!({})] {
            let snapshot = snapshot_with(vec![invalid, json!({})]);
            let providers = snapshot["providers"].as_array().unwrap();
            assert!(providers.iter().all(|p| p["plan"].is_null()));
        }
    }

    #[test]
    fn quota_windows_and_pace_land_in_snapshot() {
        let v = snapshot_with(vec![
            json!({"ok": true, "fetched_at": 990.0, "balance": {"amount": 5.0, "currency": "USD"},
                "windows": [{"label": "5h", "remaining_pct": 80, "resets_at": 1000.0 + 90000.0,
                             "window_s": 180000.0}]}),
            json!({"ok": true, "fetched_at": 990.0,
                "windows": [{"label": "5h", "remaining_pct": 50, "resets_at": 1000.0 + 3600.0,
                             "window_s": 18000.0}]}),
        ]);
        // Ordem do SwiftBar: zai (reseta dentro do horizonte) antes de deepseek.
        let providers = v["providers"].as_array().unwrap();
        assert_eq!(providers[0]["id"], "zai");
        assert_eq!(providers[1]["id"], "deepseek");
        let deepseek = &providers[1];
        assert_eq!(deepseek["balance"], "US$ 5.00");
        assert_eq!(deepseek["stale"], false);
        assert_eq!(deepseek["color"], "green");
        let wins = deepseek["windows"].as_array().unwrap();
        assert_eq!(wins.len(), 1);
        assert_eq!(wins[0]["label"], "5h");
        assert_eq!(wins[0]["used_pct"].as_f64().unwrap(), 20.0);
        assert!(wins[0]["text"].as_str().unwrap().contains("usado"));
        assert_eq!(wins[0]["resets_at"].as_f64().unwrap(), 1000.0 + 90000.0);
        let zai = &providers[0];
        assert_eq!(zai["stale"], false);
        assert_eq!(zai["windows"].as_array().unwrap().len(), 1);
        assert!(zai["windows"][0]["kind"].is_null());
        assert_eq!(v["use_order"], json!(["zai", "deepseek"]));
    }

    #[test]
    fn status_forwards_zai_window_kinds() {
        let v = snapshot_with(vec![
            json!({}),
            json!({"ok": true, "windows": [
                {"label": "5h", "kind": "session", "remaining_pct": 90, "resets_at": 4000},
                {"label": "sem", "kind": "week", "remaining_pct": 80, "resets_at": 5000},
                {"label": "mês", "kind": "month", "remaining_pct": 70, "resets_at": 6000},
                {"label": "17m", "kind": "other", "remaining_pct": 60, "resets_at": 7000}
            ]}),
        ]);
        let zai = v["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "zai")
            .unwrap();
        assert_eq!(
            zai["windows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| w["kind"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["session", "week", "month", "other"]
        );
    }

    #[test]
    fn failed_quota_retains_values_but_is_excluded_from_recommendations() {
        let v = snapshot_with(vec![
            json!({"ok": false, "error": "expired", "windows": [
                {"label": "sem", "remaining_pct": 100, "resets_at": 4600}]}),
            json!({"ok": true, "windows": [
                {"label": "sem", "remaining_pct": 10, "resets_at": 361000}]}),
        ]);
        assert_eq!(v["use_order"], json!(["zai"]));
        let retained = &v["providers"][1];
        assert_eq!(retained["id"], "deepseek");
        assert_eq!(retained["error"], "expired");
        assert_eq!(retained["windows"][0]["used_pct"], 0.0);
        assert!(retained["priority_hint"].as_str().unwrap().contains("consulta de cota"));
    }

    #[test]
    fn snapshot_order_and_hints_share_monthly_and_extra_reset_policy() {
        let v = snapshot_with(vec![
            json!({"windows": [
                {"label": "sem", "remaining_pct": 90, "resets_at": 2800},
                {"label": "mês", "remaining_pct": 10, "resets_at": 201000}]}),
            json!({"windows": [{"label": "sem", "remaining_pct": 60, "resets_at": 361000}],
                "reset_credits": {"available": 2, "expires": [4600]}}),
        ]);
        assert_eq!(v["use_order"], json!(["zai", "deepseek"]));
        assert!(
            v["providers"][0]["priority_hint"]
                .as_str()
                .unwrap()
                .contains("reset extra")
        );
        assert_eq!(v["providers"][1]["priority_hint"], "Ordem pela cota mensal");
    }

    #[test]
    fn blocked_provider_with_credits_stays_out_of_use_order_until_redemption() {
        let v = snapshot_with(vec![
            json!({"windows": [{"label": "5h", "remaining_pct": 0, "resets_at": 4600}],
                "reset_credits": {"available": 1, "expires": [8200]}}),
            json!({}),
        ]);
        assert!(v["use_order"].as_array().unwrap().is_empty());
        let p = v["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "deepseek")
            .unwrap();
        assert!(p["priority_hint"].as_str().unwrap().contains("resgate manual"));
    }

    #[test]
    fn window_hint_suggests_action_per_state() {
        assert_eq!(
            window_hint(PaceColor::Green, 1.0, Some(96.0), None),
            "ritmo saudável · dá pra usar mais 96%"
        );
        assert_eq!(window_hint(PaceColor::Green, 10.0, None, None), "no ritmo");
        assert_eq!(
            window_hint(PaceColor::Orange, 78.0, None, None),
            "no limite · 78% já foi"
        );
        assert_eq!(
            window_hint(PaceColor::Red, 90.0, None, Some("25m".to_owned())),
            "acima do ritmo · pausa 25m"
        );
        assert_eq!(
            window_hint(PaceColor::Red, 100.0, None, None),
            "esgotado · aguarde o reset"
        );
    }

    #[test]
    fn reset_credits_reports_count_and_nearest_future_expiry() {
        let v = reset_credits(
            &json!({"reset_credits": {"available": 3, "expires": [500.0, 5000.0, 2000.0]}}),
            1000.0,
        );
        assert_eq!(v["available"], 2);
        assert_eq!(v["next_expiry_at"], 2000.0);
        assert!(v["next_expiry"].is_string());
        assert!(reset_credits(&json!({"reset_credits": {"available": 0}}), 0.0).is_null());
        assert!(reset_credits(&json!({}), 0.0).is_null());
        let no_expiry = reset_credits(&json!({"reset_credits": {"available": 1}}), 0.0);
        assert_eq!(
            (no_expiry["available"].as_i64(), no_expiry["next_expiry"].is_null()),
            (Some(1), true)
        );
    }

    #[test]
    fn unknown_zai_expiries_keep_provider_count_and_omit_normalized_dates() {
        let entry = json!({"reset_credits": {
            "available": 2,
            "expiry_groups": [
                {"available": 2, "expires_at_raw": "2026-10-28 11:02:31"}
            ]
        }});
        let rendered = reset_credits(&entry, 9_999_999_999.0);
        assert_eq!(rendered["available"], 2);
        assert!(rendered.get("next_expiry").is_none());
        assert!(rendered.get("next_expiry_at").is_none());
        assert_eq!(rendered["expiry_groups"][0]["expires_at_raw"], "2026-10-28 11:02:31");

        // Legacy cache can have timezone-guessed epochs. Raw z.ai groups signal
        // that these epochs must not hide the provider-reported available count.
        let cached = json!({"reset_credits": {
            "available": 2,
            "expires": [500, 900],
            "expiry_groups": [
                {"available": 2, "expires_at_raw": "2026-10-28 11:02:31"}
            ]
        }});
        let rendered = reset_credits(&cached, 1000.0);
        assert_eq!(rendered["available"], 2);
        assert!(rendered.get("next_expiry_at").is_none());
    }

    #[test]
    fn expired_reset_credits_do_not_remain_available() {
        let entry = json!({"reset_credits": {"available": 2, "expires": [900.0, 1100.0]}});
        assert_eq!(reset_credits(&entry, 1000.0)["available"], 1);
        assert!(reset_credits(&entry, 1200.0).is_null());
    }

    #[test]
    fn resets_week_mirrors_resets_only_when_present() {
        let with_week = json!({
            "reset_credits": {"available": 2, "expires": [2000.0]},
            "week_reset_credits": {"available": 3, "expires": [5000.0, 4000.0]},
        });
        let week = week_reset_credits(&with_week, 1000.0).expect("presente");
        assert_eq!(week["available"], 3);
        assert_eq!(week["next_expiry_at"], 4000.0);
        assert!(week["next_expiry"].is_string());
        // sem o campo no cache (v1 antigo) a chave não aparece no snapshot
        assert!(week_reset_credits(&json!({"reset_credits": {"available": 1}}), 0.0).is_none());
        let v = snapshot_with(vec![with_week, json!({})]);
        let providers = v["providers"].as_array().unwrap();
        let deepseek = providers.iter().find(|p| p["id"] == "deepseek").unwrap();
        assert!(deepseek.get("resets_week").is_some_and(|r| r["available"] == 3));
        let zai = providers.iter().find(|p| p["id"] == "zai").unwrap();
        assert!(zai.get("resets_week").is_none(), "ausente não vira null");
        // credits esgotados viram null, como o campo `resets`
        let gone = week_reset_credits(
            &json!({"week_reset_credits": {"available": 1, "expires": [500.0]}}),
            1000.0,
        );
        assert_eq!(gone, Some(Value::Null));
    }

    #[test]
    fn exhausted_reset_reports_longest_zeroed_window() {
        let v = snapshot_with(vec![
            json!({"ok": true, "fetched_at": 990.0,
            "windows": [
                {"label": "5h", "remaining_pct": 0, "resets_at": 1000.0 + 3600.0,
                 "window_s": 18000.0},
                {"label": "sem", "remaining_pct": 0, "resets_at": 1000.0 + 100000.0,
                 "window_s": 604800.0},
                {"label": "old", "remaining_pct": 0, "resets_at": 500.0,
                 "window_s": 18000.0},
            ]}),
            json!({}),
        ]);
        let providers = v["providers"].as_array().unwrap();
        assert_eq!(providers[0]["exhausted_reset"], "1d3h");
        assert_eq!(providers[1]["exhausted_reset"], Value::Null);
    }

    #[test]
    fn render_writes_single_line_json() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.sqlite"), "", utc_now()).unwrap();
        let cfg = parse_config(CFG).unwrap();
        let entries = vec![json!({}), json!({})];
        let ctx = StatusCtx {
            store: &store,
            cfg: &cfg,
            entries: &entries,
            now: 1000.0,
            month: "2026-10",
        };
        let mut out = Vec::new();
        render(&mut out, &ctx).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["month"], "2026-10");
    }

    #[test]
    fn io_errors_are_propagated_not_panics() {
        struct Fail;
        impl Write for Fail {
            fn write(&mut self, _b: &[u8]) -> IoResult<usize> {
                Err(IoError::other("disco"))
            }
            fn flush(&mut self) -> IoResult<()> {
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.sqlite"), "", utc_now()).unwrap();
        let cfg = parse_config(CFG).unwrap();
        let entries = vec![json!({}), json!({})];
        let ctx = StatusCtx {
            store: &store,
            cfg: &cfg,
            entries: &entries,
            now: 1000.0,
            month: "2026-10",
        };
        assert!(render(&mut Fail, &ctx).is_err());
    }
}
