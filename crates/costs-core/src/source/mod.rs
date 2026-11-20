//! Parsers nativos dos logs de agentes (substituem o ccusage): pi, opencode, zcode e kimi.
//!
//! Não replicado: faixas de preço (tiers), 2x de modelos `-free`, modos rápidos e a
//! resolução de nome por provedor do LiteLLM. Custo vem de `prices.json` (OpenRouter).

pub mod claude;
pub mod codex;
pub mod kimi;
pub mod opencode;
pub mod pi;
pub mod zcode;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate, Utc};
use serde_json::Value;

use crate::model::{REPORTED, Row, canon_model, merge_same_model, stronger_basis};
use crate::prices::{Usage, components};

pub use crate::prices::PriceTable;
pub use crate::pricing::is_free_model;

/// Converte um instante UTC no dia (calendário) local.
pub type LocalDate = fn(DateTime<Utc>) -> NaiveDate;

/// Dia no fuso local da máquina (mesmo critério do ccusage sem `-z`).
pub fn local_date(ts: DateTime<Utc>) -> NaiveDate {
    ts.with_timezone(&Local).date_naive()
}

/// Falhas ao ler os logs de um agente.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("erro de E/S em {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("erro de sqlite em {path}: {source}")]
    Sqlite { path: PathBuf, source: rusqlite::Error },
}

pub(crate) fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> SourceError + '_ {
    move |source| SourceError::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub(crate) fn sql_err(path: &Path) -> impl FnOnce(rusqlite::Error) -> SourceError + '_ {
    move |source| SourceError::Sqlite {
        path: path.to_path_buf(),
        source,
    }
}

/// Divide `input` (que inclui cache) em (entrada, cache leitura, cache escrita).
pub(crate) fn split_input(input: i64, cache_read: i64, cache_write: i64) -> (i64, i64, i64) {
    let cr = cache_read.min(input);
    let cw = cache_write.min(input - cr);
    (input - cr - cw, cr, cw)
}

/// Inteiro não negativo de um campo JSON (aceita float, como o ccusage).
pub(crate) fn int(v: Option<&Value>) -> i64 {
    v.and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
        .max(0)
}

/// Origem do custo de um registro.
pub(crate) enum Cost {
    /// Custo reportado pelo agente, usado como está.
    Stored(f64),
    /// Calculado a partir da tabela de preços.
    Priced(Usage),
}

#[derive(Default)]
struct Acc {
    tokens: [i64; 4],
    cost: Option<f64>,
    basis: Option<String>,
}

/// Soma registros por (dia, modelo cru).
pub(crate) struct Aggregator<'a> {
    prices: Option<&'a PriceTable>,
    acc: BTreeMap<(NaiveDate, String), Acc>,
}

impl<'a> Aggregator<'a> {
    pub fn new(prices: Option<&'a PriceTable>) -> Self {
        Self {
            prices,
            acc: BTreeMap::new(),
        }
    }

    /// `tokens` = [entrada, saída, cache leitura, cache escrita] exibidos.
    pub fn add(&mut self, date: NaiveDate, model: &str, tokens: [i64; 4], cost: Cost) {
        // Rota gratuita é zero por definição, mesmo que o agente informe um custo nominal.
        let (cost, basis) = if is_free_model(model) {
            (Some(0.0), "free")
        } else {
            match cost {
                Cost::Stored(c) => (Some(c), REPORTED),
                Cost::Priced(u) => match self.prices.and_then(|t| t.lookup(model)) {
                    Some(entry) => (
                        Some(components(u, &entry.price).total),
                        if entry.fallback { "fallback" } else { "estimated" },
                    ),
                    None => (None, "estimated"),
                },
            }
        };
        let a = self.acc.entry((date, model.to_string())).or_default();
        for (slot, t) in a.tokens.iter_mut().zip(tokens) {
            *slot += t;
        }
        a.basis = Some(stronger_basis(a.basis.as_deref(), basis));
        if let Some(c) = cost {
            *a.cost.get_or_insert(0.0) += c;
        }
    }

    /// Linhas por (dia, modelo canônico), em ordem de data.
    pub fn into_rows(self, host: &str, agent: &str) -> Vec<Row> {
        let rows = self
            .acc
            .into_iter()
            .map(|((date, raw), a)| {
                let [input, output, cache_read, cache_write] = a.tokens;
                Row {
                    host: host.to_string(),
                    agent: agent.to_string(),
                    date: date.format("%Y-%m-%d").to_string(),
                    model: canon_model(&raw),
                    model_raw: raw,
                    provider: "outro".to_string(),
                    input,
                    output,
                    cache_read,
                    cache_write,
                    cost_usd: a.cost,
                    cost_basis: a.basis.unwrap_or_else(|| "estimated".to_string()),
                }
            })
            .collect();
        merge_same_model(rows)
    }
}

