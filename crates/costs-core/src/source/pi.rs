//! Parser do pi (`~/.pi/agent/sessions/**/*.jsonl`), equivalente a `ccusage pi daily`.

use std::collections::HashSet;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::model::Row;
use crate::prices::Usage;
use crate::source::{Aggregator, Cost, LocalDate, PriceTable, SourceError, int, read_jsonl, walk_files};

const AGENT: &str = "pi";
const SESSIONS_DIR: &str = "sessions";
const SKIPPED_DIR: &str = "subagent-artifacts";
/// Prefixo literal do nome do modelo no ccusage.
const MODEL_PREFIX: &str = "[pi] ";

/// Chave de dedupe (por arquivo): timestamp, modelo, tokens, total e custo.
type Key = (String, String, [i64; 5], Option<u64>);

struct Entry {
    ts: DateTime<Utc>,
    model: String,
    tokens: [i64; 4],
    cost: Option<f64>,
    key: Key,
}

fn parse(line: &Value) -> Option<Entry> {
    if line.get("type")?.as_str()? != "message" {
        return None;
    }
    let stamp = line.get("timestamp")?.as_str()?;
    let ts = DateTime::parse_from_rfc3339(stamp).ok()?.with_timezone(&Utc);
    let msg = line.get("message")?;
    if msg.get("role")?.as_str()? != "assistant" {
        return None;
    }
    let model = msg.get("model")?.as_str().filter(|m| !m.is_empty())?;
    let usage = msg.get("usage")?;
    let f = |k| int(usage.get(k));
    let (input, mut output, cr, cw, total) = (
        f("input"),
        f("output"),
        f("cacheRead"),
        f("cacheWrite"),
        f("totalTokens"),
    );
    if input + output + cr + cw + total == 0 {
        return None;
    }
    let parts = input + cr + cw;
    if output == 0 && total > parts {
        output = total - parts;
    }
    let cost = usage.get("cost").and_then(|c| c.get("total")).and_then(Value::as_f64);
    let key = (
        stamp.to_string(),
        model.to_string(),
        [input, usage_out(usage), cr, cw, total],
        cost.map(f64::to_bits),
    );
    Some(Entry {
        ts,
        model: model.to_string(),
        tokens: [input, output, cr, cw],
        cost,
        key,
    })
}

/// `output` bruto (antes da derivação), que é o que entra na chave de dedupe.
fn usage_out(usage: &Value) -> i64 {
    int(usage.get("output"))
}

