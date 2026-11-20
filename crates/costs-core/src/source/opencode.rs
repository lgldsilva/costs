//! Parser do opencode (sqlite `opencode.db` + JSON legado), equivalente a `ccusage opencode daily`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::model::Row;
use crate::prices::Usage;
use crate::source::{Aggregator, Cost, LocalDate, PriceTable, SourceError, int, io_err, sql_err, walk_files};

const AGENT: &str = "opencode";
const PREFERRED_DB: &str = "opencode.db";

/// O store só cresce (`merge_rows` nunca encolhe um dia), então reler o banco inteiro a
/// cada coleta só gasta disco: num HDD dividido com o `opencode serve`, a varredura
/// travava o event loop dele além dos 45s do heartbeat e o cliente reiniciava o serviço
/// em loop. 60 dias cobrem o mês corrente e o anterior com folga.
const DEFAULT_LOOKBACK_DAYS: i64 = 60;
/// `0` desliga a janela (histórico inteiro: carga inicial de um host, validação).
const LOOKBACK_ENV: &str = "COSTS_OPENCODE_LOOKBACK_DAYS";
const DAY_MS: i64 = 86_400_000;

fn lookback_days() -> i64 {
    std::env::var(LOOKBACK_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_LOOKBACK_DAYS)
}

/// Janela da coleta em dias locais inteiros: um dia cortado no meio voltaria menor que o
/// gravado e cada sync registraria a tentativa no `usage_history`.
#[derive(Clone, Copy)]
struct Window {
    first_day: NaiveDate,
    /// Corte do sqlite, um dia antes: o fuso local nunca desloca mais que isso.
    since_ms: i64,
}

impl Window {
    fn new(days: i64, now: DateTime<Utc>, local_date: LocalDate) -> Option<Self> {
        (days > 0).then(|| {
            let start = now - Duration::days(days);
            Self {
                first_day: local_date(start),
                since_ms: start.timestamp_millis() - DAY_MS,
            }
        })
    }
}

/// Um registro de uso já normalizado.
struct Entry {
    id: String,
    ts_ms: i64,
    model: String,
    /// [entrada, saída, cache leitura, cache escrita] exibidos.
    tokens: [i64; 4],
    /// Saída cobrada: reasoning e resíduo de `total` contam como saída.
    billed_output: i64,
    cost: f64,
}

/// Campos de uso de uma mensagem, extraídos do JSON (no sqlite ou do arquivo legado).
struct Fields<'a> {
    model: Option<&'a str>,
    tokens: &'a Value,
    ts_ms: Option<i64>,
    cost: Option<f64>,
}

impl<'a> Fields<'a> {
    /// Mensagem legada completa (`modelID` no topo).
    fn from_doc(data: &'a Value) -> Self {
        Self {
            model: data.get("modelID").and_then(Value::as_str),
            tokens: data.get("tokens").unwrap_or(&Value::Null),
            ts_ms: data.get("time").and_then(|t| t.get("created")).and_then(Value::as_i64),
            cost: data.get("cost").and_then(Value::as_f64),
        }
    }

    /// `[modelo, tokens, criado, custo]` devolvido pelo `json_extract` de `read_table`.
    fn from_extract(arr: &'a Value) -> Self {
        Self {
            model: arr.get(0).and_then(Value::as_str),
            tokens: arr.get(1).unwrap_or(&Value::Null),
            ts_ms: arr.get(2).and_then(Value::as_i64),
            cost: arr.get(3).and_then(Value::as_f64),
        }
    }
}

fn parse(id: &str, f: &Fields) -> Option<Entry> {
    let model = f.model?;
    if model.is_empty() {
        return None;
    }
    let tokens = f.tokens;
    let cache = tokens.get("cache")?;
    let (input, output, reasoning, total) = (
        int(tokens.get("input")),
        int(tokens.get("output")),
        int(tokens.get("reasoning")),
        int(tokens.get("total")),
    );
    let (cr, cw) = (int(cache.get("read")), int(cache.get("write")));
    if input + output + reasoning + total + cr + cw == 0 {
        return None;
    }
    let ts_ms = f.ts_ms?;
    let residual = total - input - cr - cw;
    Some(Entry {
        id: id.to_string(),
        ts_ms,
        model: model.to_string(),
        tokens: [input, output, cr, cw],
        billed_output: (output + reasoning).max(residual),
        cost: f.cost.unwrap_or(0.0),
    })
}

fn has_table(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |_| Ok(()),
    )
    .map(|()| true)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(false),
        other => Err(other),
    })
}

