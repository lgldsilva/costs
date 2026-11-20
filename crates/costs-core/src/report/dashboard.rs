//! Offline report payload and script-safe document assembly.
use super::query::{Filters, canonical_provider};
use crate::config::Config;
use rusqlite::{Connection, params_from_iter};
use serde_json::{Value, json};

pub fn html(
    con: &Connection,
    month: &str,
    filters: &Filters,
    cfg: &Config,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut export_filters = filters.clone();
    export_filters.range = None;
    let (predicate, values) = export_filters.predicate(None);
    let mut stmt = con.prepare(&format!("SELECT date,host,agent,provider,model,input,output,cache_read,cache_write,cost_usd FROM usage WHERE {predicate} ORDER BY date,host,agent,provider,model"))?;
    let rows = stmt.query_map(params_from_iter(values), |r| {
        Ok(json!({"date":r.get::<_,String>(0)?,"host":r.get::<_,String>(1)?,"agent":r.get::<_,String>(2)?,"provider":r.get::<_,String>(3)?,"model":r.get::<_,String>(4)?,"input":r.get::<_,Option<i64>>(5)?.unwrap_or(0),"output":r.get::<_,Option<i64>>(6)?.unwrap_or(0),"read":r.get::<_,Option<i64>>(7)?.unwrap_or(0),"write":r.get::<_,Option<i64>>(8)?.unwrap_or(0),"cost":r.get::<_,Option<f64>>(9)?}))
    })?.collect::<Result<Vec<Value>,_>>()?;
    let mut stmt = con.prepare("SELECT host,agent,ts,status,COALESCE(message,''),(SELECT MAX(ts) FROM runs ok WHERE ok.host=r.host AND ok.agent=r.agent AND ok.status='ok') FROM runs r WHERE rowid=(SELECT rowid FROM runs x WHERE x.host=r.host AND x.agent=r.agent ORDER BY ts DESC,rowid DESC LIMIT 1) ORDER BY host,agent")?;
    let runs = stmt.query_map([], |r| Ok(json!({"host":r.get::<_,String>(0)?,"agent":r.get::<_,String>(1)?,"ts":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,"message":r.get::<_,String>(4)?,"lastSuccess":r.get::<_,Option<String>>(5)?})))?.collect::<Result<Vec<Value>,_>>()?;
    let plans: Vec<Value> = cfg.subscriptions.iter().map(|s| {
        let providers: Vec<String> = s.provider.as_ref().map(|p| cfg.quota.cost_map.get(p).cloned().unwrap_or_else(|| vec![canonical_provider(p).to_owned()])).unwrap_or_default();
        json!({"name":s.name,"fee":s.monthly_usd,"providers":providers,"inconclusive":s.provider.as_deref()==Some("opencode-go") || s.provider.is_none()})
    }).collect();
    let expected: Vec<Value> = cfg
        .hosts
        .iter()
        .filter(|h| h.enabled)
        .flat_map(|h| h.agents.iter().map(move |a| json!({"host":h.name,"agent":a})))
        .collect();
    let hard_scope = [
        ("Provedor", filters.provider.as_deref()),
        ("Harness", filters.agent.as_deref()),
        ("Máquina", filters.host.as_deref()),
        ("Modelo", filters.model.as_deref()),
    ]
    .into_iter()
    .filter_map(|(label, value)| value.map(|value| format!("{label}: {value}")))
    .collect::<Vec<_>>()
    .join(" · ");
    let payload = json!({"month":month,"generatedAt":chrono::Utc::now().to_rfc3339(),"today":chrono::Local::now().date_naive().to_string(),"range":filters.range,"hardScope":hard_scope,"rows":rows,"runs":runs,"plans":plans,"expected":expected});
    Ok(include_str!("dashboard.html").replace("/*PAYLOAD*/null", &script_json(&payload)))
}

fn script_json(value: &Value) -> String {
    value
        .to_string()
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hostile_names_cannot_close_script() {
        let value = json!({"model":"</script><img src=x onerror=alert(1)>&\u{2028}"});
        let encoded = script_json(&value);
        assert!(!encoded.contains('<'));
        assert_eq!(serde_json::from_str::<Value>(&encoded).unwrap(), value);
    }
    #[test]
    fn freshness_keeps_last_success_and_every_expected_agent() {
        use chrono::{TimeZone, Utc};
        let directory = tempfile::tempdir().unwrap();
        let success = Utc.with_ymd_and_hms(2024, 1, 15, 8, 0, 0).unwrap();
        let attempt = Utc.with_ymd_and_hms(2024, 1, 15, 12, 0, 0).unwrap();
        let store = crate::store::Store::open(&directory.path().join("test.sqlite"), "", success).unwrap();
        store.log_run(success, "mac", "codex", "ok", None, 1).unwrap();
        store
            .log_run(attempt, "mac", "codex", "error", Some("offline"), 0)
            .unwrap();
        let config = crate::config::parse_config("agents = ['codex', 'claude']\n[[hosts]]\nname = 'mac'\n").unwrap();
        let document = html(store.connection(), "2024-01", &Filters::default(), &config).unwrap();
        let payload = document
            .split("const data=")
            .nth(1)
            .unwrap()
            .split(";\n")
            .next()
            .unwrap();
        let payload: Value = serde_json::from_str(payload).unwrap();
        assert_eq!(payload["runs"][0]["status"], "error");
        assert!(
            payload["runs"][0]["lastSuccess"]
                .as_str()
                .unwrap()
                .starts_with("2024-01-15T08:00:00")
        );
        assert!(
            payload["runs"][0]["ts"]
                .as_str()
                .unwrap()
                .starts_with("2024-01-15T12:00:00")
        );
        assert_eq!(payload["expected"].as_array().unwrap().len(), 2);
        assert!(chrono::DateTime::parse_from_rfc3339(payload["generatedAt"].as_str().unwrap()).is_ok());
    }

    proptest::proptest! {
        #[test]
        fn any_name_roundtrips(name in ".*") {
            let value = json!([name]);
            let encoded = script_json(&value);
            proptest::prop_assert!(!encoded.contains('<'));
            proptest::prop_assert_eq!(serde_json::from_str::<Value>(&encoded).unwrap(), value);
        }
    }
}
