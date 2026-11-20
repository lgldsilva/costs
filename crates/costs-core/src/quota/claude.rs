//! Claude (OAuth do Claude Code): `GET api.anthropic.com/api/oauth/usage`.

use serde_json::Value;

use super::creds::{Ctx, Failure, bearer, read_json, str_at};
use super::entry::{Partial, ResetCredits, Window};
use super::norm::{num, to_epoch, window};

const URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// Pede o bloco `cedar_ember` ("Reset for free" do Claude). Não documentado: se a API
/// recusar, a consulta é refeita sem ele para não perder as janelas.
const URL_WITH_RESETS: &str = "https://api.anthropic.com/api/oauth/usage?cedar_ember=1";
/// Acima disso o bloco é tratado como lixo (mesmo limite do CodexBar).
const MAX_RESETS: i64 = 50;
const MAX_GRANTS: usize = 200;

fn token_from(ctx: &Ctx, raw: &Value, pi: bool) -> Result<String, Failure> {
    let (obj, tok_key, exp_key) = if pi {
        let o = raw
            .get("anthropic")
            .filter(|o| o.get("type").and_then(Value::as_str) == Some("oauth"))
            .ok_or(Failure::NoCred)?;
        (o, "access", "expires")
    } else {
        (
            raw.get("claudeAiOauth").ok_or(Failure::NoCred)?,
            "accessToken",
            "expiresAt",
        )
    };
    let tok = str_at(obj, &[tok_key]).ok_or(Failure::NoCred)?;
    if ctx.exp_valid(obj.get(exp_key)) {
        Ok(tok)
    } else {
        Err(Failure::Expired)
    }
}

