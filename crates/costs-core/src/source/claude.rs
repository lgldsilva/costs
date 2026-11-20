//! Parser nativo dos logs do Claude Code (`~/.claude/projects/**/*.jsonl`).
//!
//! Reproduz as regras do `ccusage claude daily`: aceitação estrita de linhas,
//! dedup por `(message.id, requestId)` com vencedor por total de tokens, e
//! `cache_creation` dividido (5m + 1h) substituindo o valor de topo. O custo
//! usa `costUSD` do log quando existe; senão a tabela OpenRouter (preço base,
//! sem faixas de 200k, sem multiplicador `fast`, 5m e 1h com o mesmo preço).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;

use super::{LocalDate, PriceTable, SourceError, io_err, walk_files};
use crate::model::{Row, Rules, canon_model, merge_same_model, provider_of};
use crate::prices::{Usage, components};

const AGENT: &str = "claude";
const SYNTHETIC: &str = "<synthetic>";
const USAGE_MARKER: &[u8] = b"\"usage\":{";
const ADVISOR_TYPE: &str = "advisor_message";
const BASIS_LOG: &str = "reported";
const BASIS_ESTIMATED: &str = "estimated";

/// Raízes padrão: `CLAUDE_CONFIG_DIR` (vírgulas) ou `~/.claude` e `$XDG_CONFIG_HOME/claude`.
pub fn default_roots(config_dir: Option<&str>, home: Option<&Path>, xdg: Option<&Path>) -> Vec<PathBuf> {
    if let Some(list) = config_dir.filter(|s| !s.trim().is_empty()) {
        return list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
    }
    let mut roots: Vec<PathBuf> = home.map(|h| h.join(".claude")).into_iter().collect();
    roots.extend(xdg.map(|x| x.join("claude")));
    roots
}

#[derive(Debug, Clone)]
struct Part {
    model: String,
    fast: bool,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    cost: Option<f64>,
}

impl Part {
    fn total(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

#[derive(Debug, Clone)]
struct Entry {
    day: NaiveDate,
    main: Part,
    advisors: Vec<Part>,
}

type DedupKey = (String, Option<String>);

fn u64_field(v: &Value, key: &str) -> Option<Option<i64>> {
    match v.get(key) {
        None => Some(None),
        Some(x) => x.as_u64().and_then(|n| i64::try_from(n).ok()).map(Some),
    }
}

fn cache_write_of(usage: &Value) -> Option<i64> {
    if let Some(split) = usage.get("cache_creation").filter(|c| c.is_object()) {
        let a = u64_field(split, "ephemeral_5m_input_tokens")?.unwrap_or(0);
        let b = u64_field(split, "ephemeral_1h_input_tokens")?.unwrap_or(0);
        return Some(a + b);
    }
    Some(u64_field(usage, "cache_creation_input_tokens")?.unwrap_or(0))
}

fn part_of(model: &str, fast: bool, usage: &Value, cost: Option<f64>) -> Option<Part> {
    Some(Part {
        model: model.to_string(),
        fast,
        input: u64_field(usage, "input_tokens")??,
        output: u64_field(usage, "output_tokens")??,
        cache_read: u64_field(usage, "cache_read_input_tokens")?.unwrap_or(0),
        cache_write: cache_write_of(usage)?,
        cost,
    })
}

fn advisors_of(usage: &Value) -> Vec<Part> {
    let items = usage.get("iterations").and_then(Value::as_array);
    items
        .into_iter()
        .flatten()
        .filter(|it| it.get("type").and_then(Value::as_str) == Some(ADVISOR_TYPE))
        .filter_map(|it| {
            let model = it.get("model").and_then(Value::as_str).filter(|m| !m.is_empty())?;
            part_of(model, false, it, None)
        })
        .collect()
}

fn cost_field(v: &Value) -> Option<Option<f64>> {
    match v.get("costUSD") {
        None => Some(None),
        Some(x) if x.is_number() => Some(x.as_f64()),
        Some(_) => None,
    }
}

/// Interpreta uma linha; `None` se ela não for aceita pelo ccusage.
fn parse_line(line: &[u8], day_of: LocalDate) -> Option<(Option<DedupKey>, Entry)> {
    if !line.windows(USAGE_MARKER.len()).any(|w| w == USAGE_MARKER) {
        return None;
    }
    let v: Value = serde_json::from_slice(line).ok()?;
    let ts = DateTime::parse_from_rfc3339(v.get("timestamp")?.as_str()?).ok()?;
    let msg = v.get("message")?.as_object()?;
    let model = msg.get("model")?.as_str().filter(|m| !m.is_empty())?;
    let id = match msg.get("id") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()).filter(|s| !s.is_empty()),
        Some(_) => return None,
    };
    let usage = msg.get("usage")?;
    let fast = match usage.get("speed") {
        None => false,
        Some(s) => match s.as_str()? {
            "fast" => true,
            "standard" => false,
            _ => return None,
        },
    };
    let cost = cost_field(&v)?;
    let main = part_of(model, fast, usage, cost)?;
    let request = v.get("requestId").and_then(Value::as_str).map(str::to_string);
    let entry = Entry {
        day: day_of(ts.with_timezone(&Utc)),
        main,
        advisors: advisors_of(usage),
    };
    Some((id.map(|i| (i, request)), entry))
}

