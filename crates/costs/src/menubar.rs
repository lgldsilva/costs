//! Saída legada do SwiftBar (`costs menubar`), igual à do `menubar.py`.

use std::collections::HashMap;
use std::io::{self, Write};

use costs_core::config::Config;
use costs_core::fmt::{
    STALE_AFTER_S, age_text, fmt_balance, quota_error_hint, quota_error_icon, quota_error_text, sync_age,
};
use costs_core::pace::{PaceColor, window_lines};
use costs_core::priority::{PriorityKey, priority_hint, use_priority};
use costs_core::store::Store;
use serde_json::Value;

use crate::util::commas;

const SECS_PER_HOUR: f64 = 3600.0;
const SECS_PER_DAY: f64 = 86400.0;
const MIN_DAYS_LABEL_S: f64 = 172_800.0;
const MAX_USABLE_TIER: u8 = 3;
const TOKENS_PER_MILLION: f64 = 1e6;
const DETAIL_WIDTH: usize = 56;
// SwiftBar's documented light/dark color pair also keeps passive rows readable.
const NEUTRAL_COLOR: &str = "color=#202124,#F2F2F7";

fn provider_name(provider: &str) -> &str {
    match provider {
        "claude" => "Claude",
        "codex" => "Codex",
        "zai" => "z.ai",
        "xai" => "xAI",
        "opencode-go" => "OpenCode Go",
        other => other,
    }
}

/// Tudo que o menu precisa, já resolvido pelo chamador.
pub struct MenuCtx<'a> {
    pub store: &'a Store,
    pub cfg: &'a Config,
    /// Uma entrada de quota por provedor de `cfg.quota.providers`, na mesma ordem.
    pub entries: &'a [Value],
    pub now: f64,
    pub month: &'a str,
    pub horizon_s: f64,
    /// Executável chamado pelas ações do menu.
    pub exe: &'a str,
}

fn menu_line(text: &str, action: Option<&str>) -> String {
    let safe = text.replace('|', "/");
    match action {
        Some(a) => format!("{safe} | {a}"),
        None => format!("{safe} | {NEUTRAL_COLOR}"),
    }
}

/// Wrap prose inside one provider submenu, never another nested flyout.
fn detail(out: &mut dyn Write, text: &str, action: Option<&str>) -> io::Result<()> {
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + word.chars().count() + 1 > DETAIL_WIDTH {
            writeln!(out, "{}", menu_line(&format!("--{line}"), action))?;
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        writeln!(out, "{}", menu_line(&format!("--{line}"), action))?;
    }
    Ok(())
}

fn fmt_horizon(seconds: f64) -> String {
    if seconds >= MIN_DAYS_LABEL_S && seconds % SECS_PER_DAY == 0.0 {
        return format!("{:.0} dias", seconds / SECS_PER_DAY);
    }
    format!("{}h", seconds / SECS_PER_HOUR)
}

fn non_empty_balance(entry: &Value) -> Option<&Value> {
    entry
        .get("balance")
        .filter(|b| b.as_object().is_some_and(|o| !o.is_empty()))
}

fn is_stale(entry: &Value, now: f64) -> bool {
    let data_at = ["data_at", "fetched_at"]
        .iter()
        .filter_map(|k| entry.get(k).and_then(Value::as_f64))
        .find(|v| *v != 0.0);
    let old = data_at.is_some_and(|d| now - d > STALE_AFTER_S);
    entry.get("ok") == Some(&Value::Bool(false)) || old
}

fn worst_color(entry: &Value, now: f64) -> (PaceColor, Vec<costs_core::pace::WindowLine>) {
    let lines = window_lines(entry, now);
    let color = lines
        .iter()
        .map(|w| w.color)
        .max_by_key(|c| c.rank())
        .unwrap_or(PaceColor::Gray);
    (color, lines)
}

fn priorities(c: &MenuCtx) -> Vec<(usize, PriorityKey)> {
    let mut order: Vec<(usize, PriorityKey)> = c
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| (i, use_priority(e, c.now, c.horizon_s)))
        .collect();
    order.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    order
}

