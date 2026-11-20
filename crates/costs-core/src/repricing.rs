//! `costs repricing`: recalcula o custo de linhas já gravadas no banco.
//!
//! Serve para recuperar dias coletados quando o preço ainda não existia (custo
//! nulo) sem depender dos logs originais. Por padrão só toca linhas com
//! `cost_usd IS NULL`; com `--all` recalcula também as estimadas, mas nunca as
//! `allocated` (custo rateado pelo próprio agente).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params_from_iter};

use crate::model::cost_eq;
use crate::prices::{PriceTable, Usage, components};
use crate::pricing::is_free_model;
use crate::store::format_ts;

/// Resumo do repricing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Repricing {
    /// Linhas examinadas.
    pub rows: usize,
    /// Linhas que receberam preço de catálogo.
    pub priced: usize,
    /// Linhas de rota gratuita (`:free`): custo zero.
    pub free: usize,
    /// Linhas cujo custo recalculado é o gravado: não são regravadas nem vão ao histórico.
    pub unchanged: usize,
    /// Modelos sem preço (nome e tokens), para o aviso.
    pub missing: Vec<(String, i64)>,
    /// Custo estimado somado às linhas alteradas.
    pub added_usd: f64,
    /// Só simulou (sem `--apply`).
    pub dry_run: bool,
}

struct Pending {
    rowid: i64,
    cost: f64,
    basis: &'static str,
}

/// `(rowid, model, model_raw, input, output, cache_read, cache_write, cost_usd, cost_basis)`.
type RowCost = (i64, String, String, i64, i64, i64, i64, Option<f64>, String);

fn push_if_changed(pending: &mut Vec<Pending>, report: &mut Repricing, stored: (Option<f64>, &str), p: Pending) {
    if cost_eq(stored.0, Some(p.cost)) && stored.1 == p.basis {
        report.unchanged += 1;
    } else {
        pending.push(p);
    }
}

