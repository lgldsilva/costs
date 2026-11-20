//! OpenCode Go: `GET opencode.ai/zen/go/v1/usage` (rolling 5h, semanal, mensal).

use serde_json::Value;

use super::creds::{Ctx, Failure, bearer};
use super::entry::{Partial, Window};
use super::norm::{num, to_epoch, window};

const URL: &str = "https://opencode.ai/zen/go/v1/usage";
// Cloudflare rejects the default HTTP client identity with error 1010.
const HEADERS: [(&str, &str); 2] = [
    ("User-Agent", concat!("costs/", env!("CARGO_PKG_VERSION"))),
    ("Accept", "application/json"),
];

fn win(usage: &Value, label: &str, key: &str) -> Option<Window> {
    let x = usage.get(key).filter(|x| !x.is_null())?;
    let pct = num(x.get("percent")?)?;
    Some(window(label, Some(100.0 - pct), x.get("resetsAt").and_then(to_epoch)))
}

/// Port de `opencode-go.jq`: sem `usage` é erro; janela ausente é omitida.
pub fn parse(v: &Value) -> Option<Partial> {
    let usage = v.get("usage").filter(|u| u.is_object())?;
    let windows = [("5h", "rolling"), ("sem", "weekly"), ("mês", "monthly")]
        .iter()
        .filter_map(|(l, k)| win(usage, l, k))
        .collect();
    Some(Partial {
        windows: Some(windows),
        ..Partial::default()
    })
}

