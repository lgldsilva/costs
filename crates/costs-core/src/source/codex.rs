//! Parser nativo dos logs do Codex (`sessions/**/*.jsonl`), no formato do ccusage.
//!
//! Agrega como a visão `ccusage codex session`: todo o uso de uma sessão vai
//! para a data do diretório do arquivo (`sessions/AAAA/MM/DD/`), não para a data
//! de cada evento.
//!
//! Não replica: faixa de contexto longo (>272k), multiplicadores fast/flex e
//! deduplicação de réplicas em forks/subagentes. Custo vem da tabela OpenRouter.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use serde_json::Value;

use super::{Aggregator, Cost, LocalDate, PriceTable, SourceError, int, io_err, walk_files};
use crate::model::Row;
use crate::prices::Usage;

const AGENT: &str = "codex";
const PROVIDER: &str = "openai";
const FALLBACK_MODEL: &str = "gpt-5";
/// Intervalo máximo entre eventos consecutivos de uma rajada de réplica.
const REPLAY_GAP: TimeDelta = TimeDelta::seconds(1);
const SUBDIRS: [&str; 2] = ["sessions", "archived_sessions"];
const EXTENSION: &str = ".jsonl";
const COMPRESSED_EXTENSION: &str = ".jsonl.zst";

/// Contadores de um evento (campos ausentes valem 0).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Counts {
    input: i64,
    cached: i64,
    cache_write: i64,
    output: i64,
    reasoning: i64,
    total: i64,
}

impl Counts {
    fn parse(v: &Value) -> Self {
        let n = |k: &str| int(v.get(k));
        Self {
            input: n("input_tokens"),
            cached: n("cached_input_tokens"),
            cache_write: n("cache_write_input_tokens"),
            output: n("output_tokens"),
            reasoning: n("reasoning_output_tokens"),
            total: n("total_tokens"),
        }
    }

    fn regressed_from(&self, prev: &Self) -> bool {
        self.input < prev.input
            || self.cached < prev.cached
            || self.cache_write < prev.cache_write
            || self.output < prev.output
    }

    fn minus(&self, prev: &Self) -> Self {
        Self {
            input: self.input - prev.input,
            cached: self.cached - prev.cached,
            cache_write: self.cache_write - prev.cache_write,
            output: self.output - prev.output,
            reasoning: self.reasoning - prev.reasoning,
            total: self.total - prev.total,
        }
    }
}

/// Estado de um arquivo de sessão enquanto é percorrido.
struct FileState<'a, 'p> {
    local_date: LocalDate,
    /// Data do diretório da sessão; quando ausente vale a data local do evento.
    session_date: Option<NaiveDate>,
    agg: &'a mut Aggregator<'p>,
    model: Option<String>,
    prev_total: Option<Counts>,
    last_record: Option<(String, Counts, DateTime<Utc>)>,
    /// Rajada inicial de um fork/subagente (réplica do pai), ainda não decidida.
    replay: Option<Replay>,
}

/// Eventos iniciais encadeados (intervalo < `REPLAY_GAP`) de um fork/subagente.
#[derive(Default)]
struct Replay {
    pending: Vec<(Option<Counts>, DateTime<Utc>)>,
}

