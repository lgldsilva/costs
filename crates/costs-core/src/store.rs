//! Armazenamento SQLite: schema, migração, gravação e consultas de gasto.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};

use crate::model::{ParseError, Row, basis_rank, cost_eq, load_rules, provider_of};

pub const SCHEMA_VERSION: i64 = 3;
const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);
const BACKUP_PAGES_PER_STEP: std::ffi::c_int = 100;
/// Retenção padrão da tabela `runs`, em dias.
pub const DEFAULT_PRUNE_DAYS: i64 = 90;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS usage (
    host         TEXT NOT NULL,
    agent        TEXT NOT NULL,
    date         TEXT NOT NULL,
    model        TEXT NOT NULL,
    model_raw    TEXT NOT NULL,
    provider     TEXT NOT NULL,
    input        INTEGER,
    output       INTEGER,
    cache_read   INTEGER,
    cache_write  INTEGER,
    cost_usd     REAL,
    cost_basis   TEXT NOT NULL,
    collected_at TEXT NOT NULL,
    PRIMARY KEY (host, agent, date, model)
);
CREATE TABLE IF NOT EXISTS runs (
    ts      TEXT NOT NULL,
    host    TEXT NOT NULL,
    agent   TEXT NOT NULL,
    status  TEXT NOT NULL,
    message TEXT,
    rows    INTEGER
);
-- Log append-only do que cada coleta viu: preserva o dado mesmo quando o log do
-- harness e arquivado/apagado ou o dia chega incompleto. `kept = 1` virou o valor
-- atual; `kept = 0` foi ignorado (tentativa de encolher o dia).
CREATE TABLE IF NOT EXISTS usage_history (
    ts          TEXT NOT NULL,
    origin      TEXT NOT NULL,
    host        TEXT NOT NULL,
    agent       TEXT NOT NULL,
    date        TEXT NOT NULL,
    model       TEXT NOT NULL,
    model_raw   TEXT NOT NULL,
    provider    TEXT NOT NULL,
    input       INTEGER,
    output      INTEGER,
    cache_read  INTEGER,
    cache_write INTEGER,
    cost_usd    REAL,
    cost_basis  TEXT NOT NULL,
    kept        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS usage_history_key ON usage_history (host, agent, date, model, ts);
-- Frescor por par (host, agente): sem o índice a consulta do dashboard é quadrática.
CREATE INDEX IF NOT EXISTS runs_pair_ts ON runs (host, agent, ts);
";

/// Grava uma observação no log append-only. `kept = 1` quando virou o valor atual.
const HISTORY_SQL: &str = "
INSERT INTO usage_history (ts, origin, host, agent, date, model, model_raw, provider,
                           input, output, cache_read, cache_write, cost_usd, cost_basis, kept)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
";

/// Colunas que o schema v2 acrescentou. A migração preserva o histórico em vez de
/// recriar a tabela: dias cujos logs de origem já foram podados só existem aqui.
const MIGRATION_COLUMNS: [(&str, &str); 9] = [
    ("provider", "TEXT NOT NULL DEFAULT 'outro'"),
    ("model_raw", "TEXT NOT NULL DEFAULT ''"),
    ("input", "INTEGER DEFAULT 0"),
    ("output", "INTEGER DEFAULT 0"),
    ("cache_read", "INTEGER DEFAULT 0"),
    ("cache_write", "INTEGER DEFAULT 0"),
    ("cost_usd", "REAL"),
    ("cost_basis", "TEXT NOT NULL DEFAULT 'estimated'"),
    ("collected_at", "TEXT NOT NULL DEFAULT ''"),
];

const UPSERT_SQL: &str = "
INSERT INTO usage (host, agent, date, model, model_raw, provider, input, output,
                   cache_read, cache_write, cost_usd, cost_basis, collected_at)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT (host, agent, date, model) DO UPDATE SET
    model_raw = excluded.model_raw,
    provider = excluded.provider,
    input = excluded.input,
    output = excluded.output,
    cache_read = excluded.cache_read,
    cache_write = excluded.cache_write,
    cost_usd = excluded.cost_usd,
    cost_basis = excluded.cost_basis,
    collected_at = excluded.collected_at
";

/// Total e quebra `(rótulo, US$, tokens)` de `spend_by_provider`.
pub type ProviderSpend = (f64, Vec<(String, f64, i64)>);

/// Linha já gravada, no que importa para o merge.
#[derive(Debug, Clone, PartialEq)]
pub struct Stored {
    pub tokens: i64,
    pub cost: Option<f64>,
    pub basis: String,
    pub model_raw: String,
    pub provider: String,
}

/// O que `merge_rows` faz com uma observação.
#[derive(Debug, Clone, PartialEq)]
pub enum Merge {
    /// Dia/modelo novo, ou a fonte ganhou tokens: grava a linha inteira.
    Write,
    /// Nada a fazer.
    Unchanged,
    /// Observação menor que a gravada: mantém o valor atual (o dia nunca encolhe).
    Shrunk,
    /// Mesmos tokens: atualiza só o que a observação melhora (custo e base, metadados).
    Refresh { cost: Option<f64>, basis: String },
}

/// Decide o merge de uma observação. Com os mesmos tokens o custo gravado só muda para
/// preencher um NULL ou subir de base (`estimated` → `reported`): reprecificar o
/// histórico com a tabela do dia é papel do `costs repricing`, e um custo conhecido
/// nunca vira NULL (catálogo de preços que falhou no dia).
pub fn decide_merge(stored: Option<&Stored>, r: &Row) -> Merge {
    let Some(old) = stored else { return Merge::Write };
    let new_total = r.input + r.output + r.cache_read + r.cache_write;
    if new_total > old.tokens {
        return Merge::Write;
    }
    if new_total < old.tokens {
        return Merge::Shrunk;
    }
    let upgrade = r.cost_usd.is_some() && (old.cost.is_none() || basis_rank(&r.cost_basis) > basis_rank(&old.basis));
    let (cost, basis) = if upgrade {
        (r.cost_usd, r.cost_basis.clone())
    } else {
        (old.cost, old.basis.clone())
    };
    let meta_changed = r.model_raw != old.model_raw || r.provider != old.provider;
    if !meta_changed && cost_eq(cost, old.cost) && basis == old.basis {
        return Merge::Unchanged;
    }
    Merge::Refresh { cost, basis }
}

/// Resultado de `merge_rows`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeOutcome {
    /// Linhas novas ou que cresceram (e viraram o valor atual).
    pub written: usize,
    /// Linhas idênticas ao que já estava gravado.
    pub unchanged: usize,
    /// Observações menores que o gravado: mantidas fora para não encolher o dia.
    pub kept: usize,
}

