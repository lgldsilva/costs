//! DeepSeek: saldo via `GET api.deepseek.com/user/balance`.

use serde_json::Value;

use super::creds::{Ctx, Failure, bearer};
use super::entry::{Balance, Partial};
use super::norm::num;

const URL: &str = "https://api.deepseek.com/user/balance";

/// Port de `deepseek.jq`: prefere a entrada USD, senão a primeira.
pub fn parse(v: &Value) -> Partial {
    let infos: &[Value] = v
        .get("balance_infos")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let pick = infos
        .iter()
        .find(|b| b.get("currency").and_then(Value::as_str) == Some("USD"))
        .or(infos.first());
    let balance = pick.and_then(|b| {
        let amount = num(b.get("total_balance")?)?;
        let currency = b.get("currency").and_then(Value::as_str).unwrap_or_default();
        Some(Balance {
            amount,
            currency: currency.to_string(),
        })
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
    let key = ctx.api_key("deepseek", &["DEEPSEEK_API_KEY"]).ok_or(Failure::NoKey)?;
    let body = ctx.get(URL, &bearer(&key, &[]))?;
    Ok(parse(&serde_json::from_str(&body).map_err(|_| Failure::Parse)?))
}

#[cfg(test)]
mod tests {
    use crate::quota::entry::Balance;
    use crate::quota::testutil::*;

    const BODY: &str = r#"{"is_available":true,"balance_infos":[
        {"currency":"CNY","total_balance":"70.00"},{"currency":"USD","total_balance":"12.34"}]}"#;

    #[test]
    fn success_parse_prefers_usd() {
        let t = Fixture::new();
        let http = FakeHttp::new().route("https://api.deepseek.com/", 200, BODY);
        let e = t.with_env("DEEPSEEK_API_KEY", "sekret-ds").run("deepseek", &http);
        assert!(e.ok, "{e:?}");
        assert_eq!(
            e.balance,
            Some(Balance {
                amount: 12.34,
                currency: "USD".into()
            })
        );
        assert!(e.windows.is_empty());
        t.assert_no_secret(&e, "sekret-ds");
    }

    #[test]
    fn missing_key_is_nokey() {
        let t = Fixture::new();
        assert_eq!(t.run("deepseek", &FakeHttp::new()).error.as_deref(), Some("nokey"));
    }

    #[test]
    fn http_failure() {
        let t = Fixture::new();
        let http = FakeHttp::new().route("https://api.deepseek.com/", 500, "x");
        assert_eq!(
            t.with_env("DEEPSEEK_API_KEY", "k")
                .run("deepseek", &http)
                .error
                .as_deref(),
            Some("http")
        );
    }
}