impl FileState<'_, '_> {
    fn line(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let interesting = [
            "token_count",
            "turn_context",
            "compacted",
            "token_usage_record",
            "session_meta",
        ]
        .iter()
        .any(|m| text.contains(m));
        if !interesting {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let payload = v.get("payload");
        match v.get("type").and_then(Value::as_str) {
            Some("session_meta") => self.session_meta(payload),
            Some("turn_context") => self.turn_context(payload),
            Some("event_msg") => self.event_msg(&v, payload),
            Some("token_usage_record") => self.record(&v, payload),
            Some("compacted") => self.compacted(&v, payload),
            _ => {}
        }
    }

    /// Fork ou subagente: o início do arquivo pode replicar eventos do pai.
    fn session_meta(&mut self, payload: Option<&Value>) {
        let Some(p) = payload else { return };
        let forked = p.get("forked_from_id").is_some_and(|v| !v.is_null());
        let spawned = p
            .pointer("/source/subagent/thread_spawn/parent_thread_id")
            .is_some_and(|v| !v.is_null());
        if (forked || spawned) && self.replay.is_none() && self.prev_total.is_none() {
            self.replay = Some(Replay::default());
        }
    }

    fn turn_context(&mut self, payload: Option<&Value>) {
        if let Some(m) = payload.and_then(|p| p.get("model")).and_then(Value::as_str) {
            self.model = Some(m.to_string());
        }
    }

    fn record(&mut self, v: &Value, payload: Option<&Value>) {
        let Some(p) = payload else { return };
        let (Some(id), Some(usage), Some(at)) = (
            p.get("response_id").and_then(Value::as_str),
            p.get("usage"),
            timestamp(v),
        ) else {
            return;
        };
        self.last_record = Some((id.to_string(), Counts::parse(usage), at));
    }

    fn compacted(&mut self, v: &Value, payload: Option<&Value>) {
        let id = payload
            .and_then(|p| p.get("compaction_response_id"))
            .and_then(Value::as_str);
        if self.last_record.as_ref().map(|r| r.0.as_str()) != id {
            return;
        }
        if let Some((_, counts, at)) = self.last_record.take() {
            self.add(counts, timestamp(v).unwrap_or(at));
        }
    }

    fn event_msg(&mut self, v: &Value, payload: Option<&Value>) {
        let Some(p) = payload else { return };
        if p.get("type").and_then(Value::as_str) != Some("token_count") {
            return;
        }
        let Some(info) = p.get("info").filter(|i| i.is_object()) else {
            return;
        };
        let Some(at) = timestamp(v) else { return };
        let total = info.get("total_token_usage").map(Counts::parse);
        let last = info.get("last_token_usage").map(Counts::parse);
        if total.is_some() && total == self.prev_total {
            self.emit(None, at);
            return;
        }
        let delta = match (last, total) {
            (Some(l), _) => Some(l),
            (None, Some(t)) => Some(match self.prev_total {
                Some(prev) if !t.regressed_from(&prev) => t.minus(&prev),
                _ => t,
            }),
            (None, None) => None,
        };
        if total.is_some() {
            self.prev_total = total;
        }
        self.emit(delta, at);
    }

    /// Soma o delta, ou o retém enquanto a rajada inicial de réplica não terminou.
    fn emit(&mut self, delta: Option<Counts>, at: DateTime<Utc>) {
        let Some(mut replay) = self.replay.take() else {
            if let Some(d) = delta {
                self.add(d, at);
            }
            return;
        };
        let chained = replay.pending.last().is_none_or(|(_, prev)| at - *prev < REPLAY_GAP);
        if chained {
            replay.pending.push((delta, at));
            self.replay = Some(replay);
            return;
        }
        self.settle(replay);
        self.emit(delta, at);
    }

    /// Uma rajada de 2+ eventos é réplica e é descartada; um evento isolado conta.
    fn settle(&mut self, replay: Replay) {
        if let [(Some(d), at)] = replay.pending[..] {
            self.add(d, at);
        }
    }

    fn finish(&mut self) {
        if let Some(replay) = self.replay.take() {
            self.settle(replay);
        }
    }

    fn add(&mut self, c: Counts, at: DateTime<Utc>) {
        let raw = self.model.as_deref().unwrap_or(FALLBACK_MODEL);
        let net = (c.input - c.cached - c.cache_write).max(0);
        let usage = Usage {
            input: net as f64,
            output: c.output as f64,
            cache_read: c.cached as f64,
            cache_write: c.cache_write as f64,
        };
        let tokens = [net, c.output, c.cached, c.cache_write];
        let date = self.session_date.unwrap_or_else(|| (self.local_date)(at));
        self.agg.add(date, raw, tokens, Cost::Priced(usage));
    }
}

fn timestamp(v: &Value) -> Option<DateTime<Utc>> {
    let s = v.get("timestamp")?.as_str()?;
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

/// Conteúdo do arquivo; `.jsonl.zst` (Codex comprime sessões antigas) é descomprimido.
fn read_session(path: &Path) -> Result<Vec<u8>, SourceError> {
    let raw = fs::File::open(path).map_err(io_err(path))?;
    let compressed = path.to_string_lossy().ends_with(COMPRESSED_EXTENSION);
    let mut out = Vec::new();
    if compressed {
        let mut dec = ruzstd::decoding::StreamingDecoder::new(raw)
            .map_err(|e| io_err(path)(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())))?;
        dec.read_to_end(&mut out).map_err(io_err(path))?;
    } else {
        std::io::BufReader::new(raw)
            .read_to_end(&mut out)
            .map_err(io_err(path))?;
    }
    Ok(out)
}

/// Data do diretório `AAAA/MM/DD` do arquivo sob `base`, a mesma que o ccusage
/// usa como `directory` da sessão. Arquivos fora desse layout não têm data.
fn session_date(base: &Path, file: &Path) -> Option<NaiveDate> {
    let mut parts = file.strip_prefix(base).ok()?.parent()?.components();
    let mut next = || parts.next()?.as_os_str().to_str()?.parse::<u32>().ok();
    let (y, m, d) = (next()?, next()?, next()?);
    NaiveDate::from_ymd_opt(i32::try_from(y).ok()?, m, d)
}

/// Raízes padrão: `$CODEX_HOME` ou `~/.codex`.
pub fn default_roots() -> Vec<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return vec![PathBuf::from(home)];
    }
    directories::BaseDirs::new()
        .map(|d| vec![d.home_dir().join(".codex")])
        .unwrap_or_default()
}

