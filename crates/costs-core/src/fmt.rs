//! Formatação de duração, reset, saldo e idade de dados.
//!
//! Port of the formatting helpers. O tempo é sempre um argumento explícito.

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use serde_json::Value;

/// Passa de 1h e o menu marca o número como velho.
pub const STALE_AFTER_S: f64 = 3600.0;

const MINUTE: i64 = 60;
const HOUR: i64 = 3600;
const DAY: i64 = 86400;

/// Texto do código de erro da cota (campo `error` do cache); desconhecido passa como está.
pub fn quota_error_text(code: &str) -> String {
    match code {
        "nocred" => "sem login (token não encontrado)",
        "expired" => "login expirado",
        "nokey" => "API key ausente",
        "auth" => "credencial recusada (401/403)",
        "http" => "falha de rede/API",
        "parse" => "resposta inesperada da API",
        other => other,
    }
    .to_owned()
}

/// Ícone do erro da cota: credencial (🔑), chave (🗝️), rede (🌐) ou outro (⚠️).
pub fn quota_error_icon(code: &str) -> &'static str {
    match code {
        "nocred" | "expired" | "auth" => "🔑",
        "nokey" => "🗝️",
        "http" => "🌐",
        _ => "⚠️",
    }
}

/// Como resolver o erro, por provider; `None` quando não depende do usuário.
pub fn quota_error_hint(code: &str, provider: &str) -> Option<String> {
    let login = match provider {
        "claude" => Some("abra o Claude Code (/login)"),
        "codex" => Some("codex login"),
        "xai" => Some("grok login (ou use o grok/pi para renovar)"),
        _ => None,
    };
    if code == "nocred" && provider == "antigravity" {
        return Some("abra o Antigravity CLI: o statusline grava a cota local".to_owned());
    }
    match (code, login) {
        ("nocred" | "expired" | "auth", Some(cmd)) => Some(format!("faça login: {cmd}")),
        ("nokey" | "auth", None) => Some("configure a API key (env ou auth.json do pi/opencode)".to_owned()),
        ("nocred" | "expired", None) => Some("faça login no agente do provedor".to_owned()),
        _ => None,
    }
}

/// Símbolo de moeda conhecido (`USD` -> `US$`, `CNY` -> `¥`).
pub fn currency_symbol(code: &str) -> Option<&'static str> {
    match code {
        "USD" => Some("US$"),
        "CNY" => Some("¥"),
        _ => None,
    }
}

/// Duração curta: `30m`, `2h05`, `3d4h`. Negativos viram `0m`.
pub fn fmt_duration(seconds: f64) -> String {
    // `as` trunca em direção a zero e satura, como `int()` do Python.
    let s = seconds as i64;
    if s < HOUR {
        format!("{}m", s.max(0) / MINUTE)
    } else if s < DAY {
        format!("{}h{:02}", s / HOUR, (s % HOUR) / MINUTE)
    } else {
        format!("{}d{}h", s / DAY, (s % DAY) / HOUR)
    }
}

/// Tempo até o reset: `-` sem reset, `agora` se já passou.
pub fn fmt_reset(resets_at: Option<f64>, now: f64) -> String {
    match resets_at {
        Some(ra) if ra != 0.0 => {
            let left = ra - now;
            if left <= 0.0 {
                "agora".to_owned()
            } else {
                fmt_duration(left)
            }
        }
        _ => "-".to_owned(),
    }
}

/// Número com 2 casas e separador de milhar (`{:,.2f}` do Python).
fn group_2f(amount: f64) -> String {
    let raw = format!("{:.2}", amount.abs());
    let (int, frac) = raw.split_once('.').unwrap_or((raw.as_str(), "00"));
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let sign = if amount < 0.0 && raw != "0.00" { "-" } else { "" };
    format!("{sign}{grouped}.{frac}")
}

/// Saldo em dinheiro de uma entrada de quota (`{currency, amount}`).
pub fn fmt_balance(balance: &Value) -> String {
    let cur = balance
        .get("currency")
        .and_then(Value::as_str)
        .filter(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase()))
        .unwrap_or("");
    let amount = balance.get("amount").and_then(Value::as_f64).unwrap_or(0.0);
    let text = group_2f(amount);
    match currency_symbol(cur) {
        Some(sym) => format!("{sym} {text}"),
        None => format!("{text} {cur}").trim().to_owned(),
    }
}

fn parse_epoch(iso: &str) -> Option<f64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        return Some(dt.timestamp_millis() as f64 / 1000.0);
    }
    // Como `datetime.fromisoformat`: sem fuso, vale o horário local.
    let naive = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(iso, f).ok())?;
    let dt = Local.from_local_datetime(&naive).earliest()?;
    Some(dt.timestamp_millis() as f64 / 1000.0)
}