fn header(out: &mut dyn Write, c: &MenuCtx, spend: f64) -> Result<(), Box<dyn std::error::Error>> {
    let pairs: Vec<(String, String)> = c
        .cfg
        .hosts
        .iter()
        .filter(|h| h.enabled)
        .flat_map(|h| h.agents.iter().map(|a| (h.name.clone(), a.clone())))
        .collect();
    let (age, stale) = sync_age(c.store.oldest_ok_sync(&pairs)?.as_deref(), c.now);
    let mark = if stale { " ~" } else { "" };
    writeln!(out, "≈ US$ {} · {}", commas(spend, 0), c.month)?;
    writeln!(out, "---")?;
    writeln!(
        out,
        "{}",
        menu_line(&format!("API estimada · US$ {} · {}", commas(spend, 2), c.month), None,)
    )?;
    detail(out, "Estimativa a preço de API; não é pagamento.", None)?;
    detail(out, "Assinaturas e recargas não incluídas.", None)?;
    writeln!(
        out,
        "{}",
        menu_line(&format!("Coletas · mais antiga {age}{mark}"), None,)
    )?;
    detail(
        out,
        "Idade da coleta mais antiga; outros dados podem ser recentes.",
        None,
    )?;
    for (host, agent) in &pairs {
        let pair = [(host.clone(), agent.clone())];
        let (source_age, source_stale) = sync_age(c.store.oldest_ok_sync(&pair)?.as_deref(), c.now);
        let status = if source_stale { " · atrasada" } else { "" };
        detail(out, &format!("{host} / {agent} · {source_age}{status}"), None)?;
    }
    let subs = &c.cfg.subscriptions;
    if !subs.is_empty() {
        let fixed: f64 = subs.iter().map(|s| s.monthly_usd).sum();
        let total = format!("Assinaturas · US$ {}/mês", commas(fixed, 2));
        writeln!(out, "{}", menu_line(&total, None))?;
        for s in subs {
            let line = format!("--{} · US$ {}/mês", s.name, commas(s.monthly_usd, 2));
            writeln!(out, "{}", menu_line(&line, Some("font=Menlo size=11")))?;
        }
    }
    Ok(())
}

fn provider_block(
    out: &mut dyn Write,
    c: &MenuCtx,
    idx: usize,
    sub_by_provider: &HashMap<&str, f64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let provider = c.cfg.quota.providers[idx].as_str();
    let entry = &c.entries[idx];
    let (cost_total, by_agent) = match c.cfg.quota.cost_map.get(provider) {
        Some(keys) if !keys.is_empty() => {
            let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
            let (total, rows) = c.store.spend_by_provider(&keys, c.month)?;
            (Some(total), rows)
        }
        _ => (None, Vec::new()),
    };
    let balance = non_empty_balance(entry);
    let stale = if is_stale(entry, c.now) {
        " · desatualizado"
    } else {
        ""
    };
    let (_, windows) = worst_color(entry, c.now);
    let usage = if windows.is_empty() {
        balance.map_or_else(|| "cotas n/d".to_owned(), |b| format!("saldo {}", fmt_balance(b)))
    } else {
        windows
            .iter()
            .map(|w| format!("{} {:.0}%", w.label, w.used))
            .collect::<Vec<_>>()
            .join(" · ")
            + " usado"
    };
    let title = format!("{} · {usage}{stale}", provider_name(provider));
    writeln!(out, "{}", menu_line(&title, None))?;
    detail(
        out,
        &format!(
            "Estimativa API · {}",
            cost_total.map_or("n/d".to_owned(), |v| format!("US$ {}", commas(v, 2)))
        ),
        None,
    )?;
    if let Some(monthly) = sub_by_provider.get(provider) {
        detail(out, &format!("Assinatura · US$ {}/mês", commas(*monthly, 2)), None)?;
    }
    if let Some(b) = balance {
        detail(out, &format!("Saldo disponível · {}", fmt_balance(b)), None)?;
    }
    if let Some(line) = error_line(entry, provider) {
        detail(out, line.trim_start_matches('-'), None)?;
    }
    if !windows.is_empty() {
        writeln!(out, "-----")?;
        detail(out, "Cotas · uso / tempo / ritmo", None)?;
    }
    for w in &windows {
        let action = format!("color={}", w.color.menu_color());
        detail(
            out,
            &format!("{} · {:.0}% usado · renova {}", w.label, w.used, w.reset),
            Some(&action),
        )?;
        // Keep the shared projection and elapsed-time explanation, without the
        // terminal progress bar or a single over-wide quota row.
        let pace = w
            .text
            .split(" · ")
            .skip(1)
            .filter(|part| !part.starts_with('⏱'))
            .collect::<Vec<_>>()
            .join(" · ");
        detail(out, &pace, None)?;
    }
    if let Some(line) = resets_line(entry, c.now) {
        detail(out, line.trim_start_matches('-'), None)?;
    }
    if let Some(hint) = priority_hint(entry, c.now) {
        writeln!(out, "-----")?;
        detail(out, &hint, None)?;
    }
    if !by_agent.is_empty() {
        writeln!(out, "-----")?;
        detail(out, "Por máquina / harness · API estimada", None)?;
        for (agent, v, tok) in &by_agent {
            let tok_txt = if *tok != 0 {
                format!(" · {} M tokens", commas(*tok as f64 / TOKENS_PER_MILLION, 1))
            } else {
                String::new()
            };
            writeln!(
                out,
                "{}",
                menu_line(&format!("----{agent} · US$ {}{tok_txt}", commas(*v, 2)), None)
            )?;
        }
    }
    detail(
        out,
        &format!("Cotas / saldo · atualizado {}", age_text(entry, c.now)),
        None,
    )?;
    Ok(())
}

