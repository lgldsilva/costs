//! Parser do kimi (`<dir>/sessions/**/wire.jsonl`), equivalente a `ccusage kimi daily`.

use std::collections::HashSet;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::model::Row;
use crate::prices::Usage;
use crate::source::{Aggregator, Cost, LocalDate, PriceTable, SourceError, int, read_jsonl, walk_files};

const AGENT: &str = "kimi";
const WIRE_FILE: &str = "wire.jsonl";
const MODEL_PREFIX: &str = "kimi-code/";
/// Modelo dos registros do formato legado.
const LEGACY_MODEL: &str = "kimi-for-coding";

struct Entry {
    ts: DateTime<Utc>,
    model: String,
    /// [entrada, saída, cache leitura, cache escrita].
    tokens: [i64; 4],
}

fn tokens_of(u: &Value, keys: [&str; 4]) -> [i64; 4] {
    keys.map(|k| int(u.get(k)))
}

fn parse(line: &Value) -> Option<Entry> {
    let (ts_ms, model, tokens) = if line.get("type").and_then(Value::as_str) == Some("usage.record") {
        let scope = line.get("usageScope").and_then(Value::as_str);
        if !matches!(scope, None | Some("turn")) {
            return None;
        }
        let model = line.get("model")?.as_str()?;
        let model = model.strip_prefix(MODEL_PREFIX).unwrap_or(model);
        let usage = line.get("usage")?;
        let ms = line.get("time")?.as_f64()?;
        (
            ms as i64,
            model.to_string(),
            tokens_of(usage, ["inputOther", "output", "inputCacheRead", "inputCacheCreation"]),
        )
    } else {
        let msg = line.get("message")?;
        if msg.get("type")?.as_str()? != "StatusUpdate" {
            return None;
        }
        let usage = msg.get("payload")?.get("token_usage")?;
        let secs = line.get("timestamp")?.as_f64()?;
        (
            (secs * 1000.0) as i64,
            LEGACY_MODEL.to_string(),
            tokens_of(
                usage,
                ["input_other", "output", "input_cache_read", "input_cache_creation"],
            ),
        )
    };
    if model.is_empty() || tokens.iter().sum::<i64>() == 0 {
        return None;
    }
    Some(Entry {
        ts: DateTime::from_timestamp_millis(ts_ms)?,
        model,
        tokens,
    })
}

/// Linhas diárias por modelo. `root` é o diretório de dados (contém `sessions/`).
pub fn daily_rows(
    root: &Path,
    host: &str,
    local_date: LocalDate,
    prices: Option<&PriceTable>,
) -> Result<Vec<Row>, SourceError> {
    let mut agg = Aggregator::new(prices);
    for file in walk_files(&root.join("sessions"), &|n| n == WIRE_FILE, None)? {
        let mut seen = HashSet::new();
        for e in read_jsonl(&file)?.iter().filter_map(parse) {
            if !seen.insert((e.ts, e.model.clone(), e.tokens)) {
                continue;
            }
            let [input, output, cache_read, cache_write] = e.tokens;
            let usage = Usage {
                input: input as f64,
                output: output as f64,
                cache_read: cache_read as f64,
                cache_write: cache_write as f64,
            };
            agg.add(local_date(e.ts), &e.model, e.tokens, Cost::Priced(usage));
        }
    }
    Ok(agg.into_rows(host, AGENT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::testutil::{home, table, utc_date, validate};
    use std::fs;

    const T0: i64 = 1_767_261_600_000;

    fn rec(extra: &str, model: &str, usage: &str, time: i64) -> String {
        format!(r#"{{"type":"usage.record","model":"{model}","usage":{usage}{extra},"time":{time}}}"#)
    }

    fn run(files: &[(&str, Vec<String>)]) -> Vec<Row> {
        let dir = tempfile::tempdir().unwrap();
        for (name, lines) in files {
            let p = dir.path().join("sessions").join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, lines.join("\n")).unwrap();
        }
        daily_rows(dir.path(), "h", utc_date, Some(&table())).unwrap()
    }

    #[test]
    fn dedupe_por_arquivo_e_prefixo_do_modelo() {
        let u = r#"{"inputOther":10,"output":5,"inputCacheRead":2,"inputCacheCreation":1}"#;
        let l = rec("", "kimi-code/m", u, T0);
        let rows = run(&[
            ("a/wire.jsonl", vec![l.clone(), l.clone()]),
            ("b/c/wire.jsonl", vec![l]),
        ]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model_raw, "m");
        assert_eq!(
            (rows[0].input, rows[0].output, rows[0].cache_read, rows[0].cache_write),
            (20, 10, 4, 2)
        );
        assert!(rows[0].cost_usd.is_some());
    }

    #[test]
    fn escopo_so_turn_ou_ausente_e_ignora_ruim() {
        let u = r#"{"inputOther":1}"#;
        let lines = vec![
            rec(r#","usageScope":"turn""#, "m", u, T0),
            rec(r#","usageScope":"session""#, "m", u, T0 + 1),
            rec(r#","usageScope":"step""#, "m", u, T0 + 2),
            rec("", "m", u, T0 + 3),
            rec("", "m", r#"{}"#, T0 + 4),
            "{ruim".to_string(),
            r#"{"type":"other"}"#.to_string(),
        ];
        let rows = run(&[("wire.jsonl", lines), ("x.jsonl", vec![rec("", "m", u, T0)])]);
        assert_eq!(rows[0].input, 2);
    }

    #[test]
    fn formato_legado_nao_dedupa_por_message_id() {
        let l = |ts: f64| {
            format!(
                r#"{{"timestamp":{ts},"message":{{"type":"StatusUpdate","payload":{{"message_id":"x","token_usage":{{"input_other":3,"output":1,"input_cache_read":0,"input_cache_creation":0}}}}}}}}"#
            )
        };
        let rows = run(&[("h/s/wire.jsonl", vec![l(1_767_261_600.0), l(1_767_261_601.5)])]);
        assert_eq!(
            (rows[0].model.as_str(), rows[0].input, rows[0].output),
            ("kimi-for-coding", 6, 2)
        );
    }

    #[test]
    fn sem_diretorio_sessions() {
        let dir = tempfile::tempdir().unwrap();
        assert!(daily_rows(dir.path(), "h", utc_date, None).unwrap().is_empty());
    }

    #[test]
    fn valida_contra_ccusage() {
        let root = std::env::var_os("KIMI_DATA_DIR").map_or_else(|| home().join(".kimi-code"), Into::into);
        let rows = daily_rows(&root, "h", crate::source::local_date, None).unwrap();
        validate(AGENT, &rows);
    }
}