/// Linhas diárias por modelo. `root` é o diretório do agente (usa `<root>/sessions` se existir).
pub fn daily_rows(
    root: &Path,
    host: &str,
    local_date: LocalDate,
    prices: Option<&PriceTable>,
) -> Result<Vec<Row>, SourceError> {
    let sessions = root.join(SESSIONS_DIR);
    let base = if sessions.is_dir() { sessions.as_path() } else { root };
    let mut agg = Aggregator::new(prices);
    for file in walk_files(base, &|n| n.ends_with(".jsonl"), Some(SKIPPED_DIR))? {
        let mut seen: HashSet<Key> = HashSet::new();
        for e in read_jsonl(&file)?.iter().filter_map(parse) {
            if !seen.insert(e.key) {
                continue;
            }
            let [input, output, cache_read, cache_write] = e.tokens;
            // Como no opencode: só custo positivo vale como informado. Provedores de plano
            // (qwen-token-plan) gravam 0 com tokens, o que escondia o gasto equivalente em API.
            let cost = e.cost.filter(|c| *c > 0.0).map_or(
                Cost::Priced(Usage {
                    input: input as f64,
                    output: output as f64,
                    cache_read: cache_read as f64,
                    cache_write: cache_write as f64,
                }),
                Cost::Stored,
            );
            agg.add(local_date(e.ts), &format!("{MODEL_PREFIX}{}", e.model), e.tokens, cost);
        }
    }
    Ok(agg.into_rows(host, AGENT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::testutil::{home, table, utc_date, validate};
    use std::fs;

    fn line(ts: &str, model: &str, usage: &str) -> String {
        format!(
            r#"{{"type":"message","timestamp":"{ts}","message":{{"role":"assistant","model":"{model}","usage":{usage}}}}}"#
        )
    }

    fn run(files: &[(&str, Vec<String>)], prices: Option<&PriceTable>) -> Vec<Row> {
        let dir = tempfile::tempdir().unwrap();
        for (name, lines) in files {
            let p = dir.path().join("sessions").join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, lines.join("\n")).unwrap();
        }
        daily_rows(dir.path(), "h", utc_date, prices).unwrap()
    }

    #[test]
    fn dedupe_por_arquivo_ignora_id_mas_nao_entre_arquivos() {
        let u = r#"{"input":10,"output":5,"cacheRead":2,"cacheWrite":1,"totalTokens":18}"#;
        let l = line("2026-01-01T10:00:00Z", "m", u);
        let rows = run(&[("a.jsonl", vec![l.clone(), l.clone()]), ("b.jsonl", vec![l])], None);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].input, rows[0].output, rows[0].cache_read, rows[0].cache_write),
            (20, 10, 4, 2)
        );
    }

    #[test]
    fn deriva_saida_so_quando_zero_e_nome_canonico() {
        let rows = run(
            &[(
                "a.jsonl",
                vec![line(
                    "2026-01-01T10:00:00Z",
                    "Foo/M",
                    r#"{"input":10,"output":0,"totalTokens":50}"#,
                )],
            )],
            None,
        );
        assert_eq!(rows[0].output, 40);
        assert_eq!(rows[0].model_raw, "[pi] Foo/M");
        assert_eq!(rows[0].model, "m");
        let rows = run(
            &[(
                "a.jsonl",
                vec![line(
                    "2026-01-01T10:00:00Z",
                    "m",
                    r#"{"input":10,"output":3,"totalTokens":50}"#,
                )],
            )],
            None,
        );
        assert_eq!(rows[0].output, 3);
    }

    #[test]
    fn custo_positivo_vale_e_zero_ou_ausente_cai_na_tabela() {
        let t = table();
        let zero = line("2026-01-01T10:00:00Z", "m", r#"{"input":1000000,"cost":{"total":0.0}}"#);
        let text = line(
            "2026-01-02T10:00:00Z",
            "m",
            r#"{"input":1000000,"cost":{"total":"2.5"}}"#,
        );
        let stored = line("2026-01-03T10:00:00Z", "m", r#"{"input":1000000,"cost":{"total":3.0}}"#);
        let rows = run(&[("a.jsonl", vec![zero, text, stored])], Some(&t));
        let costs: Vec<_> = rows
            .iter()
            .map(|r| (r.cost_usd.unwrap(), r.cost_basis.as_str()))
            .collect();
        assert_eq!(
            costs,
            [(1.0, "estimated"), (1.0, "estimated"), (3.0, "reported")],
            "0 com tokens não é grátis"
        );
    }

    #[test]
    fn ignora_malformadas_papeis_zerados_sem_modelo_e_subagentes() {
        let ok = line("2026-01-01T10:00:00Z", "m", r#"{"input":1}"#);
        let user = ok.replace("assistant", "user");
        let zero = line("2026-01-01T11:00:00Z", "m", r#"{"input":0}"#);
        let nomodel = r#"{"type":"message","timestamp":"2026-01-01T12:00:00Z","message":{"role":"assistant","usage":{"input":9}}}"#.to_string();
        let badts = ok.replace("2026-01-01T10:00:00Z", "ontem");
        let other = r#"{"type":"session"}"#.to_string();
        let rows = run(
            &[
                (
                    "a.jsonl",
                    vec![
                        "{nao json".into(),
                        ok.clone(),
                        user,
                        zero,
                        nomodel,
                        badts,
                        other,
                        String::new(),
                    ],
                ),
                ("subagent-artifacts/x.jsonl", vec![ok]),
                ("c.json", vec!["{}".into()]),
            ],
            None,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input, 1);
    }

    #[test]
    fn raiz_sem_pasta_sessions_e_raiz_ausente() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.jsonl"),
            line("2026-01-01T10:00:00Z", "m", r#"{"input":4}"#),
        )
        .unwrap();
        assert_eq!(daily_rows(dir.path(), "h", utc_date, None).unwrap().len(), 1);
        assert!(
            daily_rows(&dir.path().join("nope"), "h", utc_date, None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn valida_contra_ccusage() {
        let root = std::env::var_os("PI_AGENT_DIR").map_or_else(|| home().join(".pi/agent/sessions"), Into::into);
        let rows = daily_rows(&root, "h", crate::source::local_date, None).unwrap();
        validate(AGENT, &rows);
    }
}
