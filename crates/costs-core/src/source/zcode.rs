//! Parser do zcode (`<ZCODE_HOME>/cli/db/db.sqlite`), equivalente a `ccusage zcode daily`.
//!
//! `input_tokens` do zcode inclui o cache: ele é subtraído (com limites) para exibir/cobrar.

use std::collections::HashSet;
use std::path::Path;

use chrono::DateTime;
use rusqlite::{Connection, OpenFlags};

use crate::model::Row;
use crate::prices::Usage;
use crate::source::{Aggregator, Cost, LocalDate, PriceTable, SourceError, split_input, sql_err};

const AGENT: &str = "zcode";
const DB_RELATIVE: [&str; 3] = ["cli", "db", "db.sqlite"];

fn columns(conn: &Connection) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('model_usage')")?;
    stmt.query_map([], |r| r.get(0))?.collect()
}

/// Linhas diárias por modelo. `home` é o `ZCODE_HOME` (ex.: `~/.zcode`).
pub fn daily_rows(
    home: &Path,
    host: &str,
    local_date: LocalDate,
    prices: Option<&PriceTable>,
) -> Result<Vec<Row>, SourceError> {
    let db: std::path::PathBuf = DB_RELATIVE.iter().fold(home.to_path_buf(), |p, s| p.join(s));
    if !db.is_file() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(sql_err(&db))?;
    let cols = columns(&conn).map_err(sql_err(&db))?;
    let opt = |c: &str| {
        if cols.contains(c) {
            format!("COALESCE({c},0)")
        } else {
            "0".to_string()
        }
    };
    let sql = format!(
        "SELECT started_at, model_id, COALESCE(input_tokens,0), COALESCE(output_tokens,0), {}, {} \
         FROM model_usage WHERE status='completed' AND model_id IS NOT NULL AND model_id <> ''",
        opt("cache_read_input_tokens"),
        opt("cache_creation_input_tokens"),
    );
    let mut stmt = conn.prepare(&sql).map_err(sql_err(&db))?;
    let mut rows = stmt.query([]).map_err(sql_err(&db))?;
    let mut agg = Aggregator::new(prices);
    while let Some(r) = rows.next().map_err(sql_err(&db))? {
        let get = |i: usize| r.get::<_, i64>(i).map(|v| v.max(0)).map_err(sql_err(&db));
        let (input, output) = (get(2)?, get(3)?);
        let (cache_read, cache_creation) = (get(4)?, get(5)?);
        if input + output + cache_read + cache_creation == 0 {
            continue;
        }
        let Some(ts) = DateTime::from_timestamp_millis(get(0)?) else {
            continue;
        };
        let model: String = r.get(1).map_err(sql_err(&db))?;
        let (shown, cr, cw) = split_input(input, cache_read, cache_creation);
        let usage = Usage {
            input: shown as f64,
            output: output as f64,
            cache_read: cr as f64,
            cache_write: cw as f64,
        };
        agg.add(local_date(ts), &model, [shown, output, cr, cw], Cost::Priced(usage));
    }
    Ok(agg.into_rows(host, AGENT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::testutil::{home, table, utc_date, validate};
    use std::fs;

    const T0: i64 = 1_767_261_600_000;

    fn make(home: &Path, with_cache_cols: bool) -> Connection {
        let dir = home.join("cli/db");
        fs::create_dir_all(&dir).unwrap();
        let c = Connection::open(dir.join("db.sqlite")).unwrap();
        let cache = if with_cache_cols {
            ", cache_read_input_tokens INT, cache_creation_input_tokens INT"
        } else {
            ""
        };
        c.execute(&format!("CREATE TABLE model_usage(id TEXT PRIMARY KEY, status TEXT, started_at INT, model_id TEXT, input_tokens INT, output_tokens INT{cache})"), []).unwrap();
        c
    }

    fn ins(c: &Connection, (id, status, model): (&str, &str, &str), (i, o, cr, cw): (i64, i64, i64, i64)) {
        c.execute(
            "INSERT INTO model_usage(id,status,started_at,model_id,input_tokens,output_tokens,cache_read_input_tokens,cache_creation_input_tokens) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![id, status, T0, model, i, o, cr, cw],
        )
        .unwrap();
    }

    #[test]
    fn subtrai_cache_do_input_com_limites() {
        let dir = tempfile::tempdir().unwrap();
        let c = make(dir.path(), true);
        ins(&c, ("a", "completed", "m"), (100, 9, 50, 7));
        ins(&c, ("b", "completed", "m"), (10, 0, 50, 0));
        let rows = daily_rows(dir.path(), "h", utc_date, None).unwrap();
        assert_eq!(
            (rows[0].input, rows[0].output, rows[0].cache_read, rows[0].cache_write),
            (43, 9, 60, 7)
        );
        assert_eq!(rows[0].cost_usd, None);
    }

    #[test]
    fn so_completed_ignora_vazios_e_calcula_custo() {
        let dir = tempfile::tempdir().unwrap();
        let c = make(dir.path(), true);
        ins(&c, ("a", "completed", "m"), (1_000_000, 0, 0, 0));
        ins(&c, ("b", "error", "m"), (5, 5, 0, 0));
        ins(&c, ("c", "completed", ""), (5, 5, 0, 0));
        ins(&c, ("d", "completed", "m"), (0, 0, 0, 0));
        let rows = daily_rows(dir.path(), "h", utc_date, Some(&table())).unwrap();
        assert_eq!(rows.len(), 1);
        assert!((rows[0].cost_usd.unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn esquema_sem_colunas_de_cache_e_db_ausente() {
        let dir = tempfile::tempdir().unwrap();
        assert!(daily_rows(dir.path(), "h", utc_date, None).unwrap().is_empty());
        let c = make(dir.path(), false);
        c.execute("INSERT INTO model_usage VALUES ('a','completed',?1,'m',8,2)", [T0])
            .unwrap();
        let rows = daily_rows(dir.path(), "h", utc_date, None).unwrap();
        assert_eq!((rows[0].input, rows[0].cache_read), (8, 0));
    }

    #[test]
    fn db_corrompido_da_erro() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("cli/db")).unwrap();
        fs::write(dir.path().join("cli/db/db.sqlite"), "lixo lixo lixo lixo lixo lixo").unwrap();
        assert!(daily_rows(dir.path(), "h", utc_date, None).is_err());
    }

    #[test]
    fn valida_contra_ccusage() {
        let root = std::env::var_os("ZCODE_HOME").map_or_else(|| home().join(".zcode"), Into::into);
        let rows = daily_rows(&root, "h", crate::source::local_date, None).unwrap();
        validate(AGENT, &rows);
    }
}
