//! Z.ai (GLM Coding Plan): `GET api.z.ai/api/monitor/usage/quota/limit`.

use serde_json::Value;

use super::creds::{Ctx, Failure, bearer};
use super::entry::{Partial, Window};
use super::norm::{label_from_minutes, num, to_epoch, window4};
use super::zai_resets;

const URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";
const OK_CODE: f64 = 200.0;

fn unit_minutes(unit: f64) -> Option<f64> {
    match unit as i64 {
        1 => Some(1440.0),
        3 => Some(60.0),
        5 => Some(43200.0),
        6 => Some(10080.0),
        _ => None,
    }
}

fn limit_window(l: &Value) -> Option<(f64, Window)> {
    let unit = unit_minutes(l.get("unit")?.as_f64()?)?;
    let number = l.get("number")?.as_f64()?;
    let pct = l.get("percentage")?.as_f64()?;
    let minutes = unit * number;
    let mcp = l.get("type").and_then(Value::as_str) == Some("TIME_LIMIT")
        && l.get("usageDetails").is_some_and(Value::is_array);
    let label = if mcp {
        "mcp".to_string()
    } else {
        label_from_minutes(minutes)
    };
    let resets = l.get("nextResetTime").and_then(to_epoch);
    let mut window = window4(&label, Some(100.0 - pct), resets, Some(minutes * 60.0));
    window.window_kind = Some(
        match minutes as i64 {
            300 => "session",
            10080 => "week",
            43200 => "month",
            _ => "other",
        }
        .to_owned(),
    );
    Some((minutes, window))
}

/// Port de `zai.jq`: sem sucesso é erro; janelas ordenadas por duração.
pub fn parse(v: &Value) -> Option<Partial> {
    let code = v.get("code").filter(|c| !c.is_null()).and_then(num).unwrap_or(OK_CODE);
    if v.get("success") == Some(&Value::Bool(false)) || code != OK_CODE {
        return None;
    }
    let data = v.get("data").unwrap_or(&Value::Null);
    let mut ws: Vec<(f64, Window)> = data
        .get("limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|l| {
            matches!(
                l.get("type").and_then(Value::as_str),
                Some("TOKENS_LIMIT" | "TIME_LIMIT")
            )
        })
        .filter_map(limit_window)
        .collect();
    ws.sort_by(|a, b| a.0.total_cmp(&b.0));
    Some(Partial {
        plan: Some(data.get("level").and_then(Value::as_str).map(str::to_string)),
        windows: Some(ws.into_iter().map(|w| w.1).collect()),
        balance: None,
        reset_credits: None,
        week_reset_credits: None,
        data_at: None,
    })
}