/// Falhas do armazenamento.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("erro SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("regras de provedor inválidas: {0}")]
    Rules(#[from] ParseError),
    #[error("banco nao encontrado: {0}")]
    Missing(PathBuf),
}

/// Instante atual em UTC. Só a camada de CLI chama; a lógica recebe o relógio por parâmetro.
pub fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

/// ISO-8601 com segundos e offset `+00:00` (igual ao `isoformat(timespec="seconds")`).
pub fn format_ts(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// Banco SQLite de uso.
pub struct Store {
    con: Connection,
}

fn connect(path: &Path) -> Result<Connection, StoreError> {
    let con = Connection::open(path)?;
    con.busy_timeout(BUSY_TIMEOUT)?; // clique no menu + LaunchAgent podem coincidir
    Ok(con)
}

fn usage_columns(con: &Connection) -> Result<HashSet<String>, StoreError> {
    let mut stmt = con.prepare("PRAGMA table_info(usage)")?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<_, _>>()?;
    Ok(cols)
}

fn backup_path(path: &Path, now: DateTime<Utc>) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.v1-{}.bak", now.format("%Y%m%dT%H%M%SZ")))
}

/// v1 -> v2 sem perder linhas: copia o banco (API de backup), adiciona colunas e faz backfill.
fn migrate_v1(
    path: &Path,
    con: &mut Connection,
    cols: &HashSet<String>,
    rules_toml: &str,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let mut bak = Connection::open(backup_path(path, now))?;
    Backup::new(con, &mut bak)?.run_to_completion(BACKUP_PAGES_PER_STEP, Duration::ZERO, None)?;
    drop(bak);

    let tx = con.transaction()?;
    for (name, ddl) in MIGRATION_COLUMNS {
        if !cols.contains(name) {
            tx.execute_batch(&format!("ALTER TABLE usage ADD COLUMN {name} {ddl}"))?;
        }
    }
    if !cols.contains("provider") {
        let rules = load_rules(rules_toml)?;
        let rows: Vec<(i64, String, String)> = {
            let mut stmt = tx.prepare("SELECT rowid, agent, model FROM usage")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<_, _>>()?
        };
        let mut upd = tx.prepare("UPDATE usage SET provider = ? WHERE rowid = ?")?;
        for (rowid, agent, model) in rows {
            upd.execute(params![provider_of(&model, &agent, &rules), rowid])?;
        }
    }
    if !cols.contains("model_raw") {
        tx.execute("UPDATE usage SET model_raw = model WHERE model_raw = ''", [])?;
    }
    tx.commit()?;
    Ok(())
}

impl Store {
    /// Abre (e migra, se v1) o banco. `rules_toml` é o texto de `models.toml`;
    /// `now` nomeia o backup da migração.
    pub fn open(path: &Path, rules_toml: &str, now: DateTime<Utc>) -> Result<Self, StoreError> {
        let mut con = connect(path)?;
        let version: i64 = con.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let cols = usage_columns(&con)?;
        // v1 -> v2 tem colunas novas; v2 -> v3 só acrescenta a tabela de histórico.
        if !cols.is_empty() && (version < 2 || !cols.contains("provider")) {
            migrate_v1(path, &mut con, &cols, rules_toml, now)?;
        }
        con.execute_batch(SCHEMA)?;
        con.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        Ok(Self { con })
    }

    /// Conexão subjacente (consultas ad hoc e testes).
    pub fn connection(&self) -> &Connection {
        &self.con
    }

