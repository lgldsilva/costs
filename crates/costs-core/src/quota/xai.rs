//! xAI / SuperGrok: billing do Grok CLI, com fallback de saldo pré-pago (management API).

use std::path::PathBuf;

use serde_json::{Value, json};

use super::creds::{Ctx, Failure, bearer, read_json, str_at};
use super::entry::{Balance, Partial};
use super::norm::{label_from_minutes, label_from_value, label_window_s, num, to_epoch, window4};

const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const MGMT_URL: &str = "https://management-api.x.ai/v1/billing/teams";
const GROK_KEY_PREFIX: &str = "https://auth.x.ai::";
const EXPIRY_MARGIN_S: i64 = 60;
const DEFAULT_LABEL: &str = "cota";

fn order(harness: &str) -> [&'static str; 3] {
    match harness {
        "pi" => ["pi", "grok", "opencode"],
        "opencode" => ["opencode", "pi", "grok"],
        _ => ["grok", "pi", "opencode"],
    }
}

fn live(ctx: &Ctx, exp: Option<i64>) -> bool {
    exp.is_none_or(|e| e > ctx.now + EXPIRY_MARGIN_S)
}

fn grok_token(ctx: &Ctx) -> Result<String, Failure> {
    let home = ctx
        .env
        .var("GROK_HOME")
        .map(PathBuf::from)
        .or_else(|| ctx.home_file(".grok"))
        .ok_or(Failure::NoCred)?;
    let auth = read_json(home.join("auth.json")).ok_or(Failure::NoCred)?;
    let found: Vec<(String, Option<i64>)> = auth
        .as_object()
        .ok_or(Failure::NoCred)?
        .iter()
        .filter(|(k, _)| k.starts_with(GROK_KEY_PREFIX))
        .filter(|(_, v)| v.is_object())
        .filter_map(|(_, v)| Some((str_at(v, &["key"])?, v.get("expires_at").and_then(to_epoch))))
        .collect();
    if found.is_empty() {
        return Err(Failure::NoCred);
    }
    found
        .into_iter()
        .find(|(_, e)| live(ctx, *e))
        .map(|(t, _)| t)
        .ok_or(Failure::Expired)
}

fn oauth_token(ctx: &Ctx, auth: Option<Value>) -> Result<String, Failure> {
    let x = auth
        .as_ref()
        .and_then(|a| a.get("xai"))
        .filter(|x| x.get("type").and_then(Value::as_str) == Some("oauth"))
        .ok_or(Failure::NoCred)?;
    let tok = str_at(x, &["access"]).ok_or(Failure::NoCred)?;
    if live(ctx, x.get("expires").and_then(to_epoch)) {
        Ok(tok)
    } else {
        Err(Failure::Expired)
    }
}

fn opencode_auth_xdg(ctx: &Ctx) -> Option<Value> {
    let base = ctx
        .env
        .var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| ctx.home_file(".local/share"))?;
    read_json(base.join("opencode/auth.json"))
}

/// Primeira fonte com token vivo; sem nenhuma, `Expired` se alguma tinha token vencido.
fn pick_token(ctx: &Ctx) -> Result<String, Failure> {
    let mut failure = Failure::NoCred;
    for src in order(&ctx.harness()) {
        let r = match src {
            "grok" => grok_token(ctx),
            "pi" => oauth_token(ctx, ctx.pi_auth()),
            _ => oauth_token(ctx, opencode_auth_xdg(ctx)),
        };
        match r {
            Ok(t) => return Ok(t),
            Err(f) => failure = failure.most_specific(f),
        }
    }
    Err(failure)
}

fn cents(v: Option<&Value>) -> Option<f64> {
    let v = v.filter(|v| !v.is_null())?;
    match v {
        Value::Object(o) => num(o.get("val")?),
        _ => num(v),
    }
}

/// Primeiro valor não nulo (jq `a // b // c`).
fn first<'a>(c: &[Option<&'a Value>]) -> Option<&'a Value> {
    c.iter()
        .flatten()
        .find(|v| !v.is_null() && ***v != Value::Bool(false))
        .copied()
}

/// Port de `xai.jq`: janela de créditos e saldo USD; sem nenhum dos dois é erro.
pub fn parse(v: &Value) -> Option<Partial> {
    let empty = json!({});
    let cfg = v.get("config").filter(|c| !c.is_null()).unwrap_or(&empty);
    let both = |k: &str| first(&[cfg.get(k), v.get(k)]);
    let period = both("currentPeriod").unwrap_or(&empty);
    let used = both("creditUsagePercent").and_then(num);
    let start = first(&[
        period.get("start"),
        cfg.get("billingPeriodStart"),
        v.get("billingPeriodStart"),
    ]);
    let end = first(&[
        period.get("end"),
        cfg.get("billingPeriodEnd"),
        v.get("billingPeriodEnd"),
    ]);
    let mins = match (start.and_then(to_epoch), end.and_then(to_epoch)) {
        (Some(s), Some(e)) if e > s => Some(((e - s) / 60) as f64),
        _ => None,
    };
    let prepaid = cents(both("prepaidBalance"));
    let total = v.get("managementPrepaid").and_then(|m| cents(m.get("total")));
    let usd = match (prepaid, total) {
        (Some(p), _) if p > 0.0 => Some(p / 100.0),
        (_, Some(t)) => Some(if t < 0.0 { -t / 100.0 } else { 0.0 }),
        _ => None,
    };
    let plan = both("subscriptionTier")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let label = period
        .get("type")
        .and_then(label_from_value)
        .or_else(|| mins.map(label_from_minutes))
        .unwrap_or_else(|| DEFAULT_LABEL.to_string());
    let windows: Vec<_> = used
        .map(|u| {
            let ws = mins.map(|m| m * 60.0).or_else(|| label_window_s(&label));
            window4(&label, Some(100.0 - u), end.and_then(to_epoch), ws)
        })
        .into_iter()
        .collect();
    if windows.is_empty() && usd.is_none() {
        return None;
    }
    Some(Partial {
        plan: Some(plan.map(str::to_string)),
        windows: Some(windows),
        balance: Some(usd.map(|amount| Balance {
            amount,
            currency: "USD".into(),
        })),
        reset_credits: None,
        week_reset_credits: None,
        data_at: None,
    })
}

