//! Antigravity (CLI do Google): lê a quota que o statusline do ai-standards grava
//! no cache compartilhado (`antigravity-payload.json`, só os campos de quota —
//! sem rede e sem email). Os buckets vêm por família de modelo: `gemini-*` para
//! Gemini e `3p-*` para Claude/GPT de terceiros; com as duas presentes, os
//! rótulos ganham o prefixo da família para exibir as duas quotas.

use serde_json::Value;

use super::creds::{Ctx, Failure};
use super::entry::Partial;
use super::norm::{num, to_epoch, window};

/// Gravado (atômico) pelo `ai-usage` quando o Antigravity renderiza o statusline.
const PAYLOAD_FILE: &str = "antigravity-payload.json";

pub fn fetch(ctx: &Ctx) -> Result<Partial, Failure> {
    let path = ctx.cache_dir.join(PAYLOAD_FILE);
    let raw = std::fs::read_to_string(&path).map_err(|_| Failure::NoCred)?;
    let v: Value = serde_json::from_str(&raw).map_err(|_| Failure::Parse)?;
    let mut partial = parse(&v, ctx.now).ok_or(Failure::Parse)?;
    // o payload é do último render do statusline, não de agora
    partial.data_at = observed_at(&path).filter(|t| *t < ctx.now);
    Ok(partial)
}

fn observed_at(path: &std::path::Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    i64::try_from(secs).ok()
}

/// Prefixo curto da família (o rótulo de janela é truncado em 8 caracteres).
fn family_of(key: &str) -> &'static str {
    match key.split(['-', '_']).next().unwrap_or(key) {
        "gemini" => "gem",
        other if other.eq_ignore_ascii_case("3p") => "3p",
        _ => "ag",
    }
}

fn label_of(key: &str) -> String {
    let k = key.to_ascii_lowercase();
    if k.contains("week") {
        "sem".into()
    } else if k.contains("day") || k.contains("daily") {
        "dia".into()
    } else if k.contains("5h") || k.contains("five") {
        "5h".into()
    } else if k.contains("month") {
        "mês".into()
    } else {
        key.chars().take(8).collect()
    }
}

fn bucket_reset(x: &Value, now: i64) -> Option<i64> {
    x.get("reset_time")
        .and_then(to_epoch)
        .or_else(|| x.get("reset_in_seconds").and_then(Value::as_i64).map(|s| now + s))
}

fn plan_tier(v: &Value) -> Option<String> {
    v.get("plan_tier")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| p.chars().take(32).collect())
}

pub fn parse(v: &Value, now: i64) -> Option<Partial> {
    let quota = v.get("quota")?.as_object()?;
    let buckets: Vec<(&str, &Value)> = quota
        .iter()
        .filter(|(_, x)| x.get("remaining_fraction").is_some_and(|f| num(f).is_some()))
        .map(|(k, x)| (k.as_str(), x))
        .collect();
    if buckets.is_empty() {
        return None;
    }
    let mixed = buckets
        .iter()
        .map(|(k, _)| family_of(k))
        .collect::<std::collections::HashSet<_>>()
        .len()
        > 1;
    let windows: Vec<_> = buckets
        .iter()
        .filter_map(|(k, x)| {
            let remaining = num(x.get("remaining_fraction")?)? * 100.0;
            let label = if mixed {
                format!("{} {}", family_of(k), label_of(k))
            } else {
                label_of(k)
            };
            Some(window(&label, Some(remaining), bucket_reset(x, now)))
        })
        .collect();
    (!windows.is_empty()).then(|| Partial {
        plan: Some(plan_tier(v)),
        windows: Some(windows),
        balance: None,
        ..Partial::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::testutil::*;
    use serde_json::json;

    const PAYLOAD: &str = r#"{"quota":{
        "gemini-weekly-limit":{"remaining_fraction":0.4,"reset_time":"2026-10-30T00:00:00Z"},
        "3p-weekly-limit":{"remaining_fraction":0.9,"reset_in_seconds":3600}},
        "plan_tier":"pro","model":{"id":"gemini-3-pro-preview"},"email":"x@y.z"}"#;

    #[test]
    fn both_families_show_prefixed_labels() {
        let v: Value = serde_json::from_str(PAYLOAD).unwrap();
        let p = parse(&v, 1_000).unwrap();
        let w = p.windows.unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].label, "gem sem");
        assert_eq!(w[0].remaining_pct, Some(40));
        assert_eq!(w[1].label, "3p sem");
        assert_eq!(w[1].remaining_pct, Some(90));
        assert_eq!(w[1].resets_at, Some(1_000 + 3_600));
        assert_eq!(p.plan, Some(Some("pro".into())));
    }

    #[test]
    fn single_family_has_no_prefix() {
        let v = json!({"quota": {"gemini-weekly-limit": {"remaining_fraction": 0.5}}});
        let p = parse(&v, 0).unwrap();
        assert_eq!(p.windows.unwrap()[0].label, "sem");
    }

    #[test]
    fn empty_or_missing_quota_is_parse_error() {
        assert!(parse(&json!({}), 0).is_none());
        assert!(parse(&json!({"quota": {"x": {}}}), 0).is_none());
    }

    #[test]
    fn fetch_reads_payload_from_cache_dir() {
        let t = Fixture::new();
        let dir = t.cache_dir();
        std::fs::write(dir.join(PAYLOAD_FILE), PAYLOAD).unwrap();
        let e = t.run("antigravity", &FakeHttp::new());
        assert!(e.ok, "{e:?}");
        assert_eq!(e.plan.as_deref(), Some("pro"));
        assert_eq!(e.windows.len(), 2);
    }

    #[test]
    fn missing_payload_is_nocred() {
        let t = Fixture::new();
        let e = t.run("antigravity", &FakeHttp::new());
        assert_eq!(e.error.as_deref(), Some("nocred"));
    }
}