/// Só os campos de uso saem do sqlite: o `data` do formato v2 carrega a mensagem inteira
/// (~11 KB cada, centenas de MB num banco grande) e desserializá-lo todo a cada coleta
/// dominava o tempo. `json_valid` mantém o comportamento de pular linhas corrompidas
/// (o sqlite reaproveita o parse entre as duas funções). `since_ms` usa a coluna
/// `time_created` (indexada em `session_message`); `table` é sempre literal do código.
fn read_table(
    conn: &Connection,
    table: &str,
    model_path: &str,
    assistant_only: bool,
    since_ms: Option<i64>,
) -> rusqlite::Result<Vec<Entry>> {
    let mut conds = Vec::new();
    if since_ms.is_some() {
        conds.push("time_created >= ?1");
    }
    if assistant_only {
        conds.push("type = 'assistant'");
    }
    conds.push("json_valid(data)");
    let sql = format!(
        "SELECT id, json_extract(data, '{model_path}', '$.tokens', '$.time.created', '$.cost') \
         FROM {table} WHERE {}",
        conds.join(" AND ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(since_ms), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, fields) = row?;
        if let Some(e) = serde_json::from_str::<Value>(&fields)
            .ok()
            .and_then(|arr| parse(&id, &Fields::from_extract(&arr)))
        {
            out.push(e);
        }
    }
    Ok(out)
}

/// Schemas antigos do opencode não tinham `time_created`: sem ela, só o pós-filtro vale.
fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn read_db(path: &Path, window: Option<Window>) -> Result<Vec<Entry>, SourceError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(sql_err(path))?;
    // v2 (`session_message`): `model.id` em vez de `modelID`, e só as respostas têm uso.
    let tables = [("message", "$.modelID", false), ("session_message", "$.model.id", true)];
    let mut out = Vec::new();
    for (table, model_path, assistant_only) in tables {
        if !has_table(&conn, table).map_err(sql_err(path))? {
            continue;
        }
        let since = match window {
            Some(w) if has_column(&conn, table, "time_created").map_err(sql_err(path))? => Some(w.since_ms),
            _ => None,
        };
        out.extend(read_table(&conn, table, model_path, assistant_only, since).map_err(sql_err(path))?);
    }
    Ok(out)
}

fn read_json_files(root: &Path) -> Result<Vec<Entry>, SourceError> {
    let mut out = Vec::new();
    for file in walk_files(&root.join("storage").join("message"), &|n| n.ends_with(".json"), None)? {
        let bytes = fs::read(&file).map_err(io_err(&file))?;
        let Ok(data) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let stem = file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let id = data.get("id").and_then(Value::as_str).map_or(stem, str::to_string);
        out.extend(parse(&id, &Fields::from_doc(&data)));
    }
    Ok(out)
}

/// `opencode.db` tem preferência; senão o primeiro `opencode-*.db` (ordem alfabética).
fn pick_db(root: &Path) -> Result<Option<PathBuf>, SourceError> {
    let preferred = root.join(PREFERRED_DB);
    if preferred.is_file() {
        return Ok(Some(preferred));
    }
    let dbs = walk_files(
        root,
        &|n| n.starts_with("opencode-") && n.ends_with(".db"),
        Some("storage"),
    )?;
    Ok(dbs.into_iter().find(|p| p.parent() == Some(root)))
}

/// Linhas diárias por modelo dos últimos `COSTS_OPENCODE_LOOKBACK_DAYS` (padrão 60).
/// `root` é o diretório de dados do opencode ou um arquivo `.db`.
pub fn daily_rows(
    root: &Path,
    host: &str,
    local_date: LocalDate,
    prices: Option<&PriceTable>,
) -> Result<Vec<Row>, SourceError> {
    let window = Window::new(lookback_days(), Utc::now(), local_date);
    rows_in_window(root, host, local_date, prices, window)
}