/// `--🔑 login expirado · faça login: grok login` quando a última consulta falhou.
fn error_line(entry: &Value, provider: &str) -> Option<String> {
    if entry.get("ok").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let code = entry.get("error").and_then(Value::as_str)?;
    let hint = quota_error_hint(code, provider).map_or_else(String::new, |h| format!(" · {h}"));
    Some(format!("--{} {}{hint}", quota_error_icon(code), quota_error_text(code)))
}

/// `--♻️ N resets disponíveis · vence X`, quando o provider tem créditos de reset.
fn resets_line(entry: &Value, now: f64) -> Option<String> {
    let rc = crate::status::reset_credits(entry, now);
    let n = rc.get("available").and_then(Value::as_i64)?;
    let noun = if n == 1 {
        "reset disponível"
    } else {
        "resets disponíveis"
    };
    let expiry = rc
        .get("next_expiry")
        .and_then(Value::as_str)
        .map_or_else(String::new, |e| format!(" · vence {e}"));
    Some(format!("--♻️ {n} {noun}{expiry}"))
}

fn footer(out: &mut dyn Write, exe: &str, month: &str) -> io::Result<()> {
    let bash = format!("terminal=false bash='{exe}'");
    let actions = [
        (
            "Atualizar saldos",
            format!("refresh=true {bash} param1=quota param2=--refresh"),
        ),
        (
            "Abrir painel HTML do mês",
            format!("{bash} param1=report param2=--format param3=html param4=--open param5=--month param6={month}"),
        ),
        ("Sincronizar custos agora", format!("refresh=true {bash} param1=sync")),
        (
            "Relatório do mês no terminal",
            format!(
                "terminal=true bash='{exe}' param1=report param2=--by param3=provider+model param4=--month param5={month}"
            ),
        ),
    ];
    for (label, action) in actions {
        writeln!(out, "{}", menu_line(label, Some(&action)))?;
    }
    Ok(())
}