/// Idade da coleta de custo: `(texto, velho)`. Sem coleta conta como velho.
///
/// Diferença do Python: data ilegível vira `("nunca", true)` em vez de exceção.
pub fn sync_age(collected_at: Option<&str>, now: f64) -> (String, bool) {
    let Some(at) = collected_at.filter(|s| !s.is_empty()).and_then(parse_epoch) else {
        return ("nunca".to_owned(), true);
    };
    let age = (now - at).max(0.0);
    let text = if age < 60.0 {
        "agora".to_owned()
    } else {
        fmt_duration(age)
    };
    (text, age > STALE_AFTER_S)
}

/// Idade dos dados de uma entrada de quota (`data_at`, senão `fetched_at`).
pub fn age_text(entry: &Value, now: f64) -> String {
    let at = ["data_at", "fetched_at"]
        .iter()
        .filter_map(|k| entry.get(k).and_then(Value::as_f64))
        .find(|v| *v != 0.0);
    match at {
        Some(at) => format!("{} atrás", fmt_duration(now - at)),
        None => "-".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_errors_have_text_icon_and_hint() {
        assert_eq!(quota_error_text("expired"), "login expirado");
        assert_eq!(quota_error_text("nokey"), "API key ausente");
        assert_eq!(quota_error_text("sem cache"), "sem cache");
        assert_eq!(quota_error_icon("auth"), "🔑");
        assert_eq!(quota_error_icon("http"), "🌐");
        assert_eq!(
            quota_error_hint("expired", "xai").unwrap(),
            "faça login: grok login (ou use o grok/pi para renovar)"
        );
        assert!(quota_error_hint("auth", "deepseek").unwrap().contains("API key"));
        assert!(quota_error_hint("http", "claude").is_none());
    }
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn duration_branches() {
        assert_eq!(fmt_duration(-30.0), "0m");
        assert_eq!(fmt_duration(1799.0), "29m");
        assert_eq!(fmt_duration(3600.0), "1h00");
        assert_eq!(fmt_duration(7500.0), "2h05");
        assert_eq!(fmt_duration(86400.0 * 3.0 + 4.0 * 3600.0), "3d4h");
    }

    #[test]
    fn reset_formats_remaining_time() {
        let now = 1_000_000.0;
        assert_eq!(fmt_reset(None, now), "-");
        assert_eq!(fmt_reset(Some(0.0), now), "-");
        assert_eq!(fmt_reset(Some(now - 5.0), now), "agora");
        assert!(fmt_reset(Some(now + 4.0 * 3600.0), now).contains('h'));
    }

    #[test]
    fn balance_formats() {
        assert!(fmt_balance(&json!({"amount": null, "currency": "USD"})).contains("0.00"));
        assert!(!fmt_balance(&json!({"amount": 1, "currency": "US\nD"})).contains('\n'));
        assert_eq!(
            fmt_balance(&json!({"amount": 1234.5, "currency": "USD"})),
            "US$ 1,234.50"
        );
        assert_eq!(fmt_balance(&json!({"amount": 3, "currency": "CNY"})), "¥ 3.00");
        assert_eq!(
            fmt_balance(&json!({"amount": 1234567.891, "currency": "EUR"})),
            "1,234,567.89 EUR"
        );
        assert_eq!(fmt_balance(&json!({"amount": -1234.5})), "-1,234.50");
        assert_eq!(fmt_balance(&json!({"amount": 5, "currency": "usd"})), "5.00");
    }

    #[test]
    fn sync_age_marks_stale_after_one_hour() {
        let now = DateTime::parse_from_rfc3339("2026-10-19T12:00:00+00:00")
            .unwrap()
            .timestamp() as f64;
        let age = |s| sync_age(Some(s), now);
        assert_eq!(age("2026-10-19T11:30:00+00:00"), ("30m".into(), false));
        assert_eq!(age("2026-10-19T11:00:00+00:00"), ("1h00".into(), false));
        assert_eq!(age("2026-10-19T10:00:00+00:00"), ("2h00".into(), true));
        assert_eq!(age("2026-10-19T11:59:30+00:00"), ("agora".into(), false));
        assert_eq!(sync_age(None, now), ("nunca".into(), true));
        assert_eq!(sync_age(Some("lixo"), now), ("nunca".into(), true));
        assert!(sync_age(Some("2020-01-01T00:00:00"), now).1);
    }

    #[test]
    fn age_text_uses_data_at_then_fetched_at() {
        assert_eq!(age_text(&json!({}), 1_000_000.0), "-");
        let now = 1_000_000.0;
        assert_eq!(age_text(&json!({"fetched_at": now - 600.0}), now), "10m atrás");
        let both = json!({"data_at": now - 7200.0, "fetched_at": now - 60.0});
        assert_eq!(age_text(&both, now), "2h00 atrás");
    }

    proptest! {
        #[test]
        fn duration_never_panics_and_has_unit(s in -1e9f64..1e9) {
            let t = fmt_duration(s);
            prop_assert!(t.ends_with('m') || t.ends_with('h') || t.contains('h') || t.contains('d'));
        }
    }
}