/// `window` `None`: histórico inteiro.
fn rows_in_window(
    root: &Path,
    host: &str,
    local_date: LocalDate,
    prices: Option<&PriceTable>,
    window: Option<Window>,
) -> Result<Vec<Row>, SourceError> {
    let (db, dir) = if root.is_file() {
        (Some(root.to_path_buf()), root.parent().unwrap_or(root))
    } else {
        (pick_db(root)?, root)
    };
    let mut entries = match db {
        Some(p) => read_db(&p, window)?,
        None => Vec::new(),
    };
    if !root.is_file() {
        entries.extend(read_json_files(dir)?);
    }
    let mut seen = HashSet::new();
    let mut agg = Aggregator::new(prices);
    for e in entries {
        if !seen.insert(e.id.clone()) {
            continue;
        }
        let Some(ts) = DateTime::from_timestamp_millis(e.ts_ms) else {
            continue;
        };
        let day = local_date(ts);
        if window.is_some_and(|w| day < w.first_day) {
            continue;
        }
        let [input, _, cache_read, cache_write] = e.tokens;
        let cost = if e.cost > 0.0 {
            Cost::Stored(e.cost)
        } else {
            Cost::Priced(Usage {
                input: input as f64,
                output: e.billed_output as f64,
                cache_read: cache_read as f64,
                cache_write: cache_write as f64,
            })
        };
        agg.add(day, &e.model, e.tokens, cost);
    }
    Ok(agg.into_rows(host, AGENT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::testutil::{home, table, utc_date, validate};

    const T0: i64 = 1_767_261_600_000; // 2026-01-01T10:00:00Z

    fn msg(model: &str, tokens: &str, extra: &str) -> String {
        format!(r#"{{"modelID":"{model}","time":{{"created":{T0}}},"tokens":{tokens}{extra}}}"#)
    }

    fn msg_at(model: &str, tokens: &str, created: i64) -> String {
        format!(r#"{{"modelID":"{model}","time":{{"created":{created}}},"tokens":{tokens}}}"#)
    }

    /// Schema real do opencode (`time_created` em ms); `created` só alimenta a coluna.
    fn make_db_at(dir: &Path, name: &str, rows: &[(&str, i64, String)]) -> PathBuf {
        let p = dir.join(name);
        let c = Connection::open(&p).unwrap();
        c.execute(
            "CREATE TABLE IF NOT EXISTS message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, \
             time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL)",
            [],
        )
        .unwrap();
        for (id, created, data) in rows {
            c.execute(
                "INSERT INTO message VALUES (?1,'s',?2,?2,?3)",
                rusqlite::params![id, created, data],
            )
            .unwrap();
        }
        p
    }

    fn make_db(dir: &Path, name: &str, rows: &[(&str, String)]) -> PathBuf {
        let rows: Vec<_> = rows.iter().map(|(id, data)| (*id, T0, data.clone())).collect();
        make_db_at(dir, name, &rows)
    }

    /// Histórico inteiro: as fixtures são de 2026-01-01, fora de qualquer janela atual.
    fn all(root: &Path, prices: Option<&PriceTable>) -> Result<Vec<Row>, SourceError> {
        rows_in_window(root, "h", utc_date, prices, None)
    }

    #[test]
    fn exibe_so_output_cobra_reasoning_e_residuo() {
        let dir = tempfile::tempdir().unwrap();
        let t = r#"{"input":1000000,"output":0,"reasoning":1000000,"total":3000000,"cache":{"read":0,"write":0}}"#;
        make_db(dir.path(), "opencode.db", &[("a", msg("m", t, ""))]);
        let rows = all(dir.path(), Some(&table())).unwrap();
        assert_eq!((rows[0].input, rows[0].output), (1_000_000, 0));
        // saida cobrada = max(0+1M, 3M-1M) = 2M => 1.0 + 4.0
        assert!((rows[0].cost_usd.unwrap() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn custo_positivo_vale_e_zero_cai_na_tabela() {
        let dir = tempfile::tempdir().unwrap();
        let t = r#"{"input":1000000,"output":0,"cache":{"read":0,"write":0}}"#;
        make_db(
            dir.path(),
            "opencode.db",
            &[("a", msg("m", t, r#","cost":7.5"#)), ("b", msg("m", t, r#","cost":0"#))],
        );
        let rows = all(dir.path(), Some(&table())).unwrap();
        assert!((rows[0].cost_usd.unwrap() - 8.5).abs() < 1e-9);
    }

    #[test]
    fn dedupe_por_id_entre_db_e_json_e_prefere_opencode_db() {
        let dir = tempfile::tempdir().unwrap();
        let t = r#"{"input":10,"cache":{"read":1,"write":2}}"#;
        make_db(dir.path(), "opencode.db", &[("a", msg("m", t, ""))]);
        make_db(dir.path(), "opencode-next.db", &[("z", msg("m", t, ""))]);
        let sdir = dir.path().join("storage/message/s1");
        fs::create_dir_all(&sdir).unwrap();
        fs::write(sdir.join("a.json"), msg("m", t, "")).unwrap();
        fs::write(sdir.join("b.json"), msg("m", t, "")).unwrap();
        fs::write(sdir.join("c.json"), "{ruim").unwrap();
        let rows = all(dir.path(), None).unwrap();
        assert_eq!((rows[0].input, rows[0].cache_read, rows[0].cache_write), (20, 2, 4));
    }

    #[test]
    fn ignora_registros_invalidos_e_le_next_db_sozinho_e_v2() {
        let dir = tempfile::tempdir().unwrap();
        let no_cache = msg("m", r#"{"input":5}"#, "");
        let zero = msg("m", r#"{"cache":{}}"#, "");
        let bad_json = "nao json".to_string();
        let p = make_db(
            dir.path(),
            "opencode-next.db",
            &[("a", no_cache), ("b", zero), ("c", bad_json)],
        );
        let c = Connection::open(&p).unwrap();
        c.execute(
            "CREATE TABLE session_message(id TEXT, session_id TEXT, type TEXT, data TEXT)",
            [],
        )
        .unwrap();
        let d = format!(
            r#"{{"model":{{"id":"v2m"}},"time":{{"created":{T0}}},"tokens":{{"input":3,"output":4,"cache":{{"read":0,"write":0}}}}}}"#
        );
        for (id, ty, data) in [
            ("v1", "assistant", d.as_str()),
            ("v2", "user", d.as_str()),
            ("v3", "assistant", "{ruim"),
        ] {
            c.execute("INSERT INTO session_message VALUES (?1,'s',?2,?3)", [id, ty, data])
                .unwrap();
        }
        drop(c);
        let rows = all(dir.path(), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].model.as_str(), rows[0].input, rows[0].output), ("v2m", 3, 4));
        // db direto
        assert_eq!(all(&p, None).unwrap().len(), 1);
    }

    #[test]
    fn sem_dados_e_db_corrompido() {
        let dir = tempfile::tempdir().unwrap();
        assert!(all(dir.path(), None).unwrap().is_empty());
        fs::write(dir.path().join("opencode.db"), "lixo lixo lixo lixo lixo").unwrap();
        assert!(all(dir.path(), None).is_err());
    }

    #[test]
    fn janela_le_dias_locais_inteiros_e_corta_no_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let t = r#"{"input":10,"cache":{"read":0,"write":0}}"#;
        let now = DateTime::from_timestamp_millis(T0).unwrap(); // 2026-01-01T10:00Z
        let at = |ms: i64, tok: i64| msg_at("m", &t.replace("10", &tok.to_string()), ms);
        let day_start = 1_767_052_800_000; // 2025-12-30T00:00Z, primeiro dia da janela de 2
        make_db_at(
            dir.path(),
            "opencode.db",
            &[
                // 00:30 do primeiro dia: antes de now-2d, mas o dia entra inteiro.
                ("inicio", day_start + 1_800_000, at(day_start + 1_800_000, 1)),
                // 23:00 da véspera: passa no corte do sqlite (folga de 1 dia), cai no pós-filtro.
                ("vespera", day_start - 3_600_000, at(day_start - 3_600_000, 100)),
                // Coluna antiga com JSON recente: só o corte do sqlite a exclui.
                ("coluna", T0 - 90 * DAY_MS, at(T0, 10_000)),
            ],
        );
        let w = Window::new(2, now, utc_date);
        let rows = rows_in_window(dir.path(), "h", utc_date, None, w).unwrap();
        let got: Vec<_> = rows.iter().map(|r| (r.date.as_str(), r.input)).collect();
        assert_eq!(got, [("2025-12-30", 1)]);

        assert!(Window::new(0, now, utc_date).is_none(), "0 desliga a janela");
        let total: i64 = all(dir.path(), None).unwrap().iter().map(|r| r.input).sum();
        assert_eq!(total, 10_101, "sem janela, o histórico inteiro volta");
    }

    #[test]
    fn janela_sem_coluna_de_tempo_filtra_pelo_json() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("opencode.db");
        let c = Connection::open(&p).unwrap();
        c.execute("CREATE TABLE message(id TEXT PRIMARY KEY, data TEXT)", [])
            .unwrap();
        let t = r#"{"input":10,"cache":{"read":0,"write":0}}"#;
        for (id, ms) in [("novo", T0), ("velho", T0 - 90 * DAY_MS)] {
            c.execute("INSERT INTO message VALUES (?1, ?2)", [id, &msg_at("m", t, ms)])
                .unwrap();
        }
        drop(c);
        let now = DateTime::from_timestamp_millis(T0).unwrap();
        let rows = rows_in_window(dir.path(), "h", utc_date, None, Window::new(60, now, utc_date)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].date, "2026-01-01");
    }

    #[test]
    fn valida_contra_ccusage() {
        let root =
            std::env::var_os("OPENCODE_DATA_DIR").map_or_else(|| home().join(".local/share/opencode"), Into::into);
        let rows = rows_in_window(&root, "h", crate::source::local_date, None, None).unwrap();
        validate(AGENT, &rows);
    }
}