/// Escreve o menu completo.
pub fn render(out: &mut dyn Write, c: &MenuCtx) -> Result<(), Box<dyn std::error::Error>> {
    let spend = c.store.month_spend(c.month)?;
    header(out, c, spend)?;
    writeln!(out, "---")?;
    let sub_by_provider: HashMap<&str, f64> = c
        .cfg
        .subscriptions
        .iter()
        .filter_map(|s| Some((s.provider.as_deref()?, s.monthly_usd)))
        .collect();
    let order = priorities(c);
    let usable: Vec<&str> = order
        .iter()
        .filter(|(_, k)| k.0 <= MAX_USABLE_TIER)
        .map(|(i, _)| c.cfg.quota.providers[*i].as_str())
        .collect();
    if !usable.is_empty() {
        let line = format!("Usar agora · {}", provider_name(usable[0]));
        writeln!(out, "{}", menu_line(&line, None))?;
        detail(
            out,
            &format!("Prioridade · próximas {}", fmt_horizon(c.horizon_s)),
            None,
        )?;
        for (rank, provider) in usable.iter().enumerate() {
            detail(out, &format!("{}. {provider}", rank + 1), None)?;
        }
    }
    writeln!(out, "---")?;
    for (idx, _) in &order {
        provider_block(out, c, *idx, &sub_by_provider)?;
    }
    writeln!(out, "---")?;
    footer(out, c.exe, c.month)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passive_rows_use_documented_readable_color_without_fake_actions() {
        let row = menu_line("Claude · 5h 30% usado", None);
        assert!(row.contains("color=#202124,#F2F2F7"), "{row}");
        for unsupported in ["disabled=", "refresh=", "bash=", "href="] {
            assert!(!row.contains(unsupported), "{row}");
        }
    }

    #[test]
    fn provider_labels_are_human_readable() {
        for (id, name) in [
            ("claude", "Claude"),
            ("codex", "Codex"),
            ("zai", "z.ai"),
            ("xai", "xAI"),
            ("opencode-go", "OpenCode Go"),
            ("deepseek", "deepseek"),
        ] {
            assert_eq!(provider_name(id), name);
        }
    }

    #[test]
    fn provider_root_keeps_quota_usage_and_secondary_context() {
        let text = render_with(
            vec![
                json!({}),
                json!({"ok": true, "fetched_at": 990,
            "windows": [{"label": "5h", "remaining_pct": 70, "resets_at": 4600},
                        {"label": "sem", "remaining_pct": 40, "resets_at": 87400}],
            "reset_credits": {"available": 2}}),
            ],
            SECS_PER_DAY,
        );
        let root = text.lines().find(|line| line.starts_with("z.ai ·")).unwrap();
        assert!(root.contains("5h 30% · sem 60% usado"), "{root}");
        assert!(root.contains(NEUTRAL_COLOR), "{root}");
        assert!(!root.contains("assinatura"));
        assert!(!text.contains("disabled="));
        assert!(text.contains("--Assinatura · US$ 10.00/mês"));
        assert!(text.contains("--Estimativa API · n/d"));
        assert!(text.contains("2 resets disponíveis"));
        assert!(text.contains("--Cotas / saldo · atualizado"));
        assert!(!text.lines().next().unwrap().contains('~'));
        assert!(text.contains("Coletas · mais antiga"));
    }

    #[test]
    fn error_line_explains_failed_quota() {
        let e = json!({"ok": false, "error": "expired"});
        assert_eq!(
            error_line(&e, "codex").unwrap(),
            "--🔑 login expirado · faça login: codex login"
        );
        assert!(error_line(&json!({"ok": true, "error": null}), "codex").is_none());
    }

    #[test]
    fn resets_line_only_with_available_credits() {
        let e = json!({"reset_credits": {"available": 2, "expires": [2000.0]}});
        let line = resets_line(&e, 1000.0).unwrap();
        assert!(line.starts_with("--♻️ 2 resets disponíveis · vence "), "{line}");
        assert_eq!(
            resets_line(&json!({"reset_credits": {"available": 1}}), 0.0).unwrap(),
            "--♻️ 1 reset disponível"
        );
        assert!(resets_line(&json!({}), 0.0).is_none());
    }
    use costs_core::config::parse_config;
    use costs_core::store::utc_now;
    use serde_json::json;

    const CFG: &str = r#"
[quota]
providers = ["deepseek", "zai"]
[quota.cost_map]
deepseek = "deepseek"
[[hosts]]
name = "mac"
[[subscriptions]]
name = "Plano | X"
provider = "zai"
monthly_usd = 10.0
"#;

    fn render_with(entries: Vec<Value>, horizon_s: f64) -> String {
        render_with_usage(entries, horizon_s, &[])
    }

    fn render_with_usage(entries: Vec<Value>, horizon_s: f64, rows: &[costs_core::model::Row]) -> String {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.sqlite"), "", utc_now()).unwrap();
        store.upsert_rows(rows, "2026-10-23T00:00:00+00:00").unwrap();
        let cfg = parse_config(CFG).unwrap();
        let ctx = MenuCtx {
            store: &store,
            cfg: &cfg,
            entries: &entries,
            now: 1000.0,
            month: "2026-10",
            horizon_s,
            exe: "/bin/costs",
        };
        let mut out = Vec::new();
        render(&mut out, &ctx).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn renders_title_subscriptions_and_actions() {
        let text = render_with(
            vec![
                json!({"ok": true, "fetched_at": 990, "balance": {"amount": 5.0, "currency": "USD"}}),
                json!({"ok": false, "error": "x"}),
            ],
            SECS_PER_DAY,
        );
        assert!(text.starts_with("≈ US$ 0 · 2026-10"), "{text}");
        assert!(text.contains("Assinaturas · US$ 10.00/mês"));
        assert!(text.contains("--Plano / X · US$ 10.00/mês | font=Menlo size=11"));
        assert!(text.contains("Usar agora · deepseek"));
        assert!(text.contains("--Saldo disponível ·"), "{text}");
        assert!(text.contains("z.ai · cotas n/d · desatualizado"));
        assert!(text.contains("--Assinatura · US$ 10.00/mês"));
        assert!(text.contains("bash='/bin/costs' param1=sync"));
    }

    #[test]
    fn menu_order_explains_monthly_allowance_and_expiring_extra_reset() {
        let text = render_with(
            vec![
                json!({"windows": [
                {"label": "sem", "remaining_pct": 90, "resets_at": 2800},
                {"label": "mês", "remaining_pct": 10, "resets_at": 201000}]}),
                json!({"windows": [{"label": "sem", "remaining_pct": 60, "resets_at": 361000}],
                "reset_credits": {"available": 2, "expires": [4600]}}),
            ],
            SECS_PER_DAY,
        );
        assert!(text.contains("Usar agora · z.ai"), "{text}");
        assert!(text.contains("Ordem pela cota mensal"));
        assert!(text.contains("reset extra que vence"));
    }

    #[test]
    fn horizon_label_uses_days_only_for_whole_days() {
        assert_eq!(fmt_horizon(172_800.0), "2 dias");
        assert_eq!(fmt_horizon(86_400.0), "24h");
        assert_eq!(fmt_horizon(5400.0), "1.5h");
    }

    #[test]
    fn monthly_report_actions_pass_the_selected_month_and_use_private_html_output() {
        let text = render_with(vec![json!({}), json!({})], SECS_PER_DAY);
        let terminal = text
            .lines()
            .find(|line| line.contains("Relatório do mês no terminal"))
            .unwrap();
        assert!(terminal.contains("param4=--month param5=2026-10"), "{terminal}");
        let html = text
            .lines()
            .find(|line| line.contains("Abrir painel HTML do mês"))
            .unwrap();
        assert!(html.contains("param4=--open param5=--month param6=2026-10"), "{html}");
        assert!(!html.contains("/tmp/"));
        assert!(!html.contains("--output"));
    }

    #[test]
    fn compact_menu_keeps_real_actions_and_passive_roots_readable() {
        let text = render_with(
            vec![
                json!({"ok": true, "fetched_at": 990, "balance": {"amount": 5.0, "currency": "USD"}}),
                json!({}),
            ],
            SECS_PER_DAY,
        );
        for action in ["param1=quota param2=--refresh", "param1=sync", "param1=report"] {
            assert!(text.contains(action), "{text}");
        }
        for line in text.lines().skip(1) {
            let label = line.split(" | ").next().unwrap();
            assert!(label.chars().count() <= 64, "over-wide label: {label}");
            let attributes = line.split_once(" | ").map(|(_, attrs)| attrs).unwrap_or("");
            let has_action = attributes.split_whitespace().any(|attribute| {
                ["bash=", "href=", "stdin=", "refresh="]
                    .iter()
                    .any(|key| attribute.starts_with(key))
            });
            if has_action {
                assert!(
                    !attributes.contains(NEUTRAL_COLOR),
                    "real action attributes must stay intact: {line}"
                );
                assert!(attributes.contains("bash='/bin/costs'"), "{line}");
            }
            if !label.starts_with('-') && !has_action {
                assert!(
                    line.contains(NEUTRAL_COLOR),
                    "root identity must stay neutral and readable: {line}"
                );
            }
        }
        assert!(text.contains("--1. deepseek"));
        assert!(!text.contains(" › "));
    }

    #[test]
    fn machine_harness_subgroup_preserves_all_costs_and_token_categories() {
        let usage = |host: &str, agent: &str, date: &str, cost_usd: Option<f64>| costs_core::model::Row {
            host: host.to_owned(),
            agent: agent.to_owned(),
            date: date.to_owned(),
            model: "deepseek-chat".to_owned(),
            model_raw: "deepseek-chat".to_owned(),
            provider: "deepseek".to_owned(),
            input: 1_000_000,
            output: 2_000_000,
            cache_read: 3_000_000,
            cache_write: 4_000_000,
            cost_usd,
            cost_basis: "reported".to_owned(),
        };
        let text = render_with_usage(
            vec![json!({}), json!({})],
            SECS_PER_DAY,
            &[
                usage("mac", "pi", "2026-10-01", Some(1.25)),
                usage("mac", "pi", "2026-10-02", Some(2.50)),
                usage("srv", "opencode", "2026-10-03", Some(4.50)),
                usage("mac", "cursor", "2026-10-04", None),
                usage("old", "pi", "2026-09-01", Some(99.00)),
            ],
        );
        assert!(text.contains("--Estimativa API · US$ 8.25"), "{text}");
        let subgroup = text
            .lines()
            .find(|line| line.starts_with("--Por máquina / harness"))
            .unwrap();
        assert!(subgroup.contains("API estimada"));
        let machine_rows: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("----") && !line.starts_with("-----"))
            .collect();
        assert_eq!(machine_rows.len(), 3, "{text}");
        for (row, expected) in machine_rows.iter().zip([
            "----srv / opencode / deepseek · US$ 4.50 · 10.0 M tokens",
            "----mac / pi / deepseek · US$ 3.75 · 20.0 M tokens",
            "----mac / cursor / deepseek · US$ 0.00 · 10.0 M tokens",
        ]) {
            assert_eq!(row.split_once(" | ").unwrap().0, expected);
            assert!(row.contains(NEUTRAL_COLOR));
        }
        assert!(!text.contains("old / pi"));
        assert!(!text.lines().any(|line| line.starts_with("------")));
    }

    #[test]
    fn detail_wraps_without_losing_words_or_adding_depth() {
        let prose = "a meaningful explanation ".repeat(8);
        let mut out = Vec::new();
        detail(&mut out, &prose, None).unwrap();
        let text = String::from_utf8(out).unwrap();
        let restored = text
            .lines()
            .map(|l| l.split(" | ").next().unwrap().trim_start_matches('-'))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(restored, prose.trim());
        assert!(
            text.lines()
                .all(|l| l.split(" | ").next().unwrap().chars().count() <= DETAIL_WIDTH + 2)
        );
    }

    #[test]
    fn pipes_in_text_are_escaped() {
        assert_eq!(menu_line("a|b", Some("x=1")), "a/b | x=1");
    }

    #[test]
    fn window_entry_gets_pace_line_and_worst_color() {
        let entry = json!({"ok": true, "fetched_at": 990, "windows": [
            {"label": "5h", "remaining_pct": 10, "resets_at": 1000 + 3600, "window_s": 18000}]});
        let text = render_with(vec![entry, json!({})], SECS_PER_DAY);
        let quota_row = text
            .lines()
            .find(|line| line.starts_with("--5h · 90% usado"))
            .expect("used-quota row must be present");
        let (_, attributes) = quota_row.split_once(" | ").expect("quota row must have attributes");
        assert!(
            attributes.split_whitespace().any(|attribute| attribute == "color=red"),
            "{quota_row}"
        );
        let cost_row = text
            .lines()
            .find(|line| line.starts_with("deepseek · 5h 90% usado"))
            .expect("provider cost row must be present");
        let (_, attributes) = cost_row.split_once(" | ").expect("cost row must have attributes");
        assert!(
            attributes
                .split_whitespace()
                .any(|attribute| attribute == NEUTRAL_COLOR),
            "{cost_row}"
        );
    }
}
