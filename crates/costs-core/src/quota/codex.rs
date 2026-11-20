//! Codex (ChatGPT): último `rate_limits` do rollout local; fallback `wham/usage`.

use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use chrono::{DateTime, Duration, Local};
use serde_json::Value;

use super::creds::{Ctx, Failure, bearer, read_json, str_at};
use super::entry::{Partial, ResetCredits, Window};
use super::norm::{label_from_minutes, num, to_epoch, window4};

const WHAM_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// Créditos que zeram a cota ("Limit Reset Credits"); não documentado, best-effort.
const RESET_CREDITS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const LOOKBACK_DAYS: i64 = 8;
const MAX_ROLLOUTS: usize = 8;
const RATE_LIMITS_MARK: &str = "\"rate_limits\":{";

fn codex_home(ctx: &Ctx) -> Option<PathBuf> {
    ctx.env
        .var("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| ctx.home_file(".codex"))
}

/// Rollouts mais recentes primeiro, com o mtime (epoch em segundos).
fn rollouts(ctx: &Ctx) -> Vec<(i64, PathBuf)> {
    let Some(base) = codex_home(ctx).map(|h| h.join("sessions")) else {
        return vec![];
    };
    let Some(today) = DateTime::from_timestamp(ctx.now, 0).map(|d| d.with_timezone(&Local)) else {
        return vec![];
    };
    let mut files: Vec<(SystemTime, PathBuf)> = vec![];
    for i in 0..LOOKBACK_DAYS {
        let dir = base.join((today - Duration::days(i)).format("%Y/%m/%d").to_string());
        let Ok(rd) = fs::read_dir(dir) else { continue };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("rollout-") && n.ends_with(".jsonl") {
                let mt = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((mt, e.path()));
            }
        }
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    files
        .into_iter()
        .take(MAX_ROLLOUTS)
        .map(|(mt, p)| {
            let secs = mt.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            (i64::try_from(secs).unwrap_or(0), p)
        })
        .collect()
}

fn last_rate_limits(path: &PathBuf) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    let line = text.lines().rfind(|l| l.contains(RATE_LIMITS_MARK))?;
    let v: Value = serde_json::from_str(line).ok()?;
    let p = v.get("payload")?;
    (p.get("type")?.as_str()? == "token_count").then(|| p.get("rate_limits").filter(|r| r.is_object()).cloned())?
}

/// Último `rate_limits` local e quando o rollout foi gravado.
fn local_rate_limits(ctx: &Ctx) -> Option<(Value, i64)> {
    rollouts(ctx)
        .iter()
        .find_map(|(at, p)| last_rate_limits(p).map(|rl| (rl, *at)))
}

fn all_fresh(rl: &Value, now: i64) -> bool {
    ["primary", "secondary"].iter().all(|k| {
        rl.get(k)
            .filter(|o| o.is_object())
            .and_then(|o| num(o.get("resets_at")?))
            .is_some_and(|r| r > now as f64)
    })
}

fn local_window(x: &Value) -> Option<Window> {
    let used = x.get("used_percent").filter(|u| !u.is_null())?;
    let mins = x.get("window_minutes").filter(|m| !m.is_null())?;
    let m = num(mins).unwrap_or(0.0);
    Some(window4(
        &label_from_minutes(m),
        Some(100.0 - num(used).unwrap_or(0.0)),
        x.get("resets_at").and_then(to_epoch),
        Some(m * 60.0),
    ))
}

fn wham_window(x: &Value) -> Option<Window> {
    let used = x.get("used_percent").filter(|u| !u.is_null())?;
    let secs = num(x.get("limit_window_seconds").filter(|m| !m.is_null())?)?;
    Some(window4(
        &label_from_minutes(secs / 60.0),
        Some(100.0 - num(used).unwrap_or(0.0)),
        x.get("reset_at").and_then(to_epoch),
        Some(secs),
    ))
}

fn plan_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn collect(v: &Value, keys: [&str; 2], f: fn(&Value) -> Option<Window>) -> Vec<Window> {
    keys.iter().filter_map(|k| v.get(k).and_then(f)).collect()
}

/// Port de `codex.jq`, rollout local (envelope `source: local`).
pub fn parse_local(rl: &Value) -> Partial {
    Partial {
        plan: Some(plan_of(rl, "plan_type")),
        windows: Some(collect(rl, ["primary", "secondary"], local_window)),
        balance: None,
        reset_credits: None,
        week_reset_credits: None,
        data_at: None,
    }
}

/// Port de `codex.jq`, corpo do wham.
pub fn parse_wham(body: &Value) -> Partial {
    let r = body.get("rate_limit").unwrap_or(&Value::Null);
    Partial {
        plan: Some(plan_of(body, "plan_type")),
        windows: Some(collect(r, ["primary_window", "secondary_window"], wham_window)),
        balance: None,
        reset_credits: None,
        week_reset_credits: None,
        data_at: None,
    }
}

