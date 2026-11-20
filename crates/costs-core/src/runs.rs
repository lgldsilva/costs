//! Comando `runs`: histórico de coletas da tabela `runs`.

use rusqlite::{Connection, params_from_iter, types::Value};

/// Filtros do `costs runs`.
#[derive(Debug, Clone, Default)]
pub struct RunsFilter {
    pub limit: i64,
    pub errors: bool,
    pub host: Option<String>,
    pub agent: Option<String>,
}

/// Histórico de coletas, mais recente primeiro.
pub fn runs(con: &Connection, f: &RunsFilter) -> Result<String, rusqlite::Error> {
    let mut sql = String::from("SELECT ts, host, agent, status, rows, COALESCE(message, '') FROM runs");
    let mut clauses: Vec<&str> = Vec::new();
    let mut params: Vec<Value> = Vec::new();
    if f.errors {
        clauses.push("status != 'ok'");
    }
    if let Some(h) = &f.host {
        clauses.push("host = ?");
        params.push(Value::Text(h.clone()));
    }
    if let Some(a) = &f.agent {
        clauses.push("agent = ?");
        params.push(Value::Text(a.clone()));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY ts DESC, rowid DESC LIMIT ?");
    params.push(Value::Integer(f.limit));

    let mut stmt = con.prepare(&sql)?;
    let rows: Vec<(String, String, String, String, i64, String)> = stmt
        .query_map(params_from_iter(params), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                r.get(5)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    if rows.is_empty() {
        let suffix = if f.errors { " com erro" } else { "" };
        return Ok(format!("sem coletas registradas{suffix}\n"));
    }
    let mut out = format!(
        "{:<22} {:<14} {:<10} {:<7} {:>6}  mensagem\n{}\n",
        "quando",
        "host",
        "agente",
        "status",
        "linhas",
        "-".repeat(100)
    );
    for (ts, host, agent, status, count, message) in rows {
        out.push_str(&format!(
            "{ts:<22} {host:<14} {agent:<10} {status:<7} {count:>6}  {message}\n"
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use tempfile::TempDir;

    use super::*;
    use crate::store::Store;

    fn seeded(dir: &TempDir) -> Store {
        let now = Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap();
        let s = Store::open(&dir.path().join("t.sqlite"), "", now).unwrap();
        let ts = |h| Utc.with_ymd_and_hms(2026, 10, 19, h, 0, 0).unwrap();
        s.log_run(ts(8), "mac", "claude", "ok", None, 3).unwrap();
        s.log_run(ts(9), "pi", "codex", "error", Some("ssh caiu"), 0).unwrap();
        s.log_run(ts(9), "mac", "codex", "ok", None, 5).unwrap(); // mesmo ts: rowid decide
        s
    }

    fn f(limit: i64) -> RunsFilter {
        RunsFilter {
            limit,
            ..Default::default()
        }
    }

    #[test]
    fn ordena_por_ts_e_rowid_desc_e_formata() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        let out = runs(s.connection(), &f(20)).unwrap();
        let mut lines = out.lines();
        assert_eq!(
            lines.next().unwrap(),
            format!(
                "{:<22} {:<14} {:<10} {:<7} {:>6}  mensagem",
                "quando", "host", "agente", "status", "linhas"
            )
        );
        assert_eq!(lines.next().unwrap(), "-".repeat(100));
        let want = [
            ("2026-10-19T09:00:00+00:00", "mac", "codex", "ok", 5, ""),
            ("2026-10-19T09:00:00+00:00", "pi", "codex", "error", 0, "ssh caiu"),
            ("2026-10-19T08:00:00+00:00", "mac", "claude", "ok", 3, ""),
        ];
        for (l, (ts, h, a, st, n, m)) in lines.zip(want) {
            assert_eq!(l, format!("{ts:<22} {h:<14} {a:<10} {st:<7} {n:>6}  {m}"));
        }
    }

    #[test]
    fn limit_corta_no_mais_recente() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        let out = runs(s.connection(), &f(1)).unwrap();
        assert_eq!(out.lines().count(), 3);
        assert!(out.contains("codex") && !out.contains("claude"));
    }

    #[test]
    fn filtros_de_erro_host_e_agente() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        let errs = runs(s.connection(), &RunsFilter { errors: true, ..f(20) }).unwrap();
        assert!(errs.contains("ssh caiu") && !errs.contains(" ok "));
        let host = runs(
            s.connection(),
            &RunsFilter {
                host: Some("mac".into()),
                ..f(20)
            },
        )
        .unwrap();
        assert_eq!(host.lines().count(), 4);
        let agent = runs(
            s.connection(),
            &RunsFilter {
                agent: Some("claude".into()),
                host: Some("mac".into()),
                ..f(20)
            },
        )
        .unwrap();
        assert_eq!(agent.lines().count(), 3);
    }

    #[test]
    fn vazio_avisa() {
        let d = TempDir::new().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap();
        let s = Store::open(&d.path().join("e.sqlite"), "", now).unwrap();
        assert_eq!(runs(s.connection(), &f(5)).unwrap(), "sem coletas registradas\n");
        let e = RunsFilter { errors: true, ..f(5) };
        assert_eq!(runs(s.connection(), &e).unwrap(), "sem coletas registradas com erro\n");
    }
}