    /// Mantém a tabela de execuções limitada (ela cresce a cada coleta).
    pub fn prune_runs(&self, days: i64, now: DateTime<Utc>) -> Result<(), StoreError> {
        let cutoff = format_ts(now - TimeDelta::days(days));
        self.con.execute("DELETE FROM runs WHERE ts < ?", [cutoff])?;
        Ok(())
    }

    /// Apaga o que o banco tem para (host, agente, dia) dos dias que a fonte reenviou,
    /// guardando o valor antigo em `usage_history` (origem `replace`).
    ///
    /// Dias fora da lista não são tocados (logs podados do Claude continuam no banco).
    /// Use só quando a fonte foi corrigida de propósito: o `sync` normal faz merge e
    /// nunca encolhe um dia.
    pub fn replace_days(&self, host: &str, agent: &str, dates: &HashSet<String>, ts: &str) -> Result<(), StoreError> {
        let mut sorted: Vec<&String> = dates.iter().collect();
        sorted.sort();
        let tx = self.con.unchecked_transaction()?;
        for d in sorted {
            tx.execute(
                "INSERT INTO usage_history (ts, origin, host, agent, date, model, model_raw, provider, \
                 input, output, cache_read, cache_write, cost_usd, cost_basis, kept) \
                 SELECT ?1, 'replace', host, agent, date, model, model_raw, provider, \
                        input, output, cache_read, cache_write, cost_usd, cost_basis, 0 \
                 FROM usage WHERE host = ?2 AND agent = ?3 AND date = ?4",
                params![ts, host, agent, d],
            )?;
            tx.execute(
                "DELETE FROM usage WHERE host = ? AND agent = ? AND date = ?",
                params![host, agent, d],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Grava as linhas somando ao histórico local: um dia nunca encolhe (ver `decide_merge`).
    ///
    /// Se a observação nova tem menos tokens que a gravada (log arquivado, sessão
    /// apagada, dia incompleto), o valor atual é mantido e a tentativa vai para
    /// `usage_history` com `kept = 0`, uma vez: a mesma tentativa repetida a cada sync não
    /// é registrada de novo.
    pub fn merge_rows(&self, rows: &[Row], collected_at: &str) -> Result<MergeOutcome, StoreError> {
        let tx = self.con.unchecked_transaction()?;
        let mut out = MergeOutcome::default();
        {
            let mut sel = tx.prepare(
                "SELECT input, output, cache_read, cache_write, cost_usd, cost_basis, model_raw, provider \
                 FROM usage WHERE host = ?1 AND agent = ?2 AND date = ?3 AND model = ?4",
            )?;
            let mut last_hist = tx.prepare(
                "SELECT input + output + cache_read + cache_write, cost_usd, cost_basis, kept FROM usage_history \
                 WHERE host = ?1 AND agent = ?2 AND date = ?3 AND model = ?4 ORDER BY ts DESC, rowid DESC LIMIT 1",
            )?;
            let mut write = tx.prepare(UPSERT_SQL)?;
            let mut refresh = tx.prepare(
                "UPDATE usage SET cost_usd = ?5, cost_basis = ?6, model_raw = ?7, provider = ?8, collected_at = ?9 \
                 WHERE host = ?1 AND agent = ?2 AND date = ?3 AND model = ?4",
            )?;
            let mut hist = tx.prepare(HISTORY_SQL)?;
            for r in rows {
                let stored = sel
                    .query_row(params![r.host, r.agent, r.date, r.model], |x| {
                        let tok = |i| x.get::<_, Option<i64>>(i).map(Option::unwrap_or_default);
                        Ok(Stored {
                            tokens: tok(0)? + tok(1)? + tok(2)? + tok(3)?,
                            cost: x.get(4)?,
                            basis: x.get(5)?,
                            model_raw: x.get(6)?,
                            provider: x.get(7)?,
                        })
                    })
                    .optional()?;
                let mut log = |kept: bool| {
                    hist.execute(params![
                        collected_at,
                        "sync",
                        r.host,
                        r.agent,
                        r.date,
                        r.model,
                        r.model_raw,
                        r.provider,
                        r.input,
                        r.output,
                        r.cache_read,
                        r.cache_write,
                        r.cost_usd,
                        r.cost_basis,
                        i64::from(kept)
                    ])
                };
                match decide_merge(stored.as_ref(), r) {
                    Merge::Unchanged => out.unchanged += 1,
                    Merge::Shrunk => {
                        out.kept += 1;
                        let total = r.input + r.output + r.cache_read + r.cache_write;
                        let last = last_hist
                            .query_row(params![r.host, r.agent, r.date, r.model], |x| {
                                Ok((
                                    x.get::<_, i64>(0)?,
                                    x.get::<_, Option<f64>>(1)?,
                                    x.get::<_, String>(2)?,
                                    x.get::<_, i64>(3)?,
                                ))
                            })
                            .optional()?;
                        let repeated = last.is_some_and(|(t, c, b, k)| {
                            k == 0 && t == total && cost_eq(c, r.cost_usd) && b == r.cost_basis
                        });
                        if !repeated {
                            log(false)?;
                        }
                    }
                    Merge::Write => {
                        if stored.is_some() {
                            log(true)?;
                        }
                        write.execute(params![
                            r.host,
                            r.agent,
                            r.date,
                            r.model,
                            r.model_raw,
                            r.provider,
                            r.input,
                            r.output,
                            r.cache_read,
                            r.cache_write,
                            r.cost_usd,
                            r.cost_basis,
                            collected_at
                        ])?;
                        out.written += 1;
                    }
                    Merge::Refresh { cost, basis } => {
                        log(true)?;
                        refresh.execute(params![
                            r.host,
                            r.agent,
                            r.date,
                            r.model,
                            cost,
                            basis,
                            r.model_raw,
                            r.provider,
                            collected_at
                        ])?;
                        out.written += 1;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(out)
    }

    pub fn upsert_rows(&self, rows: &[Row], collected_at: &str) -> Result<(), StoreError> {
        let tx = self.con.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(UPSERT_SQL)?;
            for r in rows {
                stmt.execute(params![
                    r.host,
                    r.agent,
                    r.date,
                    r.model,
                    r.model_raw,
                    r.provider,
                    r.input,
                    r.output,
                    r.cache_read,
                    r.cache_write,
                    r.cost_usd,
                    r.cost_basis,
                    collected_at
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn log_run(
        &self,
        now: DateTime<Utc>,
        host: &str,
        agent: &str,
        status: &str,
        message: Option<&str>,
        rows: i64,
    ) -> Result<(), StoreError> {
        self.con.execute(
            "INSERT INTO runs (ts, host, agent, status, message, rows) VALUES (?, ?, ?, ?, ?, ?)",
            params![format_ts(now), host, agent, status, message, rows],
        )?;
        Ok(())
    }

    /// ISO do ok mais antigo entre os pares (host, agente).
    ///
    /// `None` se a lista é vazia ou se algum par nunca teve coleta ok: o total do mês
    /// ainda mistura dado que esta coleta não atualizou.
    pub fn oldest_ok_sync(&self, pairs: &[(String, String)]) -> Result<Option<String>, StoreError> {
        if pairs.is_empty() {
            return Ok(None);
        }
        let mut stmt = self
            .con
            .prepare("SELECT host, agent, MAX(ts) FROM runs WHERE status = 'ok' GROUP BY host, agent")?;
        let latest: HashMap<(String, String), String> = stmt
            .query_map([], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let mut oldest: Option<&String> = None;
        for pair in pairs {
            match latest.get(pair) {
                Some(ts) if !ts.is_empty() => oldest = Some(oldest.map_or(ts, |o| o.min(ts))),
                _ => return Ok(None),
            }
        }
        Ok(oldest.cloned())
    }

    pub fn month_spend(&self, month: &str) -> Result<f64, StoreError> {
        Ok(self.con.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM usage WHERE date LIKE ?",
            [format!("{month}%")],
            |r| r.get(0),
        )?)
    }

    /// O banco já tem alguma linha de (host, agente)? (`false` = fonte nunca vista)
    pub fn has_usage(&self, host: &str, agent: &str) -> Result<bool, StoreError> {
        let n: i64 = self.con.query_row(
            "SELECT EXISTS(SELECT 1 FROM usage WHERE host = ? AND agent = ?)",
            params![host, agent],
            |r| r.get(0),
        )?;
        Ok(n != 0)
    }

    /// Nomes canônicos distintos com uso gravado (para o `prices` resolver todos).
    pub fn distinct_models(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.con.prepare("SELECT DISTINCT model FROM usage ORDER BY model")?;
        let rows = stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Linhas sem preço: (linhas, modelos distintos, tokens).
    pub fn unpriced(&self) -> Result<(i64, i64, i64), StoreError> {
        Ok(self.con.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT model), \
             COALESCE(SUM(input + output + cache_read + cache_write), 0) \
             FROM usage WHERE cost_usd IS NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    }

    /// Total do mês de um ou mais provedores e quebra por harness: (rótulo, US$, tokens).
    ///
    /// Tokens entram porque modelo sem preço na coleta (ex.: Cursor) tem custo 0 e
    /// ainda assim consumiu.
    pub fn spend_by_provider(&self, providers: &[&str], month: &str) -> Result<ProviderSpend, StoreError> {
        let marks = vec!["?"; providers.len()].join(",");
        let sql = format!(
            "SELECT host || ' / ' || agent || ' / ' || provider, COALESCE(SUM(cost_usd), 0), \
             COALESCE(SUM(input + output + cache_read + cache_write), 0) FROM usage \
             WHERE provider IN ({marks}) AND date LIKE ? \
             GROUP BY host, agent, provider ORDER BY 2 DESC, 3 DESC, 1"
        );
        let like = format!("{month}%");
        let args = providers.iter().copied().chain(std::iter::once(like.as_str()));
        let mut stmt = self.con.prepare(&sql)?;
        let rows: Vec<(String, f64, i64)> = stmt
            .query_map(params_from_iter(args), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let total = rows.iter().fold(0.0, |acc, r| acc + r.1); // sum() de vazio daria -0.0
        Ok((total, rows))
    }
}

impl Store {
    /// Abre o banco só para leitura: sem migração, sem criar schema, sem criar o arquivo.
    pub fn open_readonly(path: &Path) -> Result<Self, StoreError> {
        if !path.is_file() {
            return Err(StoreError::Missing(path.to_path_buf()));
        }
        let con = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        con.busy_timeout(BUSY_TIMEOUT)?;
        Ok(Self { con })
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use tempfile::TempDir;

    use super::*;

    const RULES: &str = "[[rules]]\nmatch = \"^(claude|anthropic)\"\nprovider = \"anthropic\"\n";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap()
    }

    fn open(dir: &TempDir) -> Store {
        Store::open(&dir.path().join("t.sqlite"), RULES, now()).unwrap()
    }

    fn row(host: &str, agent: &str, date: &str, model: &str, provider: &str, cost: Option<f64>) -> Row {
        Row {
            host: host.into(),
            agent: agent.into(),
            date: date.into(),
            model: model.into(),
            model_raw: model.into(),
            provider: provider.into(),
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write: 4,
            cost_usd: cost,
            cost_basis: "estimated".into(),
        }
    }

    fn count(s: &Store, table: &str) -> i64 {
        s.connection()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn pair(h: &str, a: &str) -> (String, String) {
        (h.to_string(), a.to_string())
    }

    fn insert_run(s: &Store, ts: &str, agent: &str, status: &str) {
        s.connection()
            .execute(
                "INSERT INTO runs (ts, host, agent, status, message, rows) VALUES (?, 'mac', ?, ?, NULL, 1)",
                params![ts, agent, status],
            )
            .unwrap();
    }

    #[test]
    fn upsert_does_not_duplicate_on_second_collection() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        let rows = [row("mac", "claude", "2026-10-09", "a", "anthropic", Some(1.0))];
        s.upsert_rows(&rows, "t1").unwrap();
        s.upsert_rows(&rows, "t2").unwrap();
        assert_eq!(count(&s, "usage"), 1);
    }

    #[test]
    fn same_model_on_different_hosts_does_not_collide() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        let rows = [
            row("mac", "claude", "2026-10-09", "a", "anthropic", Some(1.0)),
            row("srv", "claude", "2026-10-09", "a", "anthropic", Some(1.0)),
        ];
        s.upsert_rows(&rows, "t1").unwrap();
        assert_eq!(count(&s, "usage"), 2);
    }

    #[test]
    fn upsert_updates_value_when_source_changes() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        let mut r = row("mac", "claude", "2026-10-09", "a", "anthropic", Some(1.0));
        s.upsert_rows(std::slice::from_ref(&r), "t1").unwrap();
        r.cost_usd = Some(9.9);
        s.upsert_rows(&[r], "t2").unwrap();
        assert!((s.month_spend("2026-10").unwrap() - 9.9).abs() < 1e-9);
        let at: String = s
            .connection()
            .query_row("SELECT collected_at FROM usage", [], |r| r.get(0))
            .unwrap();
        assert_eq!(at, "t2");
    }

    #[test]
    fn merge_nunca_encolhe_o_dia_e_guarda_historico() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        let full = row("mac", "zcode", "2026-10-09", "glm-5.3", "zhipu", Some(2.0));
        s.merge_rows(std::slice::from_ref(&full), "t1").unwrap();

        let mut small = full.clone();
        small.input = 0;
        small.output = 0;
        small.cache_read = 0;
        small.cache_write = 0;
        small.cost_usd = Some(0.1);
        let out = s.merge_rows(&[small], "t2").unwrap();
        assert_eq!((out.kept, out.written), (1, 0));

        let got: (i64, f64) = s
            .connection()
            .query_row("SELECT input, cost_usd FROM usage", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(got, (1, 2.0), "valor grande fica");
        let hist: (i64, f64) = s
            .connection()
            .query_row("SELECT kept, cost_usd FROM usage_history WHERE ts = 't2'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(hist, (0, 0.1), "tentativa fica registrada como ignorada");
    }

    fn cost_and_basis(s: &Store) -> (Option<f64>, String) {
        s.connection()
            .query_row("SELECT cost_usd, cost_basis FROM usage", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap()
    }

    #[test]
    fn merge_mantem_custo_com_os_mesmos_tokens() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        let mut r = row("mac", "codex", "2026-10-09", "gpt-5.6-luna", "openai", Some(1.0));
        s.merge_rows(std::slice::from_ref(&r), "t1").unwrap();
        // tabela de preços nova: o sync não reprecifica mês fechado (papel do `costs repricing`)
        r.cost_usd = Some(3.0);
        assert_eq!(s.merge_rows(std::slice::from_ref(&r), "t2").unwrap().unchanged, 1);
        // ruído de soma em ponto flutuante também não regrava
        r.cost_usd = Some(1.0 + 1e-13);
        assert_eq!(s.merge_rows(&[r], "t3").unwrap().unchanged, 1);
        assert_eq!(cost_and_basis(&s), (Some(1.0), "estimated".into()));
        assert_eq!(count(&s, "usage_history"), 0, "nada de ruído no histórico");
    }

    #[test]
    fn merge_preenche_custo_nulo_e_nunca_troca_conhecido_por_nulo() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        let mut r = row("mac", "kimi", "2026-10-09", "k3", "moonshot", None);
        s.merge_rows(std::slice::from_ref(&r), "t1").unwrap();
        r.cost_usd = Some(2.0);
        assert_eq!(s.merge_rows(std::slice::from_ref(&r), "t2").unwrap().written, 1);
        assert_eq!(cost_and_basis(&s).0, Some(2.0), "preço que faltava entra");
        r.cost_usd = None; // catálogo que falhou no dia
        assert_eq!(s.merge_rows(&[r], "t3").unwrap().unchanged, 1);
        assert_eq!(cost_and_basis(&s).0, Some(2.0), "custo conhecido não vira NULL");
    }

    #[test]
    fn merge_sobe_base_e_atualiza_metadados_sem_mexer_no_custo() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        let mut r = row("mac", "pi", "2026-10-09", "glm-5.3", "outro", Some(1.0));
        s.merge_rows(std::slice::from_ref(&r), "t1").unwrap();
        r.provider = "zhipu".into(); // regra nova no models.toml
        r.cost_usd = Some(9.0);
        assert_eq!(s.merge_rows(std::slice::from_ref(&r), "t2").unwrap().written, 1);
        let provider: String = s
            .connection()
            .query_row("SELECT provider FROM usage", [], |x| x.get(0))
            .unwrap();
        assert_eq!((provider.as_str(), cost_and_basis(&s).0), ("zhipu", Some(1.0)));
        r.cost_usd = Some(1.5);
        r.cost_basis = "reported".into(); // o agente passou a informar o custo
        assert_eq!(s.merge_rows(&[r], "t3").unwrap().written, 1);
        assert_eq!(cost_and_basis(&s), (Some(1.5), "reported".into()));
    }

    #[test]
    fn merge_registra_a_mesma_tentativa_de_encolher_uma_vez() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        let full = row("mac", "claude", "2026-10-09", "opus", "anthropic", Some(2.0));
        s.merge_rows(std::slice::from_ref(&full), "t1").unwrap();
        let mut small = full.clone();
        small.input = 0;
        small.cost_usd = Some(1.0);
        for ts in ["t2", "t3", "t4"] {
            assert_eq!(s.merge_rows(std::slice::from_ref(&small), ts).unwrap().kept, 1);
        }
        assert_eq!(
            count(&s, "usage_history"),
            1,
            "tentativa repetida não vira ruído a cada sync"
        );
        small.cache_read = 0; // encolheu de outro jeito: é uma tentativa nova
        s.merge_rows(&[small], "t5").unwrap();
        assert_eq!(count(&s, "usage_history"), 2);
    }

    #[test]
    fn frescor_por_par_usa_o_indice_de_runs() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        let plan: String = s
            .connection()
            .query_row(
                "EXPLAIN QUERY PLAN SELECT MAX(ts) FROM runs WHERE host = 'h' AND agent = 'a'",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("runs_pair_ts"), "{plan}");
    }

    #[test]
    fn merge_cresce_o_dia_quando_a_fonte_ganha_dados() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        let mut r = row("mac", "pi", "2026-10-09", "x", "outro", Some(1.0));
        s.merge_rows(std::slice::from_ref(&r), "t1").unwrap();
        r.input += 50;
        r.cost_usd = Some(2.5);
        let out = s.merge_rows(&[r], "t2").unwrap();
        assert_eq!((out.written, out.kept), (1, 0));
        let (inp, cost): (i64, f64) = s
            .connection()
            .query_row("SELECT input, cost_usd FROM usage", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((inp, cost), (51, 2.5));
    }

    #[test]
    fn replace_days_removes_model_that_left_the_source() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        s.upsert_rows(&[row("mac", "opencode", "2026-10-09", "a", "outro", Some(1.0))], "t0")
            .unwrap();
        s.upsert_rows(
            &[row("mac", "opencode", "2026-10-08", "keep", "outro", Some(2.0))],
            "t0",
        )
        .unwrap();
        s.replace_days("mac", "opencode", &HashSet::from(["2026-10-09".to_string()]), "t1")
            .unwrap();
        let hist: i64 = s
            .connection()
            .query_row("SELECT COUNT(*) FROM usage_history WHERE origin = 'replace'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(hist, 1, "valor apagado fica no histórico");
        s.upsert_rows(&[row("mac", "opencode", "2026-10-09", "*", "outro", Some(3.0))], "t1")
            .unwrap();
        let mut stmt = s
            .connection()
            .prepare("SELECT model, cost_usd FROM usage ORDER BY date")
            .unwrap();
        let got: Vec<(String, f64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(got, vec![("keep".to_string(), 2.0), ("*".to_string(), 3.0)]);
    }

    #[test]
    fn open_sets_version_and_busy_timeout_and_is_idempotent() {
        let d = TempDir::new().unwrap();
        drop(open(&d));
        let s = open(&d);
        let v: i64 = s
            .connection()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        let bt: i64 = s
            .connection()
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!((v, bt), (3, 5000));
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1, "v2 não gera backup");
    }

    #[test]
    fn v1_schema_is_migrated_without_losing_history() {
        let d = TempDir::new().unwrap();
        let db = d.path().join("t.sqlite");
        let raw = Connection::open(&db).unwrap();
        raw.execute_batch(
            "CREATE TABLE usage (host TEXT, agent TEXT, date TEXT, model TEXT, \
             PRIMARY KEY (host, agent, date, model));
             INSERT INTO usage VALUES ('mac', 'claude', '2026-01-01', 'claude-opus-4-6');
             INSERT INTO usage VALUES ('mac', 'codex', '2026-01-02', 'gpt-5.4');
             INSERT INTO usage VALUES ('mac', 'pi', '2026-01-03', 'zzz');",
        )
        .unwrap();
        drop(raw);
        let s = Store::open(&db, RULES, now()).unwrap();
        let mut stmt = s
            .connection()
            .prepare("SELECT model, model_raw, provider, input, cost_basis FROM usage ORDER BY date")
            .unwrap();
        let got: Vec<(String, String, String, i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let want = |m: &str, p: &str| (m.to_string(), m.to_string(), p.to_string(), 0, "estimated".to_string());
        assert_eq!(
            got,
            vec![
                want("claude-opus-4-6", "anthropic"),
                want("gpt-5.4", "openai"),
                want("zzz", "outro")
            ]
        );
        let v: i64 = s
            .connection()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 3);
        let bak = d.path().join("t.sqlite.v1-20261019T120000Z.bak");
        let bcon = Connection::open(&bak).unwrap();
        let n: i64 = bcon.query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            std::fs::read_dir(d.path())
                .unwrap()
                .filter(|e| { e.as_ref().unwrap().file_name().to_string_lossy().contains(".v1-") })
                .count(),
            1
        );
    }

    #[test]
    fn v1_migration_with_invalid_rules_fails_with_context() {
        let d = TempDir::new().unwrap();
        let db = d.path().join("t.sqlite");
        Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE usage (host TEXT, agent TEXT, date TEXT, model TEXT)")
            .unwrap();
        let err = Store::open(&db, "[[rules]\n", now()).err().unwrap();
        assert!(matches!(err, StoreError::Rules(_)));
    }

    #[test]
    fn oldest_ok_sync_is_the_most_delayed_agent() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        insert_run(&s, "2026-10-19T10:00:00+00:00", "claude", "ok");
        insert_run(&s, "2026-10-19T09:00:00+00:00", "claude", "ok");
        insert_run(&s, "2026-10-19T12:00:00+00:00", "codex", "ok");
        insert_run(&s, "2026-10-19T18:00:00+00:00", "pi", "error");
        assert_eq!(
            s.oldest_ok_sync(&[pair("mac", "claude"), pair("mac", "codex")])
                .unwrap()
                .as_deref(),
            Some("2026-10-19T10:00:00+00:00")
        );
        assert_eq!(
            s.oldest_ok_sync(&[pair("mac", "claude"), pair("mac", "pi")]).unwrap(),
            None
        );
        assert_eq!(s.oldest_ok_sync(&[]).unwrap(), None);
    }

    #[test]
    fn log_run_stores_iso_seconds_utc() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        s.log_run(now(), "mac", "claude", "ok", None, 5).unwrap();
        s.log_run(now(), "mac", "pi", "error", Some("boom"), 0).unwrap();
        let ts: String = s
            .connection()
            .query_row("SELECT ts FROM runs WHERE agent = 'claude'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ts, "2026-10-19T12:00:00+00:00");
        let msg: Option<String> = s
            .connection()
            .query_row("SELECT message FROM runs WHERE agent = 'pi'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msg.as_deref(), Some("boom"));
    }

    #[test]
    fn prune_runs_removes_old_executions_using_the_clock_argument() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        insert_run(&s, "2026-07-01T00:00:00+00:00", "claude", "ok");
        insert_run(&s, "2026-10-18T00:00:00+00:00", "claude", "ok");
        s.prune_runs(DEFAULT_PRUNE_DAYS, now()).unwrap();
        assert_eq!(count(&s, "runs"), 1);
        s.prune_runs(0, now()).unwrap();
        assert_eq!(count(&s, "runs"), 0);
    }

    #[test]
    fn month_spend_sums_only_the_month_and_defaults_to_zero() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        assert_eq!(s.month_spend("2026-10").unwrap(), 0.0);
        s.upsert_rows(
            &[
                row("mac", "claude", "2026-10-09", "a", "anthropic", Some(2.5)),
                row("mac", "claude", "2026-09-09", "a", "anthropic", Some(7.0)),
            ],
            "t",
        )
        .unwrap();
        assert_eq!(s.month_spend("2026-10").unwrap(), 2.5);
    }

    #[test]
    fn spend_by_provider_groups_by_host_and_harness_ordered_by_cost() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        s.upsert_rows(
            &[
                row("mac", "claude", "2026-10-09", "a", "anthropic", Some(1.0)),
                row("srv", "claude", "2026-10-10", "a", "anthropic", Some(2.0)),
                row("mac", "opencode", "2026-10-09", "a", "anthropic", Some(0.5)),
                row("mac", "cursor", "2026-10-09", "c", "cursor", None),
                row("mac", "claude", "2026-09-09", "a", "anthropic", Some(99.0)),
            ],
            "t",
        )
        .unwrap();
        let (total, rows) = s.spend_by_provider(&["anthropic", "cursor"], "2026-10").unwrap();
        assert_eq!(total, 3.5);
        assert_eq!(
            rows,
            vec![
                ("srv / claude / anthropic".to_string(), 2.0, 10),
                ("mac / claude / anthropic".to_string(), 1.0, 10),
                ("mac / opencode / anthropic".to_string(), 0.5, 10),
                ("mac / cursor / cursor".to_string(), 0.0, 10),
            ]
        );
        assert_eq!(s.spend_by_provider(&[], "2026-10").unwrap(), (0.0, vec![]));
    }

    #[test]
    fn utc_now_is_close_to_system_time() {
        assert!(format_ts(utc_now()).ends_with("+00:00"));
    }

    #[test]
    fn open_readonly_reads_but_never_writes_or_creates() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.sqlite");
        assert!(matches!(Store::open_readonly(&path), Err(StoreError::Missing(_))));
        assert!(!path.exists());
        drop(open(&dir));
        let ro = Store::open_readonly(&path).unwrap();
        assert_eq!(ro.month_spend("2026-10").unwrap(), 0.0);
        assert!(ro.connection().execute("DELETE FROM runs", []).is_err());
    }

    #[test]
    fn empty_provider_spend_is_positive_zero() {
        let dir = TempDir::new().unwrap();
        let (total, rows) = open(&dir).spend_by_provider(&["x"], "2026-10").unwrap();
        assert!(rows.is_empty() && total == 0.0 && total.is_sign_positive());
    }

    mod merge_props {
        use super::*;
        use proptest::prelude::*;

        fn basis() -> impl Strategy<Value = String> {
            prop::sample::select(vec!["free", "estimated", "fallback", "reported", "allocated"]).prop_map(String::from)
        }

        fn stored() -> impl Strategy<Value = Stored> {
            (
                0..1000i64,
                proptest::option::of(0.0..100.0f64),
                basis(),
                prop::bool::ANY,
            )
                .prop_map(|(t, c, b, p)| Stored {
                    tokens: t,
                    cost: c,
                    basis: b,
                    model_raw: "m".into(),
                    provider: if p { "x".into() } else { "y".into() },
                })
        }

        fn obs() -> impl Strategy<Value = Row> {
            (
                0..1000i64,
                proptest::option::of(0.0..100.0f64),
                basis(),
                prop::bool::ANY,
            )
                .prop_map(|(t, c, b, p)| {
                    let mut r = row("h", "a", "2026-10-09", "m", if p { "x" } else { "y" }, c);
                    (r.input, r.output, r.cache_read, r.cache_write) = (t, 0, 0, 0);
                    r.cost_basis = b;
                    r
                })
        }

        /// Observação com tokens perto dos gravados (iguais em 1/3 dos casos): com tokens
        /// independentes o caso "mesmos tokens" quase nunca seria sorteado.
        fn pair() -> impl Strategy<Value = (Stored, Row)> {
            (stored(), obs(), -1i64..=1).prop_map(|(old, mut r, delta)| {
                r.input = (old.tokens + delta).max(0);
                (old, r)
            })
        }

        proptest! {
            #[test]
            fn o_dia_nunca_encolhe_e_custo_conhecido_nunca_vira_nulo((old, r) in pair()) {
                let total = r.input;
                match decide_merge(Some(&old), &r) {
                    Merge::Write => prop_assert!(total > old.tokens),
                    Merge::Shrunk => prop_assert!(total < old.tokens),
                    Merge::Unchanged => prop_assert_eq!(total, old.tokens),
                    Merge::Refresh { cost, basis } => {
                        prop_assert_eq!(total, old.tokens);
                        prop_assert!(old.cost.is_none() || cost.is_some(), "Some -> None");
                        // a base só desce ao preencher um NULL (ex.: rota reconhecida como `free`)
                        prop_assert!(old.cost.is_none() || basis_rank(&basis) >= basis_rank(&old.basis));
                        // custo só muda para preencher NULL ou subir de base
                        if !cost_eq(cost, old.cost) {
                            prop_assert!(old.cost.is_none() || basis_rank(&basis) > basis_rank(&old.basis));
                        }
                    }
                }
            }

            #[test]
            fn merge_e_idempotente((old, r) in pair()) {
                // aplicar a decisão e reobservar a mesma linha não muda mais nada
                let after = match decide_merge(Some(&old), &r) {
                    Merge::Write => Stored { tokens: r.input, cost: r.cost_usd, basis: r.cost_basis.clone(),
                                             model_raw: r.model_raw.clone(), provider: r.provider.clone() },
                    Merge::Unchanged | Merge::Shrunk => old.clone(),
                    Merge::Refresh { cost, basis } => Stored { cost, basis, model_raw: r.model_raw.clone(),
                                                              provider: r.provider.clone(), ..old.clone() },
                };
                let again = decide_merge(Some(&after), &r);
                prop_assert!(matches!(again, Merge::Unchanged | Merge::Shrunk), "{:?}", again);
            }
        }
    }
}