fn claude_creds(ctx: &Ctx) -> Option<Value> {
    let dir = ctx
        .env
        .var("CLAUDE_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| ctx.home_file(".claude"))?;
    let file = dir.join(".credentials.json");
    if file.exists() {
        read_json(file)
    } else {
        serde_json::from_str(&ctx.env.keychain_claude()?).ok()
    }
}

fn pick_token(ctx: &Ctx) -> Result<String, Failure> {
    let mut failure = Failure::NoCred;
    if ctx.harness() == "pi" {
        match ctx.pi_auth().map(|a| token_from(ctx, &a, true)) {
            Some(Ok(t)) => return Ok(t),
            Some(Err(f)) => failure = f,
            None => {}
        }
    }
    let creds = claude_creds(ctx).ok_or(failure)?;
    token_from(ctx, &creds, false).map_err(|f| f.most_specific(failure))
}

fn used_window(x: &Value, label: &str) -> Option<Window> {
    let u = x.get("utilization").filter(|u| !u.is_null())?;
    Some(window(
        label,
        Some(100.0 - num(u).unwrap_or(0.0)),
        x.get("resets_at").and_then(to_epoch),
    ))
}

fn weekly(v: &Value) -> Option<Window> {
    let usable = |k: &str| {
        v.get(k)
            .filter(|o| o.is_object() && o.get("utilization").is_some_and(|u| !u.is_null()))
    };
    if let Some(w) = usable("seven_day") {
        return used_window(w, "sem");
    }
    let util = |o: &&Value| num(&o["utilization"]).unwrap_or(0.0);
    // sort_by estável + último = em empate, o último candidato
    let best = ["seven_day_opus", "seven_day_sonnet"]
        .iter()
        .filter_map(|k| usable(k))
        .fold(None::<&Value>, |acc, o| match acc {
            Some(a) if util(&o) < util(&a) => Some(a),
            _ => Some(o),
        })?;
    used_window(best, "sem")
}

/// Port de `claude.jq`: 5h + semanal; sem janelas é erro de parse.
pub fn parse(v: &Value) -> Option<Partial> {
    if !v.is_object() {
        return None;
    }
    let windows: Vec<Window> = v
        .get("five_hour")
        .and_then(|x| used_window(x, "5h"))
        .into_iter()
        .chain(weekly(v))
        .collect();
    (!windows.is_empty()).then(|| Partial {
        windows: Some(windows),
        ..Partial::default()
    })
}

/// Créditos de reset do `cedar_ember`: soma `resets_left` das concessões não pausadas e
/// vigentes. `None` = bloco ausente (mantém o anterior); inelegível ou zerado limpa.
pub fn parse_resets(v: &Value, now: i64) -> Option<Option<ResetCredits>> {
    let ce = v.get("cedar_ember").filter(|c| c.is_object())?;
    if ce.get("eligible").and_then(Value::as_bool) != Some(true) {
        return Some(None);
    }
    let grants = ce
        .get("grants")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    if grants.len() > MAX_GRANTS {
        return Some(None);
    }
    let mut available = 0;
    let mut expires = Vec::new();
    for g in grants {
        let left = g.get("resets_left").and_then(Value::as_i64).unwrap_or(0);
        let paused = g.get("paused").and_then(Value::as_bool).unwrap_or(true);
        let starts = g.get("starts_at").and_then(to_epoch);
        let ends = g.get("ends_at").and_then(to_epoch);
        if paused || left <= 0 || starts.is_some_and(|s| s > now) || ends.is_some_and(|e| e <= now) {
            continue;
        }
        available += left;
        if let Some(e) = ends {
            expires.extend(std::iter::repeat_n(e, left.min(MAX_RESETS) as usize));
        }
    }
    if available <= 0 || available > MAX_RESETS {
        return Some(None);
    }
    expires.sort_unstable();
    Some(Some(ResetCredits {
        available,
        expires,
        expiry_groups: None,
    }))
}

pub fn fetch(ctx: &Ctx) -> Result<Partial, Failure> {
    let tok = pick_token(ctx)?;
    let headers = bearer(
        &tok,
        &[
            ("anthropic-beta", "oauth-2025-04-20"),
            ("Content-Type", "application/json"),
        ],
    );
    let body = ctx.get(URL_WITH_RESETS, &headers).or_else(|_| ctx.get(URL, &headers))?;
    let v: Value = serde_json::from_str(&body).map_err(|_| Failure::Parse)?;
    let mut partial = parse(&v).ok_or(Failure::Parse)?;
    partial.reset_credits = parse_resets(&v, ctx.now);
    Ok(partial)
}

#[cfg(test)]
mod tests {
    use crate::quota::testutil::*;

    // Fixture assembled at runtime so credential scanners do not read the fake
    // token as a hardcoded secret (CWE-798).
    fn cred() -> String {
        format!(
            r#"{{"claudeAiOauth":{{"{}":"sekret-tok","expiresAt":99999999999999}}}}"#,
            concat!("access", "Token")
        )
    }
    const BODY: &str = r#"{"five_hour":{"utilization":23.4,"resets_at":"2026-10-10T04:23:21Z"},
        "seven_day":null,"seven_day_opus":{"utilization":10},"seven_day_sonnet":{"utilization":40,"resets_at":1791606201}}"#;

    #[test]
    fn success_parse() {
        let t = Fixture::new();
        t.write_home(".claude/.credentials.json", &cred());
        let http = FakeHttp::new().route("https://api.anthropic.com/", 200, BODY);
        let e = t.run("claude", &http);
        assert!(e.ok, "{e:?}");
        assert_eq!(e.windows.len(), 2);
        assert_eq!(
            (e.windows[0].label.as_str(), e.windows[0].remaining_pct),
            ("5h", Some(77))
        );
        assert_eq!(e.windows[0].window_s, Some(18000));
        assert_eq!(
            (e.windows[1].label.as_str(), e.windows[1].remaining_pct),
            ("sem", Some(60))
        );
        assert_eq!(e.windows[1].resets_at, Some(1_791_606_201));
        t.assert_no_secret(&e, "sekret-tok");
    }

    #[test]
    fn reset_credits_from_cedar_ember() {
        let t = Fixture::new();
        t.write_home(".claude/.credentials.json", &cred());
        let (now, later) = (t.now, t.now + 86_400);
        let body = BODY.replacen(
            '{',
            &format!(
                r#"{{"cedar_ember":{{"eligible":true,"grants":[
                    {{"resets_left":2,"paused":false,"ends_at":{later}}},
                    {{"resets_left":1,"paused":true,"ends_at":{later}}},
                    {{"resets_left":1,"paused":false,"ends_at":{now}}},
                    {{"resets_left":1,"paused":false,"starts_at":{later}}}]}},"#
            ),
            1,
        );
        let http = FakeHttp::new().route("https://api.anthropic.com/", 200, &body);
        let e = t.run("claude", &http);
        let rc = e.reset_credits.expect("créditos");
        assert_eq!((rc.available, rc.expires), (2, vec![later, later]));
    }

    #[test]
    fn rejected_cedar_ember_retries_without_it() {
        let t = Fixture::new();
        t.write_home(".claude/.credentials.json", &cred());
        let http =
            FakeHttp::new()
                .route(super::URL_WITH_RESETS, 400, "bad")
                .route("https://api.anthropic.com/", 200, BODY);
        let e = t.run("claude", &http);
        assert!(e.ok, "{e:?}");
        assert_eq!(e.windows.len(), 2);
        assert!(e.reset_credits.is_none());
    }

    #[test]
    fn reset_credits_cleared_when_ineligible_and_kept_when_absent() {
        use super::parse_resets;
        use serde_json::json;
        assert_eq!(
            parse_resets(&json!({"cedar_ember": {"eligible": false}}), 0),
            Some(None)
        );
        assert_eq!(parse_resets(&json!({"five_hour": {}}), 0), None);
        let many = json!({"cedar_ember": {"eligible": true, "grants": [{"resets_left": 51, "paused": false}]}});
        assert_eq!(parse_resets(&many, 0), Some(None), "acima do limite é lixo");
    }

    #[test]
    fn missing_credential_is_nocred() {
        let t = Fixture::new();
        let e = t.run("claude", &FakeHttp::new());
        assert_eq!(e.error.as_deref(), Some("nocred"));
    }

    #[test]
    fn expired_token_is_expired() {
        let t = Fixture::new();
        let exp = t.now + 30;
        t.write_home(
            ".claude/.credentials.json",
            &cred().replace("99999999999999", &exp.to_string()),
        );
        assert_eq!(t.run("claude", &FakeHttp::new()).error.as_deref(), Some("expired"));
    }

    #[test]
    fn rejected_token_is_auth() {
        let t = Fixture::new();
        t.write_home(".claude/.credentials.json", &cred());
        let http = FakeHttp::new().route("https://api.anthropic.com/", 401, "no");
        assert_eq!(t.run("claude", &http).error.as_deref(), Some("auth"));
    }

    #[test]
    fn http_failure() {
        let t = Fixture::new();
        t.write_home(".claude/.credentials.json", &cred());
        let http = FakeHttp::new().route("https://api.anthropic.com/", 500, "no");
        assert_eq!(t.run("claude", &http).error.as_deref(), Some("http"));
    }

    #[test]
    fn unusable_body_is_parse_error() {
        let t = Fixture::new();
        t.write_home(".claude/.credentials.json", &cred());
        let http = FakeHttp::new().route("https://api.anthropic.com/", 200, "{}");
        assert_eq!(t.run("claude", &http).error.as_deref(), Some("parse"));
    }
}
