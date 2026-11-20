//! Normalização compartilhada (port de `lib/norm.jq`).

use chrono::{DateTime, NaiveDateTime};
use serde_json::Value;

use super::entry::Window;

const MS_THRESHOLD: f64 = 1e12;
const LABEL_MAX_CHARS: usize = 8;

/// jq `tonumber?`: número ou string numérica.
pub fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|x| x.is_finite()),
        _ => None,
    }
}

fn epoch_from_f64(x: f64) -> i64 {
    if x > MS_THRESHOLD {
        (x / 1000.0).floor() as i64
    } else {
        x.floor() as i64
    }
}

fn iso_epoch(s: &str) -> Option<i64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S%.f%z"] {
        if let Ok(dt) = DateTime::parse_from_str(s, fmt) {
            return Some(dt.timestamp());
        }
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt.and_utc().timestamp());
        }
    }
    None
}

/// Epoch (s ou ms), string numérica ou ISO 8601 → epoch s. Inválido → `None`.
pub fn to_epoch(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_f64().map(epoch_from_f64),
        Value::String(s) => {
            let t = s.trim();
            if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit() || c == '.') {
                t.parse::<f64>().ok().map(epoch_from_f64)
            } else {
                iso_epoch(t)
            }
        }
        _ => None,
    }
}

/// Minutos → rótulo curto (5h, dia, sem, mês, `<n>m`, `<n>h`, `<n>d`).
pub fn label_from_minutes(m: f64) -> String {
    if (40320.0..=44640.0).contains(&m) {
        "mês".into()
    } else if m == 10080.0 {
        "sem".into()
    } else if m == 1440.0 {
        "dia".into()
    } else if m < 60.0 {
        format!("{}m", m.floor())
    } else if m < 1440.0 {
        format!("{}h", (m / 60.0).floor())
    } else {
        format!("{}d", (m / 1440.0).floor())
    }
}

/// Como [`label_from_minutes`], mas aceita texto (`monthly`, `WEEKLY`...) ou número.
pub fn label_from_value(v: &Value) -> Option<String> {
    if let Value::String(s) = v {
        let l = s.to_ascii_lowercase();
        if l.contains("month") {
            return Some("mês".into());
        } else if l.contains("week") {
            return Some("sem".into());
        } else if l.contains("day") {
            return Some("dia".into());
        }
    }
    num(v).map(label_from_minutes)
}

/// Duração (s) a partir do rótulo; desconhecido → `None`.
pub fn label_window_s(label: &str) -> Option<f64> {
    match label {
        "5h" => Some(18000.0),
        "dia" => Some(86400.0),
        "sem" => Some(604800.0),
        "mês" => Some(2592000.0),
        _ => {
            let (n, mult) = if let Some(n) = label.strip_suffix('h') {
                (n, 3600.0)
            } else {
                (label.strip_suffix('d')?, 86400.0)
            };
            if n.is_empty() || !n.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            n.parse::<f64>().ok().map(|x| x * mult)
        }
    }
}

fn safe_text(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            let u = c as u32;
            u >= 32 && u != 127 && !(128..=159).contains(&u)
        })
        .take(LABEL_MAX_CHARS)
        .collect()
}

fn clamp_pct(x: f64) -> i64 {
    x.clamp(0.0, 100.0).round() as i64
}

/// Janela canônica com duração explícita (inválida/≤ 0 some).
pub fn window4(label: &str, remaining: Option<f64>, resets_at: Option<i64>, window_s: Option<f64>) -> Window {
    Window {
        label: safe_text(label),
        remaining_pct: remaining.map(clamp_pct),
        resets_at,
        window_s: window_s.filter(|w| *w > 0.0).map(|w| w.floor() as i64),
        window_kind: None,
    }
}

/// Janela cuja duração deriva do rótulo.
pub fn window(label: &str, remaining: Option<f64>, resets_at: Option<i64>) -> Window {
    window4(label, remaining, resets_at, label_window_s(label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn epoch_variants() {
        assert_eq!(to_epoch(&json!(1_700_000_000_000_i64)), Some(1_700_000_000));
        assert_eq!(to_epoch(&json!("1700000000")), Some(1_700_000_000));
        assert_eq!(to_epoch(&json!("2026-10-10T04:23:21.000Z")), Some(1_791_606_201));
        assert_eq!(to_epoch(&json!("2026-10-10T05:23:21+01:00")), Some(1_791_606_201));
        assert_eq!(to_epoch(&json!("2026-10-10 04:23:21")), Some(1_791_606_201));
        assert_eq!(to_epoch(&json!("lixo")), None);
        assert_eq!(to_epoch(&Value::Null), None);
    }

    #[test]
    fn labels() {
        assert_eq!(label_from_minutes(300.0), "5h");
        assert_eq!(label_from_minutes(10080.0), "sem");
        assert_eq!(label_from_minutes(43200.0), "mês");
        assert_eq!(label_from_minutes(1440.0), "dia");
        assert_eq!(label_from_minutes(45.0), "45m");
        assert_eq!(label_from_minutes(4320.0), "3d");
        assert_eq!(
            label_from_value(&json!("USAGE_PERIOD_TYPE_WEEKLY")).as_deref(),
            Some("sem")
        );
        assert_eq!(label_from_value(&json!("x")), None);
        assert_eq!(label_window_s("12h"), Some(43200.0));
        assert_eq!(label_window_s("zz"), None);
    }

    #[test]
    fn window_clamps_and_sanitizes() {
        let w = window("5h\u{1b}abcdefghij", Some(130.0), None);
        assert_eq!(w.remaining_pct, Some(100));
        assert_eq!(w.label.chars().count(), 8);
        assert!(!w.label.contains('\u{1b}'));
        assert_eq!(window4("x", Some(-3.0), None, Some(0.0)).window_s, None);
    }
}
