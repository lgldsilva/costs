//! OpenRouter: saldo via `/credits`, com fallback para `/key`.

use serde_json::Value;

use super::creds::{Ctx, Failure, bearer};
use super::entry::{Balance, Partial};
use super::norm::num;

const CREDITS_URL: &str = "https://openrouter.ai/api/v1/credits";
const KEY_URL: &str = "https://openrouter.ai/api/v1/key";
const MONEY_SCALE: f64 = 1_000_000.0;

fn money6(x: f64) -> f64 {
    (x * MONEY_SCALE).round() / MONEY_SCALE
}

/// Port de `openrouter.jq`: `/credits` (total - uso) tem prioridade; `/key` só com limite.
pub fn parse(v: &Value) -> Partial {
    let d = v.get("data").unwrap_or(&Value::Null);
    let n = |k: &str| d.get(k).and_then(num);
    let amount = match (n("total_credits"), n("total_usage"), n("limit"), n("limit_remaining")) {
        (Some(tc), Some(tu), _, _) => Some(money6(tc - tu)),
        (_, _, Some(_), Some(lr)) => Some(money6(lr)),
        _ => None,
    };
    let balance = amount.map(|amount| Balance {
        amount,
        currency: "USD".into(),
    });
    Partial {
        plan: Some(None),
        windows: Some(vec![]),
        balance: Some(balance),
        reset_credits: None,
        week_reset_credits: None,
        data_at: None,
    }
}

pub fn fetch(ctx: &Ctx) -> Result<Partial, Failure> {
    let key = ctx
        .api_key("openrouter", &["OPENROUTER_API_KEY"])
        .ok_or(Failure::NoKey)?;
    let h = bearer(&key, &[]);
    let body = ctx.get(CREDITS_URL, &h).or_else(|_| ctx.get(KEY_URL, &h))?;
    Ok(parse(&serde_json::from_str(&body).map_err(|_| Failure::Parse)?))
}

#[cfg(test)]
mod tests {
    use crate::quota::testutil::*;

    #[test]
    fn success_parse_credits() {
        let t = Fixture::new();
        let http = FakeHttp::new().route(
            "https://openrouter.ai/api/v1/credits",
            200,
            r#"{"data":{"total_credits":10.5,"total_usage":3.1234567}}"#,
        );
        let e = t.with_env("OPENROUTER_API_KEY", "sekret-or").run("openrouter", &http);
        assert!(e.ok, "{e:?}");
        let b = e.balance.clone().unwrap();
        assert_eq!((b.amount, b.currency.as_str()), (7.376543, "USD"));
        t.assert_no_secret(&e, "sekret-or");
    }

    #[test]
    fn falls_back_to_key_endpoint() {
        let t = Fixture::new();
        let http = FakeHttp::new()
            .route("https://openrouter.ai/api/v1/credits", 403, "no")
            .route(
                "https://openrouter.ai/api/v1/key",
                200,
                r#"{"data":{"limit":20,"limit_remaining":4.5}}"#,
            );
        let e = t.with_env("OPENROUTER_API_KEY", "k").run("openrouter", &http);
        assert_eq!(e.balance.map(|b| b.amount), Some(4.5));
    }

    #[test]
    fn missing_key_is_nokey() {
        let t = Fixture::new();
        assert_eq!(t.run("openrouter", &FakeHttp::new()).error.as_deref(), Some("nokey"));
    }

    #[test]
    fn http_failure() {
        let t = Fixture::new();
        let http = FakeHttp::new().route("https://openrouter.ai/", 500, "x");
        assert_eq!(
            t.with_env("OPENROUTER_API_KEY", "k")
                .run("openrouter", &http)
                .error
                .as_deref(),
            Some("http")
        );
    }
}