/// Arquivos regulares sob `root` (recursivo, ordenados) cujo nome satisfaz `want`.
/// Diretórios chamados `skip_dir` são ignorados. `root` ausente = vazio.
pub(crate) fn walk_files(
    root: &Path,
    want: &dyn Fn(&str) -> bool,
    skip_dir: Option<&str>,
) -> Result<Vec<PathBuf>, SourceError> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err(&dir)(e)),
        };
        for entry in entries {
            let entry = entry.map_err(io_err(&dir))?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = entry.file_type().map_err(io_err(&path))?;
            if kind.is_dir() {
                if skip_dir != Some(name.as_str()) {
                    stack.push(path);
                }
            } else if want(&name) {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Linhas JSON válidas de um arquivo; linhas malformadas são ignoradas.
pub(crate) fn read_jsonl(path: &Path) -> Result<Vec<Value>, SourceError> {
    let bytes = fs::read(path).map_err(io_err(path))?;
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use chrono::{DateTime, NaiveDate, Utc};

    use super::*;

    pub fn utc_date(ts: DateTime<Utc>) -> NaiveDate {
        ts.date_naive()
    }

    pub fn table() -> PriceTable {
        let file: crate::prices::PricesFile = serde_json::from_str(
            r#"{"models":{"m":{"prompt":0.000001,"completion":0.000002,"input_cache_read":0.0000001,"input_cache_write":0.000003}},
                "resolved":{"m":{"id":"or/m","how":"exact"}}}"#,
        )
        .unwrap();
        PriceTable::new(&file)
    }

    pub fn home() -> PathBuf {
        PathBuf::from(std::env::var_os("HOME").expect("HOME"))
    }

    /// Validação contra o ccusage real: só roda com COSTS_VALIDATE=1.
    /// Devolve false (teste ignorado) quando a variável não está definida.
    pub fn validate(agent: &str, rows: &[Row]) -> bool {
        if std::env::var("COSTS_VALIDATE").as_deref() != Ok("1") {
            return false;
        }
        let out = Command::new("ccusage")
            .args([agent, "daily", "--json"])
            .output()
            .expect("ccusage no PATH");
        assert!(out.status.success(), "ccusage {agent} falhou");
        let theirs = crate::model::parse_daily("h", agent, &String::from_utf8_lossy(&out.stdout), &Vec::new()).unwrap();
        let key = |r: &Row| (r.date.clone(), r.model.clone());
        let tok = |r: &Row| [r.input, r.output, r.cache_read, r.cache_write];
        let a: BTreeMap<_, _> = rows.iter().map(|r| (key(r), tok(r))).collect();
        let b: BTreeMap<_, _> = theirs.iter().map(|r| (key(r), tok(r))).collect();
        let (mut ok, mut bad) = (0, Vec::new());
        for k in a.keys().chain(b.keys()).collect::<std::collections::BTreeSet<_>>() {
            if a.get(k) == b.get(k) {
                ok += 1;
            } else {
                bad.push(format!("{k:?} native={:?} ccusage={:?}", a.get(k), b.get(k)));
            }
        }
        let report = format!(
            "agent={agent} pairs_native={} pairs_ccusage={} match={ok} mismatch={}\n{}\n",
            a.len(),
            b.len(),
            bad.len(),
            bad.join("\n")
        );
        let dir = std::env::var_os("COSTS_VALIDATE_OUT").map_or_else(std::env::temp_dir, PathBuf::from);
        std::fs::write(Path::new(&dir).join(format!("validate-{agent}.txt")), &report).unwrap();
        assert!(
            !a.is_empty() || !b.is_empty(),
            "SEM DADOS nesta maquina para {agent}: validacao nao executada"
        );
        assert!(bad.is_empty(), "{agent}: {} divergencias; ver relatorio", bad.len());
        true
    }
}

#[cfg(test)]
mod aggregator_tests {
    use chrono::NaiveDate;

    use super::*;

    fn table(file: &str) -> PriceTable {
        let file: crate::prices::PricesFile = serde_json::from_str(file).unwrap();
        PriceTable::new(&file)
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()
    }

    #[test]
    fn rota_gratuita_custa_zero_e_nao_fica_sem_preco() {
        let t = table(r#"{"models":{},"resolved":{}}"#);
        let mut agg = Aggregator::new(Some(&t));
        agg.add(
            day(),
            "minimax/minimax-m3:free",
            [100, 10, 0, 0],
            Cost::Priced(Usage {
                input: 100.0,
                output: 10.0,
                ..Usage::default()
            }),
        );
        let rows = agg.into_rows("h", "pi");
        assert_eq!(rows[0].cost_usd, Some(0.0));
        assert_eq!(rows[0].cost_basis, "free");
    }

    #[test]
    fn rota_gratuita_ignora_custo_informado_pelo_agente() {
        let t = table(r#"{"models":{},"resolved":{}}"#);
        let mut agg = Aggregator::new(Some(&t));
        agg.add(
            day(),
            "muse-spark-1.3-contributor-free",
            [10, 1, 0, 0],
            Cost::Stored(1.5),
        );
        let rows = agg.into_rows("h", "pi");
        assert_eq!(rows[0].cost_usd, Some(0.0));
        assert_eq!(rows[0].cost_basis, "free");
    }

    #[test]
    fn preco_de_irmao_marca_fallback() {
        let t = table(
            r#"{"models":{"k3":{"prompt":0.001,"completion":0.002}},
                "resolved":{"k3":{"id":"moonshotai/kimi-k3","how":"sibling"}}}"#,
        );
        let mut agg = Aggregator::new(Some(&t));
        agg.add(
            day(),
            "k3",
            [1000, 100, 0, 0],
            Cost::Priced(Usage {
                input: 1000.0,
                output: 100.0,
                ..Usage::default()
            }),
        );
        let rows = agg.into_rows("h", "kimi");
        assert!((rows[0].cost_usd.unwrap() - 1.2).abs() < 1e-9);
        assert_eq!(rows[0].cost_basis, "fallback");
    }

    #[test]
    fn modelo_sem_preco_continua_nulo() {
        let t = table(r#"{"models":{},"resolved":{}}"#);
        let mut agg = Aggregator::new(Some(&t));
        agg.add(day(), "space-bunny", [10, 1, 0, 0], Cost::Priced(Usage::default()));
        assert_eq!(agg.into_rows("h", "pi")[0].cost_usd, None);
    }
}