pub fn fetch(ctx: &Ctx) -> Result<Partial, Failure> {
    let keys = ctx.api_keys("opencode-go", &["OPENCODE_GO_API_KEY"]);
    if keys.is_empty() {
        return Err(Failure::NoKey);
    }
    for key in keys {
        match ctx.get(URL, &bearer(&key, &HEADERS)) {
            Ok(body) => return parse(&serde_json::from_str(&body).map_err(|_| Failure::Parse)?).ok_or(Failure::Parse),
            // A running desktop can retain an expired environment key after the
            // user rotates the credential in Pi/OpenCode. Never rewrite their files.
            Err(Failure::Auth) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(Failure::Auth)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use crate::quota::env::{HttpClient, HttpError};
    use crate::quota::testutil::*;

    const BODY: &str = r#"{"usage":{"rolling":{"status":"ok","percent":5,"resetsAt":"2026-10-10T04:23:21.000Z"},
        "weekly":{"percent":"20.5","resetsAt":"2026-10-12T00:00:00.000Z"},"monthly":null}}"#;

    #[test]
    fn success_parse() {
        let t = Fixture::new();
        t.write_home(
            ".local/share/opencode/auth.json",
            r#"{"opencode-go":{"key":"sekret-go"}}"#,
        );
        let http = FakeHttp::new().route("https://opencode.ai/zen/go/", 200, BODY);
        let e = t.run("opencode-go", &http);
        assert!(e.ok, "{e:?}");
        let got: Vec<_> = e.windows.iter().map(|w| (w.label.as_str(), w.remaining_pct)).collect();
        assert_eq!(got, [("5h", Some(95)), ("sem", Some(80))]);
        assert_eq!(e.windows[0].resets_at, Some(1_791_606_201));
        assert_eq!(e.windows[1].window_s, Some(604_800));
        t.assert_no_secret(&e, "sekret-go");
    }

    #[test]
    fn missing_key_is_nokey() {
        let t = Fixture::new();
        let e = t
            .with_env("OPENCODE_API_KEY", "zen-key-not-go")
            .run("opencode-go", &FakeHttp::new());
        assert_eq!(e.error.as_deref(), Some("nokey"));
    }

    #[test]
    fn http_failure() {
        let t = Fixture::new();
        let http = FakeHttp::new().route("https://opencode.ai/", 500, "no");
        let e = t.with_env("OPENCODE_GO_API_KEY", "k").run("opencode-go", &http);
        assert_eq!(e.error.as_deref(), Some("http"));
    }

    #[test]
    fn rejected_key_is_auth() {
        let t = Fixture::new();
        let http = FakeHttp::new().route("https://opencode.ai/", 403, "no");
        let e = t.with_env("OPENCODE_GO_API_KEY", "k").run("opencode-go", &http);
        assert_eq!(e.error.as_deref(), Some("auth"));
    }

    #[test]
    fn cloudflare_client_block_is_not_an_invalid_api_key() {
        let t = Fixture::new();
        let http = FakeHttp::new().route("https://opencode.ai/", 403, "error code: 1010\n");
        let entry = t.with_env("OPENCODE_GO_API_KEY", "key").run("opencode-go", &http);
        assert_eq!(entry.error.as_deref(), Some("http"));
    }

    #[test]
    fn identifies_the_usage_client_to_avoid_cloudflare_1010() {
        let t = Fixture::new();
        let http = FakeHttp::new().route("https://opencode.ai/", 200, BODY);
        assert!(t.with_env("OPENCODE_GO_API_KEY", "k").run("opencode-go", &http).ok);
        assert!(http.header_seen("User-Agent", concat!("costs/", env!("CARGO_PKG_VERSION"))));
        assert!(http.header_seen("Accept", "application/json"));
    }

    struct KeyServer {
        valid: &'static str,
        rejected_status: u16,
        seen: RefCell<Vec<String>>,
    }

    impl HttpClient for KeyServer {
        fn get(&self, url: &str, headers: &[(String, String)], _timeout: u64) -> Result<(u16, String), HttpError> {
            assert_eq!(url, super::URL);
            let auth = headers.iter().find(|(k, _)| k == "Authorization").unwrap().1.clone();
            let valid = auth == format!("Bearer {}", self.valid);
            self.seen.borrow_mut().push(auth);
            Ok(if valid {
                (200, BODY.into())
            } else {
                (self.rejected_status, "rejected".into())
            })
        }
    }

    fn run_with_server(t: &Fixture, valid: &'static str, rejected_status: u16) -> (crate::quota::Entry, Vec<String>) {
        let http = KeyServer {
            valid,
            rejected_status,
            seen: RefCell::default(),
        };
        let entry = crate::quota::fetch("opencode-go", t.now, &http, t, true, &t.cache_dir());
        (entry, http.seen.into_inner())
    }

    #[test]
    fn expired_environment_key_falls_back_to_valid_pi_key() {
        let t = Fixture::new();
        t.with_env("OPENCODE_GO_API_KEY", "expired");
        t.write_home(".pi/agent/auth.json", r#"{"opencode-go":{"key":"fresh-pi"}}"#);
        let (entry, seen) = run_with_server(&t, "fresh-pi", 401);
        assert!(
            entry.ok,
            "a rotated Pi credential must work without restarting the desktop"
        );
        assert_eq!(seen, ["Bearer expired", "Bearer fresh-pi"]);
        t.assert_no_secret(&entry, "fresh-pi");
        t.assert_no_secret(&entry, "expired");
    }

    #[test]
    fn valid_environment_key_keeps_priority() {
        let t = Fixture::new();
        t.with_env("OPENCODE_GO_API_KEY", "primary");
        t.write_home(".pi/agent/auth.json", r#"{"opencode-go":{"key":"other-account"}}"#);
        let (entry, seen) = run_with_server(&t, "primary", 401);
        assert!(entry.ok);
        assert_eq!(seen, ["Bearer primary"]);
    }

    #[test]
    fn selected_harness_then_environment_then_other_harness() {
        let t = Fixture::new();
        t.with_env("AI_USAGE_HARNESS", "pi");
        t.with_env("OPENCODE_GO_API_KEY", "expired-env");
        t.write_home(".pi/agent/auth.json", r#"{"opencode-go":{"key":"expired-pi"}}"#);
        t.write_home(
            ".local/share/opencode/auth.json",
            r#"{"opencode-go":{"key":"fresh-oc"}}"#,
        );
        let (entry, seen) = run_with_server(&t, "fresh-oc", 403);
        assert!(entry.ok);
        assert_eq!(seen, ["Bearer expired-pi", "Bearer expired-env", "Bearer fresh-oc"]);
    }

    #[test]
    fn duplicate_rejected_keys_are_tried_once() {
        let t = Fixture::new();
        t.with_env("OPENCODE_GO_API_KEY", "same");
        t.write_home(".pi/agent/auth.json", r#"{"opencode-go":{"key":"same"}}"#);
        t.write_home(".local/share/opencode/auth.json", r#"{"opencode-go":{"key":"same"}}"#);
        let (entry, seen) = run_with_server(&t, "missing", 401);
        assert_eq!(entry.error.as_deref(), Some("auth"));
        assert_eq!(seen, ["Bearer same"]);
    }

    #[test]
    fn server_failure_does_not_switch_accounts() {
        let t = Fixture::new();
        t.with_env("OPENCODE_GO_API_KEY", "primary");
        t.write_home(".pi/agent/auth.json", r#"{"opencode-go":{"key":"fresh-pi"}}"#);
        let (entry, seen) = run_with_server(&t, "fresh-pi", 500);
        assert_eq!(entry.error.as_deref(), Some("http"));
        assert_eq!(seen, ["Bearer primary"]);
    }
}