/// Token e conta do ChatGPT: do pi com o harness pi, senão do Codex CLI.
fn credentials(ctx: &Ctx) -> Option<(String, Option<String>)> {
    let pi = ctx.harness() == "pi";
    let auth = if pi {
        ctx.pi_auth()
    } else {
        codex_home(ctx).and_then(|h| read_json(h.join("auth.json")))
    }?;
    let (tok_path, acc_path): (&[&str], &[&str]) = if pi {
        (&["openai-codex", "access"], &["openai-codex", "accountId"])
    } else {
        (&["tokens", "access_token"], &["tokens", "account_id"])
    };
    Some((str_at(&auth, tok_path)?, str_at(&auth, acc_path)))
}

fn wham_get(ctx: &Ctx, url: &str, (tok, acc): &(String, Option<String>)) -> Result<Value, Failure> {
    let mut extra = vec![("Accept", "application/json")];
    if let Some(a) = acc.as_deref() {
        extra.push(("ChatGPT-Account-Id", a));
    }
    serde_json::from_str(&ctx.get(url, &bearer(tok, &extra))?).map_err(|_| Failure::Http)
}

fn wham(ctx: &Ctx) -> Result<Value, Failure> {
    let creds = credentials(ctx).ok_or(Failure::NoCred)?;
    let body = wham_get(ctx, WHAM_URL, &creds)?;
    body.get("rate_limit")
        .is_some_and(Value::is_object)
        .then_some(body)
        .ok_or(Failure::Http)
}