/// Recalcula (ou simula) o custo das linhas.
pub fn repricing(
    con: &Connection,
    table: &PriceTable,
    month: Option<&str>,
    all: bool,
    apply: bool,
    now: DateTime<Utc>,
) -> Result<Repricing, rusqlite::Error> {
    let filter = if all {
        // Custo informado pelo agente (`reported`) é a fonte; só a estimativa é refeita.
        "cost_basis NOT IN ('allocated', 'reported')"
    } else {
        // Linhas sem preço + rotas gratuitas que ainda tenham custo gravado.
        "(cost_usd IS NULL OR (cost_usd <> 0 AND lower(model_raw) LIKE '%free'))"
    };
    let sql = format!(
        "SELECT rowid, model, model_raw, input, output, cache_read, cache_write, cost_usd, cost_basis FROM usage \
         WHERE {filter}{} ORDER BY rowid",
        if month.is_some() { " AND date LIKE ?" } else { "" }
    );
    let mut stmt = con.prepare(&sql)?;
    let params: Vec<String> = month.map(|m| vec![format!("{m}%")]).unwrap_or_default();
    let rows: Vec<RowCost> = stmt
        .query_map(params_from_iter(params), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
            ))
        })?
        .collect::<Result<_, _>>()?;

    let mut report = Repricing {
        dry_run: !apply,
        ..Repricing::default()
    };
    let mut missing: BTreeMap<String, i64> = BTreeMap::new();
    let mut pending = Vec::new();
    for (rowid, model, raw, input, output, cache_read, cache_write, stored, stored_basis) in rows {
        report.rows += 1;
        let tokens = [input, output, cache_read, cache_write];
        let shown = tokens.iter().sum::<i64>();
        if is_free_model(&raw) {
            report.free += 1;
            push_if_changed(
                &mut pending,
                &mut report,
                (stored, &stored_basis),
                Pending {
                    rowid,
                    cost: 0.0,
                    basis: "free",
                },
            );
            continue;
        }
        let Some(entry) = table.lookup(&model) else {
            *missing.entry(model).or_default() += shown;
            continue;
        };
        let comp = components(
            Usage {
                input: tokens[0] as f64,
                output: tokens[1] as f64,
                cache_read: tokens[2] as f64,
                cache_write: tokens[3] as f64,
            },
            &entry.price,
        );
        report.priced += 1;
        report.added_usd += comp.total;
        push_if_changed(
            &mut pending,
            &mut report,
            (stored, &stored_basis),
            Pending {
                rowid,
                cost: comp.total,
                basis: if entry.fallback { "fallback" } else { "estimated" },
            },
        );
    }
    report.missing = missing.into_iter().collect();

    if apply {
        let ts = format_ts(now);
        let tx = con.unchecked_transaction()?;
        {
            let mut upd = tx.prepare("UPDATE usage SET cost_usd = ?1, cost_basis = ?2 WHERE rowid = ?3")?;
            let mut hist = tx.prepare(
                "INSERT INTO usage_history (ts, origin, host, agent, date, model, model_raw, provider, \
                 input, output, cache_read, cache_write, cost_usd, cost_basis, kept) \
                 SELECT ?1, 'repricing', host, agent, date, model, model_raw, provider, \
                        input, output, cache_read, cache_write, cost_usd, cost_basis, 1 \
                 FROM usage WHERE rowid = ?2",
            )?;
            for p in &pending {
                upd.execute(rusqlite::params![p.cost, p.basis, p.rowid])?;
                hist.execute(rusqlite::params![ts, p.rowid])?;
            }
        }
        tx.commit()?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use tempfile::TempDir;

    use super::*;
    use crate::model::Row;
    use crate::prices::PricesFile;
    use crate::store::Store;

    fn open(dir: &TempDir) -> Store {
        Store::open(&dir.path().join("t.sqlite"), "", now()).unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap()
    }

    fn row(model: &str, raw: &str, cost: Option<f64>, basis: &str) -> Row {
        Row {
            host: "mac".into(),
            agent: "zcode".into(),
            date: "2026-10-01".into(),
            model: model.into(),
            model_raw: raw.into(),
            provider: "outro".into(),
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 0,
            cache_write: 0,
            cost_usd: cost,
            cost_basis: basis.into(),
        }
    }

    fn table() -> PriceTable {
        let file: PricesFile = serde_json::from_str(
            r#"{"models":{
                 "glm-5.3":{"prompt":0.000001,"completion":0.000002},
                 "k3":{"prompt":0.000003,"completion":0.000004}},
                "resolved":{
                 "glm-5.3":{"id":"z-ai/glm-5.3","how":"exact"},
                 "k3":{"id":"moonshotai/kimi-k3","how":"sibling"}}}"#,
        )
        .unwrap();
        PriceTable::new(&file)
    }

    fn seeded(dir: &TempDir) -> Store {
        let s = open(dir);
        s.upsert_rows(
            &[
                row("glm-5.3", "glm-5.3", None, "estimated"),
                row("k3", "k3", None, "estimated"),
                row("nada", "nada", None, "estimated"),
                row("minimax-m3", "minimax/minimax-m3:free", None, "estimated"),
                row("claude-opus-5-5", "claude-opus-5-5", Some(9.0), "allocated"),
            ],
            "t",
        )
        .unwrap();
        s
    }

    fn costs(s: &Store) -> Vec<(String, Option<f64>, String)> {
        let mut stmt = s
            .connection()
            .prepare("SELECT model, cost_usd, cost_basis FROM usage ORDER BY model")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn dry_run_nao_altera_nada_e_resume() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        let r = repricing(s.connection(), &table(), None, false, false, now()).unwrap();
        assert!(r.dry_run);
        assert_eq!((r.rows, r.priced, r.free), (4, 2, 1));
        assert_eq!(r.missing, vec![("nada".to_string(), 2_000_000)]);
        assert!((r.added_usd - 10.0).abs() < 1e-9);
        assert!(costs(&s).iter().all(|(m, c, _)| *m == "claude-opus-5-5" || c.is_none()));
    }

    #[test]
    fn apply_grava_custo_e_base() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        repricing(s.connection(), &table(), None, false, true, now()).unwrap();
        let got = costs(&s);
        let by = |name: &str| got.iter().find(|(m, _, _)| m == name).unwrap();
        assert_eq!(by("glm-5.3").1, Some(3.0));
        assert_eq!(by("glm-5.3").2, "estimated");
        assert_eq!(by("k3").1, Some(7.0));
        assert_eq!(by("k3").2, "fallback");
        assert_eq!(by("minimax-m3").1, Some(0.0));
        assert_eq!(by("minimax-m3").2, "free");
        assert_eq!(by("nada").1, None, "sem preço continua nulo");
        assert_eq!(by("claude-opus-5-5").1, Some(9.0), "allocated intocado");
        let hist: i64 = s
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM usage_history WHERE origin = 'repricing'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hist, 3, "cada linha regravada entra no histórico");
    }

    #[test]
    fn all_preserva_custo_informado_pelo_agente() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        s.upsert_rows(
            &[
                row("glm-5.3", "glm-5.3", Some(0.5), "reported"),
                row("k3", "k3", Some(1.0), "estimated"),
            ],
            "t",
        )
        .unwrap();
        let r = repricing(s.connection(), &table(), None, true, true, now()).unwrap();
        assert_eq!(r.rows, 1, "só a estimativa entra no --all");
        let got = costs(&s);
        assert_eq!(got[0], ("glm-5.3".into(), Some(0.5), "reported".into()));
        assert_eq!(got[1].1, Some(7.0));
    }

    #[test]
    fn rota_gratuita_com_custo_gravado_vira_zero_mesmo_sem_all() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        s.upsert_rows(&[row("space-bunny", "space-bunny-free", Some(1.25), "estimated")], "t")
            .unwrap();
        let r = repricing(s.connection(), &table(), None, false, true, now()).unwrap();
        assert_eq!((r.rows, r.free, r.priced), (1, 1, 0));
        let (cost, basis): (Option<f64>, String) = s
            .connection()
            .query_row("SELECT cost_usd, cost_basis FROM usage", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((cost, basis.as_str()), (Some(0.0), "free"));
    }

    #[test]
    fn all_recalcula_estimado_mas_preserva_allocated() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        repricing(s.connection(), &table(), None, false, true, now()).unwrap();
        // segunda passada com --all não muda allocated
        let r = repricing(s.connection(), &table(), None, true, true, now()).unwrap();
        assert_eq!(r.priced + r.free + r.missing.len(), r.rows);
        let got = costs(&s);
        assert_eq!(
            got.iter().find(|(m, _, _)| m == "claude-opus-5-5").unwrap().1,
            Some(9.0)
        );
    }

    #[test]
    fn all_nao_regrava_linha_cujo_custo_nao_mudou() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        s.upsert_rows(&[row("glm-5.3", "glm-5.3", None, "estimated")], "t0")
            .unwrap();
        let first = repricing(s.connection(), &table(), None, true, true, now()).unwrap();
        assert_eq!((first.priced, first.unchanged), (1, 0));
        let again = repricing(s.connection(), &table(), None, true, true, now()).unwrap();
        assert_eq!((again.priced, again.unchanged), (1, 1));
        let hist: i64 = s
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM usage_history WHERE origin = 'repricing'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hist, 1, "o segundo repricing não gera histórico");
    }

    #[test]
    fn month_filtra_linhas() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        let r = repricing(s.connection(), &table(), Some("2026-09"), false, true, now()).unwrap();
        assert_eq!(r.rows, 0);
    }
}