pub fn fetch(ctx: &Ctx) -> Result<Partial, Failure> {
    let key = ctx
        .api_key("zai", &["ZAI_API_KEY", "Z_AI_API_KEY"])
        .ok_or(Failure::NoKey)?;
    let body = ctx.get(URL, &bearer(&key, &[]))?;
    let mut p = parse(&serde_json::from_str(&body).map_err(|_| Failure::Parse)?).ok_or(Failure::Parse)?;
    // Best-effort enrichment: any failure here keeps the quota result untouched. Skipped
    // (previous credits kept by `apply_partial`) unless `quota::fetch` allowed it.
    if ctx.enrich_resets
        && let Some((five_hour, week)) = zai_resets::enrich(ctx, &key)
    {
        p.reset_credits = Some(Some(five_hour));
        p.week_reset_credits = Some(Some(week));
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use crate::quota::testutil::*;

    const BODY: &str = r#"{"code":200,"success":true,"data":{"level":"pro","limits":[
        {"type":"TIME_LIMIT","unit":5,"number":1,"percentage":10,"usageDetails":[{}],"nextResetTime":1791900000000},
        {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":42,"nextResetTime":1791606201000},
        {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":7,"nextResetTime":1791700000000},
        {"type":"CREDIT_LIMIT","unit":3,"number":1,"percentage":1}]}}"#;

    #[test]
    fn success_parse() {
        let t = Fixture::new();
        let http = FakeHttp::new().route("https://api.z.ai/", 200, BODY);
        let e = t.with_env("ZAI_API_KEY", "sekret-z").run("zai", &http);
        assert!(e.ok, "{e:?}");
        assert_eq!(e.plan.as_deref(), Some("pro"));
        let got: Vec<_> = e.windows.iter().map(|w| (w.label.as_str(), w.remaining_pct)).collect();
        assert_eq!(got, [("5h", Some(58)), ("sem", Some(93)), ("mcp", Some(90))]);
        assert_eq!(
            e.windows
                .iter()
                .map(|w| (w.label.as_str(), w.window_kind.as_deref()))
                .collect::<Vec<_>>(),
            [("5h", Some("session")), ("sem", Some("week")), ("mcp", Some("month"))]
        );
        assert_eq!(e.windows[0].resets_at, Some(1_791_606_201));
        t.assert_no_secret(&e, "sekret-z");
    }

    #[test]
    fn missing_key_is_nokey() {
        let t = Fixture::new();
        assert_eq!(t.run("zai", &FakeHttp::new()).error.as_deref(), Some("nokey"));
    }

    #[test]
    fn http_failure() {
        let t = Fixture::new();
        let http = FakeHttp::new().route("https://api.z.ai/", 503, "x");
        assert_eq!(
            t.with_env("ZAI_API_KEY", "k").run("zai", &http).error.as_deref(),
            Some("http")
        );
    }

    // privateCipher encrypted for secret "testsecret", api id "testid" (node vector).
    const HS_OK: &str = "{\"code\":0,\"data\":{\"privateCipher\":\"AAECAwQFBgcICQoLs5uiEmjdPcKqD8QXw7NUr5fv9pJGLAzxIdruHIvo3y3Rm5zaX5FiHw83+YcRl/bW7wuVRcDK2op6VIkwVcYg7zEjT3Lyw3Hj53wj1adaCBY=\"}}";
    const LIST_OK: &str = r#"{"code":200,"data":{"fiveHourResets":[
        {"recordId":"a","grantType":"DIRECT","expireTime":"2026-10-28 11:02:31","available":true},
        {"recordId":"b","grantType":"DIRECT","expireTime":"2026-10-28 11:02:31","available":true},
        {"recordId":"c","grantType":"DIRECT","expireTime":"2026-10-17 00:00:00","available":false}],
      "weekResets":[{"recordId":"d","grantType":"GLOBAL","expireTime":"2026-12-22 23:59:59","available":true}]}}"#;

    #[test]
    fn reset_enrichment_populates_five_hour_and_week() {
        let t = Fixture::new();
        let http = FakeHttp::new()
            .route("https://api.z.ai/api/monitor/", 200, BODY)
            .route("https://api.z.ai/api/paas/", 200, HS_OK)
            .route("https://api.z.ai/api/biz/", 200, LIST_OK);
        let e = t.with_env("ZAI_API_KEY", "testid.testsecret").run("zai", &http);
        assert!(e.ok, "{e:?}");
        let rc = e.reset_credits.clone().expect("créditos 5h");
        assert_eq!(rc.available, 2);
        assert!(
            rc.expires.is_empty(),
            "z.ai dates have no timezone and must not be guessed"
        );
        assert_eq!(
            rc.expiry_groups.as_ref().unwrap()[0].expires_at_raw,
            "2026-10-28 11:02:31"
        );
        let week = e.week_reset_credits.clone().expect("créditos semanais");
        assert_eq!(week.available, 1);
        assert!(
            week.expires.is_empty(),
            "z.ai dates have no timezone and must not be guessed"
        );
        assert_eq!(
            week.expiry_groups.as_ref().unwrap()[0].expires_at_raw,
            "2026-12-22 23:59:59"
        );
        assert!(http.header_seen("X-App-Id", "zcode"));
        t.assert_no_secret(&e, "testsecret");
    }

    fn enrich_http() -> FakeHttp {
        FakeHttp::new()
            .route("https://api.z.ai/api/monitor/", 200, BODY)
            .route("https://api.z.ai/api/paas/", 200, HS_OK)
            .route("https://api.z.ai/api/biz/", 200, LIST_OK)
    }

    #[test]
    fn enriquecimento_roda_no_maximo_a_cada_6h_e_mantem_os_creditos() {
        let t = Fixture::new();
        t.with_env("ZAI_API_KEY", "testid.testsecret");
        let http = enrich_http();
        t.run("zai", &http);
        let again = t.run("zai", &http);
        assert_eq!(
            http.calls_to("https://api.z.ai/api/monitor/"),
            2,
            "a quota em si atualiza"
        );
        assert_eq!(
            http.calls_to("https://api.z.ai/api/biz/"),
            1,
            "lista de créditos só na primeira"
        );
        assert_eq!(
            again.reset_credits.as_ref().map(|r| r.available),
            Some(2),
            "créditos mantidos"
        );

        let stamp = t.cache_dir().join("zai.resets-at");
        std::fs::write(&stamp, (t.now - 6 * 3600).to_string()).unwrap();
        t.run("zai", &http);
        assert_eq!(
            http.calls_to("https://api.z.ai/api/biz/"),
            2,
            "passadas 6h, enriquece de novo"
        );
        assert_eq!(std::fs::read_to_string(&stamp).unwrap(), t.now.to_string());
    }

    #[test]
    fn enriquecimento_pode_ser_desligado() {
        let t = Fixture::new();
        t.with_env("ZAI_API_KEY", "testid.testsecret")
            .with_env("COSTS_ZAI_RESETS", "0");
        let http = enrich_http();
        let e = t.run("zai", &http);
        assert!(e.ok);
        assert_eq!(
            http.calls_to("https://api.z.ai/api/paas/") + http.calls_to("https://api.z.ai/api/biz/"),
            0
        );
        assert!(e.reset_credits.is_none());
    }

    #[test]
    fn enrichment_failure_degrades_silently() {
        // handshake rejected (e.g. code 4011) → quota still ok, no resets
        let t = Fixture::new();
        let http = FakeHttp::new().route("https://api.z.ai/api/monitor/", 200, BODY).route(
            "https://api.z.ai/api/paas/",
            200,
            "{\"code\":4011,\"msg\":\"denied\"}",
        );
        let e = t.with_env("ZAI_API_KEY", "nokey.wrongsecret").run("zai", &http);
        assert!(e.ok, "{e:?}");
        assert!(e.reset_credits.is_none());
        assert!(e.week_reset_credits.is_none());

        // list garbage → same degrade
        let t2 = Fixture::new();
        let http2 = FakeHttp::new().route("https://api.z.ai/api/monitor/", 200, BODY).route(
            "https://api.z.ai/api/biz/",
            200,
            "not json",
        );
        let e2 = t2.with_env("ZAI_API_KEY", "x.y").run("zai", &http2);
        assert!(e2.ok, "{e2:?}");
        assert!(e2.reset_credits.is_none() && e2.week_reset_credits.is_none());
    }

    #[test]
    fn unsuccessful_body_is_parse_error() {
        let t = Fixture::new();
        let http = FakeHttp::new().route("https://api.z.ai/", 200, r#"{"code":401,"success":false}"#);
        assert_eq!(
            t.with_env("Z_AI_API_KEY", "k").run("zai", &http).error.as_deref(),
            Some("parse")
        );
    }
}