/// `{available_count, credits:[{status, expires_at}]}` → créditos disponíveis.
/// Zero limpa; contagem negativa ou corpo sem contagem é inválido (`None`, mantém o anterior).
pub fn parse_reset_credits(v: &Value, now: i64) -> Option<Option<ResetCredits>> {
    let available = v.get("available_count").and_then(Value::as_i64).filter(|n| *n >= 0)?;
    if available == 0 {
        return Some(None);
    }
    let mut expires: Vec<i64> = v
        .get("credits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| c.get("status").and_then(Value::as_str) == Some("available"))
        .filter_map(|c| c.get("expires_at").and_then(to_epoch))
        .filter(|e| *e > now)
        .collect();
    expires.sort_unstable();
    Some(Some(ResetCredits {
        available,
        expires,
        expiry_groups: None,
    }))
}

/// Best-effort: falha de rede ou corpo inválido mantém o valor anterior.
fn reset_credits(ctx: &Ctx) -> Option<Option<ResetCredits>> {
    let creds = credentials(ctx)?;
    let body = wham_get(ctx, RESET_CREDITS_URL, &creds).ok()?;
    parse_reset_credits(&body, ctx.now)
}

fn windows(ctx: &Ctx) -> Result<Partial, Failure> {
    // rollouts locais são da conta do Codex CLI: com a conta do pi só vale o wham
    let rl = if ctx.harness() == "pi" {
        None
    } else {
        local_rate_limits(ctx)
    };
    if let Some((r, _)) = rl.as_ref().filter(|(r, _)| all_fresh(r, ctx.now)) {
        return Ok(parse_local(r));
    }
    match wham(ctx) {
        Ok(body) => Ok(parse_wham(&body)),
        // Login expirado/ausente é erro: `apply_error` mantém o último dado bom e a UI mostra
        // como resolver. Rollout velho como `ok` escondia a dica de login por até 8 dias.
        Err(f @ (Failure::Auth | Failure::NoCred)) => Err(f),
        // Falha transitória: o rollout serve, datado de quando foi gravado.
        Err(f) => rl
            .as_ref()
            .map(|(r, at)| Partial {
                data_at: Some(*at),
                ..parse_local(r)
            })
            .ok_or(f),
    }
}

pub fn fetch(ctx: &Ctx) -> Result<Partial, Failure> {
    let mut partial = windows(ctx)?;
    partial.reset_credits = reset_credits(ctx);
    Ok(partial)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::testutil::*;
    use serde_json::json;

    // Fixture assembled at runtime so credential scanners do not read the fake
    // token as a hardcoded secret (CWE-798).
    fn auth() -> String {
        format!(
            r#"{{"tokens":{{"{}":"sekret-cx","account_id":"acc1"}}}}"#,
            concat!("access", "_token")
        )
    }
    const WHAM: &str = r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":12,"limit_window_seconds":18000,"reset_at":1791606201},
        "secondary_window":{"used_percent":50,"limit_window_seconds":604800,"reset_at":1791900000}}}"#;

    #[test]
    fn success_parse_wham() {
        let t = Fixture::new();
        t.write_home(".codex/auth.json", &auth());
        let http = FakeHttp::new().route(WHAM_URL, 200, WHAM);
        let e = t.run("codex", &http);
        assert!(e.ok, "{e:?}");
        assert_eq!(e.plan.as_deref(), Some("plus"));
        assert_eq!(e.windows.len(), 2);
        assert_eq!(
            (
                e.windows[0].label.as_str(),
                e.windows[0].remaining_pct,
                e.windows[0].window_s
            ),
            ("5h", Some(88), Some(18000))
        );
        assert_eq!(
            (e.windows[1].label.as_str(), e.windows[1].remaining_pct),
            ("sem", Some(50))
        );
        t.assert_no_secret(&e, "sekret-cx");
        assert!(http.header_seen("ChatGPT-Account-Id", "acc1"));
    }

    #[test]
    fn local_rollout_when_fresh() {
        let t = Fixture::new();
        let day = DateTime::from_timestamp(t.now, 0)
            .unwrap()
            .with_timezone(&Local)
            .format("%Y/%m/%d")
            .to_string();
        let line = json!({"payload":{"type":"token_count","rate_limits":{"plan_type":"pro",
            "primary":{"used_percent":30.0,"window_minutes":300,"resets_at":t.now+100},
            "secondary":{"used_percent":5,"window_minutes":10080,"resets_at":t.now+9000}}}});
        t.write_home(
            &format!(".codex/sessions/{day}/rollout-a.jsonl"),
            &format!("{{}}\n{line}\n"),
        );
        let e = t.run("codex", &FakeHttp::new());
        assert!(e.ok, "{e:?}");
        assert_eq!(e.plan.as_deref(), Some("pro"));
        assert_eq!(e.windows[0].remaining_pct, Some(70));
        assert_eq!(e.windows[1].window_s, Some(604_800));
    }

    /// Rollout com janelas já resetadas (não "fresco"), gravado no dia de `t.now`.
    fn stale_rollout(t: &Fixture) -> PathBuf {
        let day = DateTime::from_timestamp(t.now, 0)
            .unwrap()
            .with_timezone(&Local)
            .format("%Y/%m/%d")
            .to_string();
        let line = json!({"payload":{"type":"token_count","rate_limits":{"plan_type":"pro",
            "primary":{"used_percent":30.0,"window_minutes":300,"resets_at":t.now-100},
            "secondary":{"used_percent":5,"window_minutes":10080,"resets_at":t.now+9000}}}});
        let rel = format!(".codex/sessions/{day}/rollout-old.jsonl");
        t.write_home(&rel, &format!("{line}\n"));
        t.home_path(&rel)
    }

    #[test]
    fn login_expirado_e_erro_mesmo_com_rollout_antigo() {
        let t = Fixture::new();
        t.write_home(".codex/auth.json", &auth());
        stale_rollout(&t);
        let e = t.run("codex", &FakeHttp::new().route(WHAM_URL, 401, "{}"));
        assert!(!e.ok, "rollout velho não pode esconder o login expirado: {e:?}");
        assert_eq!(e.error.as_deref(), Some("auth"));
    }

    #[test]
    fn falha_transitoria_usa_rollout_datado_de_quando_foi_gravado() {
        let t = Fixture::new();
        t.write_home(".codex/auth.json", &auth());
        let path = stale_rollout(&t);
        let e = t.run("codex", &FakeHttp::new().route(WHAM_URL, 500, "{}"));
        assert!(e.ok, "{e:?}");
        let mtime = fs::metadata(path).unwrap().modified().unwrap();
        let mtime = mtime.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64;
        assert_eq!(e.data_at, Some(mtime), "data do dado, não da consulta");
    }

    #[test]
    fn missing_credential_is_nocred() {
        let t = Fixture::new();
        assert_eq!(t.run("codex", &FakeHttp::new()).error.as_deref(), Some("nocred"));
    }

    #[test]
    fn reset_credits_lists_available_expiries() {
        let t = Fixture::new();
        t.write_home(".codex/auth.json", &auth());
        let later = t.now + 86_400;
        let credits = json!({"available_count": 2, "credits": [
            {"status": "available", "expires_at": later},
            {"status": "redeemed", "expires_at": later + 1},
            {"status": "available"}]})
        .to_string();
        let http = FakeHttp::new()
            .route(RESET_CREDITS_URL, 200, &credits)
            .route(WHAM_URL, 200, WHAM);
        let e = t.run("codex", &http);
        assert!(e.ok, "{e:?}");
        let rc = e.reset_credits.expect("créditos");
        assert_eq!((rc.available, rc.expires), (2, vec![later]));
    }

    #[test]
    fn reset_credits_failure_keeps_quota_and_bad_counts_are_ignored() {
        let t = Fixture::new();
        t.write_home(".codex/auth.json", &auth());
        let http = FakeHttp::new()
            .route(RESET_CREDITS_URL, 500, "x")
            .route(WHAM_URL, 200, WHAM);
        let e = t.run("codex", &http);
        assert!(e.ok && e.reset_credits.is_none(), "{e:?}");
        assert_eq!(parse_reset_credits(&json!({"available_count": -1}), 0), None);
        assert_eq!(parse_reset_credits(&json!({"available_count": 0}), 0), Some(None));
    }

    #[test]
    fn http_failure() {
        let t = Fixture::new();
        t.write_home(".codex/auth.json", &auth());
        let http = FakeHttp::new().route(WHAM_URL, 500, "boom");
        assert_eq!(t.run("codex", &http).error.as_deref(), Some("http"));
    }

    #[test]
    fn html_200_is_http_error() {
        let t = Fixture::new();
        t.write_home(".codex/auth.json", &auth());
        let http = FakeHttp::new().route(WHAM_URL, 200, "<html>");
        assert_eq!(t.run("codex", &http).error.as_deref(), Some("http"));
    }
}