fn calc_cost(part: &Part, prices: Option<&PriceTable>) -> Option<f64> {
    let price = prices?.lookup(&part.model)?;
    let usage = Usage {
        input: part.input as f64,
        output: part.output as f64,
        cache_read: part.cache_read as f64,
        cache_write: part.cache_write as f64,
    };
    Some(components(usage, &price.price).total)
}

fn effective_cost(part: &Part, prices: Option<&PriceTable>) -> f64 {
    part.cost.or_else(|| calc_cost(part, prices)).unwrap_or(0.0)
}

/// `new` vence `old` por mais tokens; empate, maior custo; empate total, o primeiro.
fn beats(new: &Entry, old: &Entry, prices: Option<&PriceTable>) -> bool {
    match new.main.total().cmp(&old.main.total()) {
        std::cmp::Ordering::Equal => effective_cost(&new.main, prices) > effective_cost(&old.main, prices),
        ord => ord.is_gt(),
    }
}

/// Arquivos `.jsonl` sob `projects/` de cada raiz (a raiz pode ser o próprio `projects/`).
fn jsonl_files(roots: &[PathBuf]) -> Result<Vec<PathBuf>, SourceError> {
    let mut files = Vec::new();
    for root in roots {
        let projects = root.join("projects");
        let base = if projects.is_dir() { projects } else { root.clone() };
        files.extend(walk_files(&base, &|n| n.ends_with(".jsonl"), None)?);
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_entries(
    files: &[PathBuf],
    day_of: LocalDate,
    prices: Option<&PriceTable>,
) -> Result<Vec<Entry>, SourceError> {
    let mut keyed: HashMap<DedupKey, Entry> = HashMap::new();
    let mut loose = Vec::new();
    for path in files {
        let bytes = fs::read(path).map_err(io_err(path))?;
        for line in bytes.split(|b| *b == b'\n') {
            match parse_line(line, day_of) {
                None => {}
                Some((None, entry)) => loose.push(entry),
                Some((Some(key), entry)) => match keyed.get(&key) {
                    Some(old) if !beats(&entry, old, prices) => {}
                    _ => {
                        keyed.insert(key, entry);
                    }
                },
            }
        }
    }
    loose.extend(keyed.into_values());
    Ok(loose)
}

fn part_row(host: &str, rules: &Rules, day: NaiveDate, part: &Part, prices: Option<&PriceTable>) -> Row {
    let raw = if part.fast {
        format!("{}-fast", part.model)
    } else {
        part.model.clone()
    };
    let model = canon_model(&raw);
    let (cost, basis) = match part.cost {
        Some(c) => (Some(c), BASIS_LOG),
        None => (calc_cost(part, prices), BASIS_ESTIMATED),
    };
    Row {
        host: host.to_string(),
        agent: AGENT.to_string(),
        date: day.format("%Y-%m-%d").to_string(),
        provider: provider_of(&model, AGENT, rules),
        model,
        model_raw: raw,
        input: part.input,
        output: part.output,
        cache_read: part.cache_read,
        cache_write: part.cache_write,
        cost_usd: cost,
        cost_basis: basis.to_string(),
    }
}

/// Linhas diárias por modelo, equivalentes a `modelBreakdowns` do ccusage.
///
/// `<synthetic>` fica de fora. Linhas ordenadas por (data, modelo).
pub fn daily_rows(
    roots: &[PathBuf],
    host: &str,
    rules: &Rules,
    day_of: LocalDate,
    prices: Option<&PriceTable>,
) -> Result<Vec<Row>, SourceError> {
    let entries = collect_entries(&jsonl_files(roots)?, day_of, prices)?;
    let rows = entries
        .iter()
        .flat_map(|e| std::iter::once(&e.main).chain(&e.advisors).map(|p| (e.day, p)))
        .filter(|(_, p)| p.model != SYNTHETIC)
        .map(|(day, p)| part_row(host, rules, day, p, prices))
        .collect();
    let mut merged = merge_same_model(rows);
    merged.sort_by(|a, b| (&a.date, &a.model).cmp(&(&b.date, &b.model)));
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prices::PricesFile;
    use crate::source::local_date;
    use std::io::Write;

    fn utc_day(t: DateTime<Utc>) -> NaiveDate {
        t.date_naive()
    }

    fn write(dir: &Path, rel: &str, lines: &[&str]) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = fs::File::create(p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    fn rows(dir: &Path, prices: Option<&PriceTable>) -> Vec<Row> {
        daily_rows(&[dir.to_path_buf()], "h", &Vec::new(), utc_day, prices).unwrap()
    }

    fn line(id: &str, req: &str, out: u64, extra: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-03-01T10:00:00Z","requestId":"{req}","message":{{"id":"{id}","model":"claude-x","usage":{{"input_tokens":10,"output_tokens":{out}{extra}}}}}}}"#
        )
    }

    #[test]
    fn dedup_keeps_largest_total() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (line("m", "r", 5, ""), line("m", "r", 9, ""), line("m", "r", 3, ""));
        write(d.path(), "projects/p/a.jsonl", &[&a, &b]);
        write(d.path(), "projects/p/sub/b.jsonl", &[&c]);
        let r = rows(d.path(), None);
        assert_eq!((r.len(), r[0].input, r[0].output), (1, 10, 9));
    }

    #[test]
    fn dedup_distinguishes_request_and_skips_idless() {
        let d = tempfile::tempdir().unwrap();
        let idless = r#"{"timestamp":"2026-03-01T10:00:00Z","message":{"model":"claude-x","usage":{"input_tokens":1,"output_tokens":1}}}"#;
        write(
            d.path(),
            "projects/a.jsonl",
            &[&line("m", "r1", 1, ""), &line("m", "r2", 1, ""), idless, idless],
        );
        assert_eq!(rows(d.path(), None)[0].input, 10 + 10 + 1 + 1);
    }

    #[test]
    fn tie_prefers_larger_cost() {
        let d = tempfile::tempdir().unwrap();
        let l = |c: &str| {
            format!(
                r#"{{"timestamp":"2026-03-01T10:00:00Z","requestId":"r","costUSD":{c},"message":{{"id":"m","model":"claude-x","usage":{{"input_tokens":1,"output_tokens":1}}}}}}"#
            )
        };
        write(d.path(), "projects/a.jsonl", &[&l("1"), &l("2"), &l("0.5")]);
        assert_eq!(rows(d.path(), None)[0].cost_usd, Some(2.0));
    }

    #[test]
    fn cache_split_overrides_top_level() {
        let d = tempfile::tempdir().unwrap();
        let extra = r#","cache_creation_input_tokens":1000000,"cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":50},"cache_read_input_tokens":7"#;
        write(d.path(), "projects/a.jsonl", &[&line("m", "r", 1, extra)]);
        let r = rows(d.path(), None);
        assert_eq!((r[0].cache_write, r[0].cache_read), (150, 7));
    }

    #[test]
    fn top_level_cache_without_split() {
        let d = tempfile::tempdir().unwrap();
        let extra = r#","cache_creation_input_tokens":42"#;
        write(d.path(), "projects/a.jsonl", &[&line("m", "r", 1, extra)]);
        assert_eq!(rows(d.path(), None)[0].cache_write, 42);
    }

    #[test]
    fn synthetic_is_excluded() {
        let d = tempfile::tempdir().unwrap();
        let l = line("m", "r", 1, "").replace("claude-x", SYNTHETIC);
        write(d.path(), "projects/a.jsonl", &[&l]);
        assert!(rows(d.path(), None).is_empty());
    }

    #[test]
    fn malformed_lines_are_dropped() {
        let d = tempfile::tempdir().unwrap();
        let ok = line("ok", "r", 1, "");
        let bad = [
            "not json".to_string(),
            "{}".to_string(),
            line("n", "r", 1, "").replace(r#""id":"n""#, r#""id":null"#),
            line("f", "r", 1, "").replace(r#""input_tokens":10"#, r#""input_tokens":10.0"#),
            line("g", "r", 1, "").replace(r#""input_tokens":10"#, r#""input_tokens":-1"#),
            line("s", "r", 1, ",\"speed\":\"Fast\""),
            line("t", "r", 1, "").replace("2026-03-01T10:00:00Z", "yesterday"),
            line("e", "r", 1, "").replace("claude-x", ""),
            line("c", "r", 1, "").replace(r#""requestId":"r""#, r#""requestId":"r","costUSD":"1""#),
            line("o", "r", 1, "").replace(r#""output_tokens":1"#, r#""x":1"#),
            line("u", "r", 1, "").replace("\"usage\":{", "\"usage\": {"),
        ];
        let mut all: Vec<&str> = bad.iter().map(String::as_str).collect();
        all.push(&ok);
        write(d.path(), "projects/a.jsonl", &all);
        write(d.path(), "projects/a.json", &[&line("z", "r", 99, "")]);
        let r = rows(d.path(), None);
        assert_eq!((r.len(), r[0].output), (1, 1));
    }

    #[test]
    fn fast_speed_suffixes_model_and_offsets_shift_day() {
        let d = tempfile::tempdir().unwrap();
        let l = line("m", "r", 1, ",\"speed\":\"fast\"").replace("2026-03-01T10:00:00Z", "2026-03-01T23:30:00-03:00");
        write(d.path(), "projects/a.jsonl", &[&l]);
        let r = rows(d.path(), None);
        assert_eq!(
            (r[0].model.as_str(), r[0].date.as_str()),
            ("claude-x-fast", "2026-03-02")
        );
    }

    #[test]
    fn advisor_iterations_become_extra_rows() {
        let d = tempfile::tempdir().unwrap();
        let extra = r#","iterations":[{"type":"advisor_message","model":"claude-adv","input_tokens":3,"output_tokens":4},{"type":"other","model":"z","input_tokens":1,"output_tokens":1}]"#;
        write(d.path(), "projects/a.jsonl", &[&line("m", "r", 1, extra)]);
        let r = rows(d.path(), None);
        let adv = r.iter().find(|x| x.model == "claude-adv").unwrap();
        assert_eq!((r.len(), adv.input, adv.output), (2, 3, 4));
    }

    #[test]
    fn cost_uses_log_value_else_table() {
        let d = tempfile::tempdir().unwrap();
        let with_cost = line("a", "r", 1, "").replace(r#""requestId":"r""#, r#""requestId":"r","costUSD":0.5"#);
        let calc = r#"{"timestamp":"2026-03-02T10:00:00Z","message":{"id":"b","model":"claude-x","usage":{"input_tokens":1000,"output_tokens":100,"cache_read_input_tokens":10}}}"#;
        write(d.path(), "projects/a.jsonl", &[&with_cost, calc]);
        let file: PricesFile = serde_json::from_str(
            r#"{"models":{"claude-x":{"prompt":0.001,"completion":0.01,"input_cache_read":0.0001}},
                "resolved":{"claude-x":{"id":"or/x","how":"exact"}}}"#,
        )
        .unwrap();
        let pt = PriceTable::new(&file);
        let r = rows(d.path(), Some(&pt));
        assert_eq!((r[0].cost_usd, r[0].cost_basis.as_str()), (Some(0.5), "reported"));
        assert!((r[1].cost_usd.unwrap() - 2.001).abs() < 1e-9);
        assert_eq!(rows(d.path(), None)[1].cost_usd, None);
    }

    #[test]
    fn roots_may_be_projects_dir_and_missing_roots_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "top.jsonl", &[&line("m", "r", 1, "")]);
        let roots = [d.path().to_path_buf(), d.path().join("nope")];
        assert_eq!(daily_rows(&roots, "h", &Vec::new(), utc_day, None).unwrap().len(), 1);
    }

    #[test]
    fn default_roots_prefer_env_list() {
        let home = Path::new("/h");
        assert_eq!(
            default_roots(Some("/a, /b/projects,"), Some(home), None),
            [PathBuf::from("/a"), PathBuf::from("/b/projects")]
        );
        assert_eq!(
            default_roots(None, Some(home), Some(Path::new("/x"))),
            [PathBuf::from("/h/.claude"), PathBuf::from("/x/claude")]
        );
        assert_eq!(
            default_roots(Some(" "), Some(home), None),
            [PathBuf::from("/h/.claude")]
        );
    }

    /// Porta de validação: roda só com `COSTS_VALIDATE=1` e o `ccusage` instalado.
    #[test]
    fn validate_against_ccusage() {
        let home = crate::source::testutil::home();
        let env = std::env::var("CLAUDE_CONFIG_DIR").ok();
        let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
        let roots = default_roots(env.as_deref(), Some(&home), xdg.as_deref());
        let rows = daily_rows(&roots, "h", &Vec::new(), local_date, None).unwrap();
        crate::source::testutil::validate("claude", &rows);
    }
}