fn prepaid(ctx: &Ctx) -> Result<Value, Failure> {
    let key = ctx.env.var("XAI_MANAGEMENT_KEY");
    let tid = ctx.env.var("XAI_TEAM_ID");
    let (Some(key), Some(tid)) = (key, tid) else {
        return Err(Failure::NoCred);
    };
    if !tid.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(Failure::NoCred);
    }
    let url = format!("{MGMT_URL}/{tid}/prepaid/balance");
    let body = ctx.get(&url, &bearer(&key, &[("Accept", "application/json")]))?;
    let v: Value = serde_json::from_str(&body).map_err(|_| Failure::Http)?;
    Ok(json!({ "managementPrepaid": v }))
}

pub fn fetch(ctx: &Ctx) -> Result<Partial, Failure> {
    let billing_err = match pick_token(ctx) {
        Err(f) => f,
        Ok(tok) => {
            let h = bearer(
                &tok,
                &[("x-xai-token-auth", "xai-grok-cli"), ("Accept", "application/json")],
            );
            match ctx.get(BILLING_URL, &h) {
                Ok(body) => {
                    let v = serde_json::from_str(&body).map_err(|_| Failure::Parse)?;
                    return parse(&v).ok_or(Failure::Parse);
                }
                Err(f) => f,
            }
        }
    };
    // sem chave de management: vale o erro do billing
    let pre = match prepaid(ctx) {
        Err(Failure::NoCred) => return Err(billing_err),
        Err(_) => return Err(Failure::Http),
        Ok(p) => p,
    };
    parse(&pre).ok_or(Failure::Parse)
}

#[cfg(test)]
mod tests {
    use crate::quota::entry::Balance;
    use crate::quota::testutil::*;

    const AUTH: &str = r#"{"https://auth.x.ai::abc":{"key":"sekret-xai","expires_at":"2099-01-01T00:00:00Z"}}"#;
    const BODY: &str = r#"{"config":{"creditUsagePercent":"37","subscriptionTier":"SuperGrok",
        "currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","start":"2026-10-05T00:00:00Z","end":"2026-10-12T00:00:00Z"},
        "prepaidBalance":{"val":"1250"}}}"#;

    #[test]
    fn success_parse() {
        let t = Fixture::new();
        t.write_home(".grok/auth.json", AUTH);
        let http = FakeHttp::new().route("https://cli-chat-proxy.grok.com/", 200, BODY);
        let e = t.run("xai", &http);
        assert!(e.ok, "{e:?}");
        assert_eq!(e.plan.as_deref(), Some("SuperGrok"));
        assert_eq!(
            (e.windows[0].label.as_str(), e.windows[0].remaining_pct),
            ("sem", Some(63))
        );
        assert_eq!(e.windows[0].window_s, Some(604_800));
        assert_eq!(
            e.balance,
            Some(Balance {
                amount: 12.5,
                currency: "USD".into()
            })
        );
        t.assert_no_secret(&e, "sekret-xai");
        assert!(http.header_seen("x-xai-token-auth", "xai-grok-cli"));
    }

    #[test]
    fn management_fallback_balance() {
        let t = Fixture::new();
        t.write_home(".grok/auth.json", AUTH);
        let http = FakeHttp::new()
            .route("https://cli-chat-proxy.grok.com/", 500, "x")
            .route(
                "https://management-api.x.ai/v1/billing/teams/team_1/prepaid/balance",
                200,
                r#"{"total":{"val":"-4200"}}"#,
            );
        let e = t
            .with_env("XAI_MANAGEMENT_KEY", "mk")
            .with_env("XAI_TEAM_ID", "team_1")
            .run("xai", &http);
        assert!(e.ok, "{e:?}");
        assert_eq!(e.balance.map(|b| b.amount), Some(42.0));
        assert!(e.windows.is_empty());
    }

    #[test]
    fn expired_token_is_expired() {
        let t = Fixture::new();
        t.write_home(
            ".grok/auth.json",
            &AUTH.replace("2099-01-01T00:00:00Z", "2001-01-01T00:00:00Z"),
        );
        assert_eq!(t.run("xai", &FakeHttp::new()).error.as_deref(), Some("expired"));
    }

    #[test]
    fn missing_credential_is_nocred() {
        let t = Fixture::new();
        assert_eq!(t.run("xai", &FakeHttp::new()).error.as_deref(), Some("nocred"));
    }

    #[test]
    fn http_failure() {
        let t = Fixture::new();
        t.write_home(".grok/auth.json", AUTH);
        let http = FakeHttp::new().route("https://cli-chat-proxy.grok.com/", 502, "x");
        assert_eq!(t.run("xai", &http).error.as_deref(), Some("http"));
    }
}