/// Uso por (data da sessão, modelo) somando todas as sessões das raízes.
pub fn daily_rows(
    roots: &[PathBuf],
    host: &str,
    local_date: LocalDate,
    prices: Option<&PriceTable>,
) -> Result<Vec<Row>, SourceError> {
    let mut agg = Aggregator::new(prices);
    for root in roots {
        for sub in SUBDIRS {
            let want = |name: &str| name.ends_with(EXTENSION) || name.ends_with(COMPRESSED_EXTENSION);
            for file in walk_files(&root.join(sub), &want, None)? {
                let bytes = read_session(&file)?;
                let mut state = FileState {
                    local_date,
                    session_date: session_date(&root.join(sub), &file),
                    agg: &mut agg,
                    model: None,
                    prev_total: None,
                    last_record: None,
                    replay: None,
                };
                bytes.split(|b| *b == b'\n').for_each(|l| state.line(l));
                state.finish();
            }
        }
    }
    let mut rows = agg.into_rows(host, AGENT);
    rows.iter_mut().for_each(|r| r.provider = PROVIDER.to_string());
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prices::PricesFile;
    use crate::source::testutil::utc_date;
    use std::collections::HashMap;

    fn tc(ts: &str, total: Option<[i64; 4]>, last: Option<[i64; 4]>) -> String {
        let u = |a: [i64; 4]| {
            format!(
                r#"{{"input_tokens":{},"cached_input_tokens":{},"cache_write_input_tokens":0,"output_tokens":{},"total_tokens":{}}}"#,
                a[0], a[1], a[2], a[3]
            )
        };
        let mut info = Vec::new();
        if let Some(t) = total {
            info.push(format!(r#""total_token_usage":{}"#, u(t)));
        }
        if let Some(l) = last {
            info.push(format!(r#""last_token_usage":{}"#, u(l)));
        }
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{{}}}}}}}"#,
            info.join(",")
        )
    }

    fn ctx(model: &str) -> String {
        format!(r#"{{"timestamp":"2026-07-01T09:00:00Z","type":"turn_context","payload":{{"model":"{model}"}}}}"#)
    }

    fn run(lines: &[String], prices: Option<&PriceTable>) -> Vec<Row> {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("sessions/2026/07/01");
        fs::create_dir_all(&day).unwrap();
        fs::write(day.join("x.jsonl"), lines.join("\n")).unwrap();
        daily_rows(&[dir.path().to_path_buf()], "h", utc_date, prices).unwrap()
    }

    const T: &str = "2026-07-01T10:00:00Z";

    #[test]
    fn last_usage_is_used_and_unchanged_total_skipped() {
        let rows = run(
            &[
                ctx("gpt-5.5"),
                tc(T, Some([100, 40, 10, 0]), Some([100, 40, 10, 0])),
                tc(T, Some([100, 40, 10, 0]), Some([100, 40, 10, 0])),
                tc(T, Some([400, 100, 30, 0]), Some([300, 60, 20, 0])),
            ],
            None,
        );
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!((r.input, r.cache_read, r.output, r.cache_write), (300, 100, 30, 0));
        assert_eq!(
            (r.model.as_str(), r.date.as_str(), r.cost_usd),
            ("gpt-5.5", "2026-07-01", None)
        );
    }

    #[test]
    fn total_only_uses_delta_and_regression_adds_whole() {
        let rows = run(
            &[
                ctx("o3"),
                tc(T, Some([1000, 0, 100, 0]), None),
                tc(T, Some([1500, 0, 150, 0]), None),
                tc(T, Some([300, 0, 50, 0]), None),
            ],
            None,
        );
        assert_eq!((rows[0].input, rows[0].output), (1000 + 500 + 300, 100 + 50 + 50));
    }

    #[test]
    fn last_only_counts_every_event_and_null_info_skipped() {
        let null =
            format!(r#"{{"timestamp":"{T}","type":"event_msg","payload":{{"type":"token_count","info":null}}}}"#);
        let rows = run(
            &[
                ctx("o3"),
                tc(T, None, Some([10, 0, 1, 0])),
                tc(T, None, Some([10, 0, 1, 0])),
                null,
            ],
            None,
        );
        assert_eq!((rows[0].input, rows[0].output), (20, 2));
    }

    #[test]
    fn compaction_rescues_matching_record_only() {
        let rec = |id: &str| {
            format!(
                r#"{{"timestamp":"{T}","type":"token_usage_record","payload":{{"response_id":"{id}","usage":{{"input_tokens":50,"cached_input_tokens":0,"output_tokens":5}}}}}}"#
            )
        };
        let compacted = |id: &str| {
            format!(r#"{{"timestamp":"{T}","type":"compacted","payload":{{"compaction_response_id":"{id}"}}}}"#)
        };
        let rows = run(&[ctx("o3"), rec("a"), rec("b"), compacted("a"), compacted("b")], None);
        // "a" é antigo (stale) e não conta; "b" é o último registro.
        assert_eq!((rows[0].input, rows[0].output), (50, 5));
        let none = run(&[ctx("o3"), rec("a")], None);
        assert!(none.is_empty());
    }

    #[test]
    fn model_comes_from_preceding_turn_context_with_fallback() {
        let rows = run(
            &[
                tc(T, None, Some([10, 0, 1, 0])),
                ctx("openai/GPT-5.5-2026-04-23"),
                tc(T, None, Some([20, 0, 2, 0])),
            ],
            None,
        );
        let by: HashMap<&str, i64> = rows.iter().map(|r| (r.model.as_str(), r.input)).collect();
        assert_eq!(by["gpt-5"], 10);
        assert_eq!(by["gpt-5.5-2026-04-23"], 20);
    }

    #[test]
    fn malformed_and_unrelated_lines_are_ignored() {
        let rows = run(
            &[
                "not json token_count".to_string(),
                String::new(),
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1}}}}"#
                    .to_string(),
                r#"{"type":"other","note":"turn_context"}"#.to_string(),
                ctx("o3"),
                tc(T, None, Some([7, 0, 1, 0])),
            ],
            None,
        );
        assert_eq!((rows.len(), rows[0].input), (1, 7));
    }

    #[test]
    fn cost_uses_price_table_and_reads_archived() {
        let file: PricesFile = serde_json::from_str(
            r#"{"models":{"o3":{"prompt":1.0,"completion":10.0,"input_cache_read":0.5,"input_cache_write":2.0}},
                "resolved":{"o3":{"id":"openai/o3","how":"exact"}}}"#,
        )
        .unwrap();
        let table = PriceTable::new(&file);
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("archived_sessions");
        fs::create_dir_all(&arch).unwrap();
        let body = [ctx("o3"), tc(T, None, Some([100, 40, 3, 0]))].join("\n");
        fs::write(arch.join("a.jsonl"), body).unwrap();
        fs::write(arch.join("ignored.json"), "x").unwrap();
        let rows = daily_rows(&[dir.path().to_path_buf()], "h", utc_date, Some(&table)).unwrap();
        // net 60*1 + out 3*10 + cached 40*0.5
        assert_eq!(rows[0].cost_usd, Some(110.0));
    }

    #[test]
    fn missing_root_is_empty_and_default_roots_nonempty_or_env() {
        let rows = daily_rows(&[PathBuf::from("/nonexistent-costs-root")], "h", utc_date, None).unwrap();
        assert!(rows.is_empty());
        let _ = default_roots();
    }

    fn meta(extra: &str) -> String {
        format!(r#"{{"timestamp":"{T}","type":"session_meta","payload":{{"id":"x"{extra}}}}}"#)
    }

    const SPAWNED: &str = r#","source":{"subagent":{"thread_spawn":{"parent_thread_id":"p"}}}"#;

    #[test]
    fn leading_burst_of_forked_session_is_replay_and_dropped() {
        let at = |s: &str| format!("2026-07-01T10:00:{s}Z");
        let burst = [
            meta(r#","forked_from_id":"p""#),
            ctx("o3"),
            tc(&at("00.000"), None, Some([100, 0, 1, 0])),
            tc(&at("00.600"), None, Some([200, 0, 2, 0])),
            tc(&at("01.200"), None, Some([300, 0, 3, 0])),
            tc(&at("30.000"), None, Some([7, 0, 1, 0])),
        ];
        assert_eq!(run(&burst, None)[0].input, 7);
        let plain = [&burst[1..]].concat();
        assert_eq!(run(&plain, None)[0].input, 607);
    }

    #[test]
    fn isolated_first_event_of_subagent_counts() {
        let at = |s: &str| format!("2026-07-01T10:00:{s}Z");
        let lines = [
            meta(SPAWNED),
            ctx("o3"),
            tc(&at("00.000"), None, Some([100, 0, 1, 0])),
            tc(&at("05.000"), None, Some([7, 0, 1, 0])),
        ];
        assert_eq!(run(&lines, None)[0].input, 107);
        let only_burst = [
            meta(SPAWNED),
            ctx("o3"),
            tc(T, None, Some([5, 0, 1, 0])),
            tc(T, None, Some([5, 0, 1, 0])),
        ];
        assert!(run(&only_burst, None).is_empty());
        let single = [meta(SPAWNED), ctx("o3"), tc(T, None, Some([5, 0, 1, 0]))];
        assert_eq!(run(&single, None)[0].input, 5);
    }

    #[test]
    fn session_goes_to_directory_date_not_event_date() {
        let rows = run(&[ctx("o3"), tc("2026-07-02T03:00:00Z", None, Some([5, 0, 1, 0]))], None);
        assert_eq!(rows[0].date, "2026-07-01");
        let base = Path::new("/r/sessions");
        assert!(session_date(base, Path::new("/r/sessions/x.jsonl")).is_none());
        assert!(session_date(base, Path::new("/r/sessions/2026/13/40/x.jsonl")).is_none());
        assert!(session_date(base, Path::new("/other/2026/07/01/x.jsonl")).is_none());
    }

    #[test]
    fn reads_zstd_compressed_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("sessions/2026/07/01");
        fs::create_dir_all(&day).unwrap();
        let body = [ctx("o3"), tc(T, None, Some([5, 0, 1, 0]))].join("\n");
        let packed = ruzstd::encoding::compress_to_vec(body.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest);
        fs::write(day.join("x.jsonl.zst"), packed).unwrap();
        fs::write(day.join("bad.jsonl.zst"), b"not zstd").unwrap();
        let err = daily_rows(&[dir.path().to_path_buf()], "h", utc_date, None).unwrap_err();
        assert!(err.to_string().contains("bad.jsonl.zst"));
        fs::remove_file(day.join("bad.jsonl.zst")).unwrap();
        let rows = daily_rows(&[dir.path().to_path_buf()], "h", utc_date, None).unwrap();
        assert_eq!((rows[0].input, rows[0].output), (5, 1));
    }

    type Key = (String, String);
    type Tokens = [i64; 4];

    /// Tokens por (data do diretório, modelo canônico) do `ccusage codex session --json`,
    /// somando o mapa `models` de cada sessão (mesma chave do Python legado).
    fn ccusage_sessions() -> std::collections::BTreeMap<Key, Tokens> {
        let out = std::process::Command::new("ccusage")
            .args(["codex", "session", "--json"])
            .output()
            .expect("ccusage no PATH");
        assert!(out.status.success(), "ccusage codex session falhou");
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        let mut map = std::collections::BTreeMap::new();
        for s in v["sessions"].as_array().into_iter().flatten() {
            let date = s["directory"].as_str().unwrap().replace('/', "-");
            for (name, m) in s["models"].as_object().into_iter().flatten() {
                let t =
                    ["inputTokens", "outputTokens", "cacheReadTokens", "cacheCreationTokens"].map(|k| int(m.get(k)));
                let slot: &mut Tokens = map.entry((date.clone(), crate::model::canon_model(name))).or_default();
                slot.iter_mut().zip(t).for_each(|(a, b)| *a += b);
            }
        }
        map
    }

    /// Porta de validação: roda só com `COSTS_VALIDATE=1` e o `ccusage` instalado.
    #[test]
    fn validate_against_ccusage() {
        if std::env::var("COSTS_VALIDATE").as_deref() != Ok("1") {
            return;
        }
        let rows = daily_rows(&default_roots(), "h", crate::source::local_date, None).unwrap();
        let native: std::collections::BTreeMap<Key, Tokens> = rows
            .iter()
            .map(|r| {
                (
                    (r.date.clone(), r.model.clone()),
                    [r.input, r.output, r.cache_read, r.cache_write],
                )
            })
            .collect();
        let theirs = ccusage_sessions();
        let keys: std::collections::BTreeSet<&Key> = native.keys().chain(theirs.keys()).collect();
        let bad: Vec<String> = keys
            .iter()
            .filter(|k| native.get(**k) != theirs.get(**k))
            .map(|k| format!("{k:?} native={:?} ccusage={:?}", native.get(*k), theirs.get(*k)))
            .collect();
        let report = format!(
            "agent=codex pairs_native={} pairs_ccusage={} match={} mismatch={}\n{}\n",
            native.len(),
            theirs.len(),
            keys.len() - bad.len(),
            bad.len(),
            bad.join("\n")
        );
        let dir = std::env::var_os("COSTS_VALIDATE_OUT").map_or_else(std::env::temp_dir, PathBuf::from);
        fs::write(dir.join("validate-codex.txt"), &report).unwrap();
        assert!(!theirs.is_empty(), "SEM DADOS: validacao nao executada");
        assert!(bad.is_empty(), "{} divergencias; ver relatorio", bad.len());
    }
}
