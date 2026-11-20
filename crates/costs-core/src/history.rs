//! Comando `history`: log append-only do que cada coleta viu (`usage_history`).

use rusqlite::{Connection, params_from_iter, types::Value};

/// Filtros do `costs history`.
#[derive(Debug, Clone, Default)]
pub struct HistoryFilter {
    pub limit: i64,
    pub model: Option<String>,
}

const KEPT_LABEL: [&str; 2] = ["ignorada", "aplicada"];

/// Observações mais recentes primeiro: quando, origem, veredito, chave e valores.
pub fn history(con: &Connection, f: &HistoryFilter) -> Result<String, rusqlite::Error> {
    let mut sql = String::from(
        "SELECT ts, origin, kept, host, agent, date, model, input, output, cache_read, cache_write, \
         COALESCE(cost_usd, -1) FROM usage_history",
    );
    let mut params: Vec<Value> = Vec::new();
    if let Some(m) = &f.model {
        sql.push_str(" WHERE model = ?");
        params.push(Value::Text(m.clone()));
    }
    sql.push_str(" ORDER BY ts DESC, rowid DESC LIMIT ?");
    params.push(Value::Integer(f.limit));

    let mut stmt = con.prepare(&sql)?;
    type Hit = (
        String,
        String,
        i64,
        String,
        String,
        String,
        String,
        i64,
        i64,
        i64,
        i64,
        f64,
    );
    let rows: Vec<Hit> = stmt
        .query_map(params_from_iter(params), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                r.get::<_, Option<i64>>(9)?.unwrap_or(0),
                r.get::<_, Option<i64>>(10)?.unwrap_or(0),
                r.get(11)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    if rows.is_empty() {
        return Ok("sem histórico de observações (rode `costs sync` antes)\n".to_string());
    }
    let mut out = format!(
        "{:<20} {:<10} {:<10} {:<12} {:<10} {:<11} {:<28} {:>12} {:>10}\n{}\n",
        "quando",
        "origem",
        "veredito",
        "host",
        "agente",
        "data",
        "modelo",
        "tokens",
        "US$",
        "-".repeat(128)
    );
    for (ts, origin, kept, host, agent, date, model, i, o, cr, cw, cost) in rows {
        let tokens = i + o + cr + cw;
        let money = if cost < 0.0 {
            "-".to_string()
        } else {
            format!("{cost:.4}")
        };
        out.push_str(&format!(
            "{:<20} {:<10} {:<10} {host:<12} {agent:<10} {date:<11} {model:<28} {tokens:>12} {money:>10}\n",
            ts.get(..19).unwrap_or(&ts),
            origin,
            KEPT_LABEL[kept.clamp(0, 1) as usize],
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use tempfile::TempDir;

    use super::*;
    use crate::model::Row;
    use crate::store::Store;

    fn row(cost: f64) -> Row {
        Row {
            host: "mac".into(),
            agent: "zcode".into(),
            date: "2026-10-01".into(),
            model: "glm-5.3".into(),
            model_raw: "glm-5.3".into(),
            provider: "zhipu".into(),
            input: 100,
            output: 10,
            cache_read: 0,
            cache_write: 0,
            cost_usd: Some(cost),
            cost_basis: "estimated".into(),
        }
    }

    #[test]
    fn mostra_observacoes_ignoradas_e_aplicadas() {
        let d = TempDir::new().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap();
        let s = Store::open(&d.path().join("t.sqlite"), "", now).unwrap();
        s.merge_rows(&[row(2.0)], "2026-10-19T12:00:00+00:00").unwrap();
        let mut smaller = row(0.1);
        smaller.input = 1;
        s.merge_rows(&[smaller], "2026-10-19T13:00:00+00:00").unwrap();
        let mut bigger = row(3.0);
        bigger.input = 200;
        s.merge_rows(&[bigger], "2026-10-19T14:00:00+00:00").unwrap();

        let text = history(s.connection(), &HistoryFilter { limit: 10, model: None }).unwrap();
        assert!(text.starts_with("quando"), "{text}");
        assert!(text.contains("ignorada"), "{text}");
        assert!(text.contains("aplicada"), "{text}");
        assert!(text.contains("glm-5.3"), "{text}");
        assert_eq!(text.matches("2026-10-19T1").count(), 2, "{text}");

        let filtered = history(
            s.connection(),
            &HistoryFilter {
                limit: 10,
                model: Some("outro".into()),
            },
        )
        .unwrap();
        assert!(filtered.contains("sem histórico"), "{filtered}");
    }
}
