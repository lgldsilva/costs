//! Comandos `report` e `breakdown`: tabelas de texto sobre a tabela `usage`.
//!
//! Report rendering. Cada comando devolve o texto renderizado; avisos que o
//! Python mandava para stderr voltam em `Breakdown::warnings`.

pub mod dashboard;
pub mod dates;
pub mod output;
pub mod query;

use rusqlite::{Connection, params_from_iter};

use crate::prices::{PriceTable, Usage, components};

/// Mensagem quando não há linhas.
pub const NO_DATA_MSG: &str = "sem dados (rode `costs sync` antes)";

const REPORT_FOOTNOTE: &str =
    "* consumo estimado pelos tokens dos logs, a preço de API. Não inclui assinaturas nem recargas.";

const BREAKDOWN_FOOTNOTE: &str = "ajuste = coletado - soma base. Positivo: faixa de preço (ex.: Haiku >100k tokens \
por requisição) ou escrita de cache de 1h. Negativo: desconto/cache já embutido no custo informado pelo agente. \
Não dá para separar só com totais.";

/// Dimensão de agrupamento do report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Host,
    Agent,
    Provider,
    Model,
    ProviderModel,
    Date,
    DateModel,
    DailyDetail,
}

impl GroupBy {
    /// Nome na CLI (cabeçalho da primeira coluna).
    pub fn name(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Agent => "agent",
            Self::Provider => "provider",
            Self::Model => "model",
            Self::ProviderModel => "provider+model",
            Self::Date => "date",
            Self::DateModel => "date+model",
            Self::DailyDetail => "date+host+agent+provider+model",
        }
    }

    /// Interpreta o nome da CLI.
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Host,
            Self::Agent,
            Self::Provider,
            Self::Model,
            Self::ProviderModel,
            Self::Date,
            Self::DateModel,
            Self::DailyDetail,
        ]
        .into_iter()
        .find(|g| g.name() == s)
    }

    /// Colunas do GROUP BY: literais fixos, nunca texto do usuário.
    fn columns(self) -> &'static [&'static str] {
        match self {
            Self::Host => &["host"],
            Self::Agent => &["agent"],
            Self::Provider => &["provider"],
            Self::Model => &["model"],
            Self::ProviderModel => &["provider", "model"],
            Self::Date => &["date"],
            Self::DateModel => &["date", "model"],
            Self::DailyDetail => &["date", "host", "agent", "provider", "model"],
        }
    }
}

/// Saída do breakdown: tabela e avisos (stderr no Python).
#[derive(Debug, Clone, PartialEq)]
pub struct Breakdown {
    pub text: String,
    pub warnings: Vec<String>,
}

/// Inteiro/decimal com separador de milhar (`{:,}` / `{:,.2f}` do Python).
fn group_thousands(digits: &str) -> String {
    let (sign, digits) = digits.strip_prefix('-').map_or(("", digits), |d| ("-", d));
    let (int, frac) = digits.split_once('.').map_or((digits, ""), |(i, f)| (i, f));
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    let dot = if frac.is_empty() { "" } else { "." };
    format!("{sign}{out}{dot}{frac}")
}

fn money(v: f64) -> String {
    group_thousands(&format!("{v:.2}"))
}

fn int(v: i64) -> String {
    group_thousands(&v.to_string())
}

fn month_filter(month: Option<&str>) -> (&'static str, Vec<String>) {
    match month {
        Some(m) => ("date LIKE ?", vec![format!("{m}%")]),
        None => ("1=1", vec![]),
    }
}

struct ReportRow {
    label: String,
    cost: Option<f64>,
    missing: i64,
    input: i64,
    output: i64,
}

/// `costs report`: consumo por dimensão, opcionalmente de um mês (`YYYY-MM`).
pub fn report(con: &Connection, by: GroupBy, month: Option<&str>) -> Result<String, rusqlite::Error> {
    render_filtered(con, by, month, &query::Filters::default(), false)
}

/// Filter before aggregation; legacy report retains its historical ordering.
pub fn report_filtered(
    con: &Connection,
    by: GroupBy,
    month: Option<&str>,
    filters: &query::Filters,
) -> Result<String, rusqlite::Error> {
    render_filtered(con, by, month, filters, true)
}

fn render_filtered(
    con: &Connection,
    by: GroupBy,
    month: Option<&str>,
    filters: &query::Filters,
    chronological: bool,
) -> Result<String, rusqlite::Error> {
    let cols = by.columns();
    let n = cols.len();
    let group = cols.join(", ");
    let (where_sql, params) = filters.predicate(month);
    let order = if chronological && cols[0] == "date" {
        group.clone()
    } else {
        format!("SUM(cost_usd) DESC, {group}")
    };
    let sql = format!(
        "SELECT {group}, SUM(cost_usd), SUM(CASE WHEN cost_usd IS NULL THEN 1 ELSE 0 END), \
         SUM(input), SUM(output) FROM usage WHERE {where_sql} \
         GROUP BY {group} ORDER BY {order}"
    );
    let mut stmt = con.prepare(&sql)?;
    let rows: Vec<ReportRow> = stmt
        .query_map(params_from_iter(params), |r| {
            let labels = (0..n).map(|i| r.get::<_, String>(i)).collect::<Result<Vec<_>, _>>()?;
            Ok(ReportRow {
                label: labels.join(" / "),
                cost: r.get(n)?,
                missing: r.get::<_, Option<i64>>(n + 1)?.unwrap_or(0),
                input: r.get::<_, Option<i64>>(n + 2)?.unwrap_or(0),
                output: r.get::<_, Option<i64>>(n + 3)?.unwrap_or(0),
            })
        })?
        .collect::<Result<_, _>>()?;
    if rows.is_empty() {
        return Ok(format!("{NO_DATA_MSG}\n"));
    }
    let width = if n > 1 { 36 } else { 28 };
    let header = format!(
        "{:<width$} {:>12} {:>10} {:>14} {:>12}",
        by.name(),
        "consumo USD*",
        "sem custo",
        "input",
        "output"
    );
    let rule = "-".repeat(header.chars().count());
    let mut out = format!("{header}\n{rule}\n");
    let (mut total_cost, mut total_missing) = (0.0, 0);
    for row in &rows {
        let cost_txt = row.cost.map_or_else(|| "-".to_string(), money);
        total_cost += row.cost.unwrap_or(0.0);
        total_missing += row.missing;
        out.push_str(&format!(
            "{:<width$} {:>12} {:>10} {:>14} {:>12}\n",
            row.label,
            cost_txt,
            row.missing,
            int(row.input),
            int(row.output)
        ));
    }
    out.push_str(&format!(
        "{rule}\n{:<width$} {:>12} {:>10}\n{REPORT_FOOTNOTE}\n",
        "TOTAL",
        money(total_cost),
        total_missing
    ));
    Ok(out)
}

const BREAKDOWN_COLUMNS: [&str; 4] = ["entrada", "saida", "cache_leitura", "cache_escrita"];

/// `costs breakdown`: custo por componente a preço base vs. o custo coletado (gravado no banco).
///
/// `table`: preços por nome canônico; `prices_label`: caminho do prices.json,
/// citado nos avisos. Modelo sem preço não entra na soma, mas é listado no fim
/// (antes ficava invisível).
pub fn breakdown(
    con: &Connection,
    month: Option<&str>,
    table: &PriceTable,
    prices_label: &str,
) -> Result<Breakdown, rusqlite::Error> {
    let (where_sql, params) = month_filter(month);
    let sql = format!(
        "SELECT model, SUM(input), SUM(output), SUM(cache_read), SUM(cache_write), SUM(cost_usd) \
         FROM usage WHERE {where_sql} GROUP BY model ORDER BY SUM(cost_usd) DESC"
    );
    let mut stmt = con.prepare(&sql)?;
    type Agg = (String, Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<f64>);
    let rows: Vec<Agg> = stmt
        .query_map(params_from_iter(params), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?
        .collect::<Result<_, _>>()?;

    let mut header = format!("{:<21}", "modelo");
    for c in BREAKDOWN_COLUMNS {
        header.push_str(&format!("{c:>15}"));
    }
    header.push_str(&format!("{:>12}{:>10}{:>10}", "soma base", "coletado", "ajuste"));
    let rule = "-".repeat(header.chars().count());
    let mut out = format!("{header}\n{rule}\n");
    let mut warnings = Vec::new();
    let mut unpriced: Vec<(String, i64)> = Vec::new();
    let mut totals = [0.0_f64; 7]; // 4 componentes, base, coletado, ajuste
    for (model, i, o, cr, cw, stored_cost) in rows {
        let tokens: i64 = [i, o, cr, cw].iter().map(|v| v.unwrap_or(0)).sum();
        let Some(entry) = table.lookup(&model) else {
            warnings.push(format!("{model:<20} sem preço em {prices_label} (rode `costs prices`)"));
            unpriced.push((model, tokens));
            continue;
        };
        let comp = components(
            Usage {
                input: i.unwrap_or(0) as f64,
                output: o.unwrap_or(0) as f64,
                cache_read: cr.unwrap_or(0) as f64,
                cache_write: cw.unwrap_or(0) as f64,
            },
            &entry.price,
        );
        let parts = [comp.input, comp.output, comp.cache_read, comp.cache_write];
        let base: f64 = parts.iter().sum();
        let collected = stored_cost.unwrap_or(0.0);
        let adj = collected - base;
        let label = if entry.fallback { format!("{model}*") } else { model };
        out.push_str(&format!("{label:<21}"));
        for p in parts {
            out.push_str(&format!("{:>15}", money(p)));
        }
        out.push_str(&format!(
            "{:>12}{:>10}{:>10}\n",
            money(base),
            money(collected),
            money(adj)
        ));
        for (t, v) in totals.iter_mut().zip(parts.into_iter().chain([base, collected, adj])) {
            *t += v;
        }
    }
    out.push_str(&format!("{rule}\n{:<21}", "TOTAL"));
    for t in &totals[..4] {
        out.push_str(&format!("{:>15}", money(*t)));
    }
    out.push_str(&format!(
        "{:>12}{:>10}{:>10}\n{BREAKDOWN_FOOTNOTE}\n",
        money(totals[4]),
        money(totals[5]),
        money(totals[6])
    ));
    if !unpriced.is_empty() {
        let names: Vec<&str> = unpriced.iter().map(|(m, _)| m.as_str()).collect();
        let total: i64 = unpriced.iter().map(|(_, t)| t).sum();
        out.push_str(&format!(
            "sem preço em {prices_label}: {} modelos, {} tokens (rode `costs prices`)\n{}\n",
            unpriced.len(),
            int(total),
            names.join(", ")
        ));
    }
    Ok(Breakdown { text: out, warnings })
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use tempfile::TempDir;

    use super::*;
    use crate::model::Row;
    use crate::store::Store;

    fn open(dir: &TempDir) -> Store {
        let now = Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap();
        Store::open(&dir.path().join("t.sqlite"), "", now).unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        host: &str,
        agent: &str,
        date: &str,
        model: &str,
        provider: &str,
        tokens: (i64, i64),
        cost: Option<f64>,
    ) -> Row {
        Row {
            host: host.into(),
            agent: agent.into(),
            date: date.into(),
            model: model.into(),
            model_raw: model.into(),
            provider: provider.into(),
            input: tokens.0,
            output: tokens.1,
            cache_read: 500_000,
            cache_write: 100_000,
            cost_usd: cost,
            cost_basis: "estimated".into(),
        }
    }

    fn seeded(dir: &TempDir) -> Store {
        let s = open(dir);
        s.upsert_rows(
            &[
                row(
                    "mac",
                    "claude",
                    "2026-10-09",
                    "claude-opus-4-6",
                    "anthropic",
                    (1_000_000, 200_000),
                    Some(6.0),
                ),
                row(
                    "mac",
                    "claude",
                    "2026-09-30",
                    "claude-haiku",
                    "anthropic",
                    (2_000, 1_000),
                    Some(0.5),
                ),
                row("pi", "codex", "2026-10-10", "gpt-5", "openai", (3_000, 4_000), None),
            ],
            "t",
        )
        .unwrap();
        s
    }

    #[test]
    fn exact_combined_filters_apply_before_totals() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        let filters = query::Filters {
            provider: Some("claude".into()),
            host: Some("mac".into()),
            agent: Some("claude".into()),
            model: Some("claude-opus-4-6".into()),
            ..Default::default()
        };
        let out = report_filtered(s.connection(), GroupBy::DailyDetail, Some("2026-10"), &filters).unwrap();
        assert!(out.contains("2026-10-09 / mac / claude / anthropic / claude-opus-4-6"));
        assert!(out.contains("6.00"));
        assert!(!out.contains("gpt-5"));
        let injection = query::Filters {
            model: Some("' OR 1=1 --".into()),
            ..Default::default()
        };
        assert_eq!(
            report_filtered(s.connection(), GroupBy::Model, None, &injection).unwrap(),
            format!("{NO_DATA_MSG}\n")
        );
    }

    #[test]
    fn filtered_daily_report_is_chronological() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        let out = report_filtered(s.connection(), GroupBy::DateModel, None, &query::Filters::default()).unwrap();
        assert!(out.find("2026-09-30").unwrap() < out.find("2026-10-09").unwrap());
    }

    const FOOT: &str = "* consumo estimado pelos tokens dos logs, a preço de API. Não inclui assinaturas nem recargas.";

    #[test]
    fn report_por_provedor() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        let out = report(s.connection(), GroupBy::Provider, None).unwrap();
        let header = format!(
            "{:<28} {:>12} {:>10} {:>14} {:>12}",
            "provider", "consumo USD*", "sem custo", "input", "output"
        );
        let rule = "-".repeat(header.len());
        let want = format!(
            "{header}\n{rule}\n\
             {:<28} {:>12} {:>10} {:>14} {:>12}\n\
             {:<28} {:>12} {:>10} {:>14} {:>12}\n\
             {rule}\n{:<28} {:>12} {:>10}\n{FOOT}\n",
            "anthropic", "6.50", 0, "1,002,000", "201,000", "openai", "-", 1, "3,000", "4,000", "TOTAL", "6.50", 1
        );
        assert_eq!(out, want);
    }

    #[test]
    fn report_provider_model_usa_largura_36() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        let out = report(s.connection(), GroupBy::ProviderModel, Some("2026-10")).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with(&format!("{:<36} {:>12}", "provider+model", "consumo USD*")));
        assert!(lines[2].starts_with(&format!("{:<36} {:>12}", "anthropic / claude-opus-4-6", "6.00")));
        assert!(lines[3].starts_with("openai / gpt-5"));
        assert_eq!(lines.len(), 7);
        assert!(!out.contains("claude-haiku"));
    }

    #[test]
    fn report_cada_dimensao() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        let first = |by| {
            report(s.connection(), by, None)
                .unwrap()
                .lines()
                .nth(2)
                .unwrap()
                .to_string()
        };
        assert!(first(GroupBy::Host).starts_with("mac "));
        assert!(first(GroupBy::Agent).starts_with("claude "));
        assert!(first(GroupBy::Model).starts_with("claude-opus-4-6 "));
        // data: custo nulo ordena por último; 2026-10-09 (6.0) antes de 2026-09-30 (0.5)
        let by_date = report(s.connection(), GroupBy::Date, None).unwrap();
        let labels: Vec<&str> = by_date
            .lines()
            .skip(2)
            .take(3)
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        assert_eq!(labels, ["2026-10-09", "2026-09-30", "2026-10-10"]);
    }

    #[test]
    fn report_filtro_de_mes_exclui_outros_meses() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        let out = report(s.connection(), GroupBy::Host, Some("2026-09")).unwrap();
        assert!(out.contains("0.50"));
        assert!(!out.contains("pi "));
    }

    #[test]
    fn report_vazio_avisa() {
        let d = TempDir::new().unwrap();
        let s = open(&d);
        assert_eq!(
            report(s.connection(), GroupBy::Host, Some("2026-10")).unwrap(),
            "sem dados (rode `costs sync` antes)\n"
        );
    }

    #[test]
    fn valor_do_mes_nao_vira_sql() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        let out = report(s.connection(), GroupBy::Host, Some("' OR 1=1 --")).unwrap();
        assert_eq!(out, "sem dados (rode `costs sync` antes)\n");
    }

    #[test]
    fn milhar_e_negativos() {
        assert_eq!(group_thousands("1234567.50"), "1,234,567.50");
        assert_eq!(group_thousands("-1234.00"), "-1,234.00");
        assert_eq!(group_thousands("123"), "123");
        assert_eq!(money(-0.5), "-0.50");
    }

    #[test]
    fn group_by_parse_ida_e_volta() {
        for n in ["host", "agent", "provider", "model", "provider+model", "date"] {
            assert_eq!(GroupBy::parse(n).unwrap().name(), n);
        }
        assert!(GroupBy::parse("x; DROP").is_none());
    }

    fn table() -> PriceTable {
        let file: crate::prices::PricesFile = serde_json::from_str(
            r#"{"models":{"claude-opus-4-6":{"prompt":4e-6,"completion":2e-5,
                 "input_cache_read":2e-7,"input_cache_write":5e-6}},
                "resolved":{"claude-opus-4-6":{"id":"anthropic/claude-opus","how":"exact"}}}"#,
        )
        .unwrap();
        PriceTable::new(&file)
    }

    #[test]
    fn breakdown_calcula_componentes_e_ajuste() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        let b = breakdown(s.connection(), None, &table(), "prices.json").unwrap();
        // opus: 1e6*4e-6=4.00, 2e5*2e-5=4.00, 5e5*2e-7=0.10, 1e5*5e-6=0.50 -> base 8.60; coletado 6.00; ajuste -2.60
        let header = format!(
            "{:<21}{:>15}{:>15}{:>15}{:>15}{:>12}{:>10}{:>10}",
            "modelo", "entrada", "saida", "cache_leitura", "cache_escrita", "soma base", "coletado", "ajuste"
        );
        let rule = "-".repeat(header.len());
        let line = |name: &str| {
            format!(
                "{name:<21}{:>15}{:>15}{:>15}{:>15}{:>12}{:>10}{:>10}\n",
                "4.00", "4.00", "0.10", "0.50", "8.60", "6.00", "-2.60"
            )
        };
        let want = format!(
            "{header}\n{rule}\n{}{rule}\n{}{BREAKDOWN_FOOTNOTE}\n\
             sem preço em prices.json: 2 modelos, 1,210,000 tokens (rode `costs prices`)\nclaude-haiku, gpt-5\n",
            line("claude-opus-4-6"),
            line("TOTAL")
        );
        assert_eq!(b.text, want);
        assert_eq!(
            b.warnings,
            [
                format!("{:<20} sem preço em prices.json (rode `costs prices`)", "claude-haiku"),
                format!("{:<20} sem preço em prices.json (rode `costs prices`)", "gpt-5"),
            ]
        );
    }

    #[test]
    fn breakdown_sem_preco_lista_modelo_em_vez_de_sumir() {
        let d = TempDir::new().unwrap();
        let s = seeded(&d);
        let empty = PriceTable::default();
        let b = breakdown(s.connection(), Some("2026-09"), &empty, "p").unwrap();
        assert!(b.text.contains(&format!("{:<21}{:>15}", "TOTAL", "0.00")));
        assert!(b.text.contains("sem preço em p: 1 modelos, 603,000 tokens"));
        assert!(b.text.contains("\nclaude-haiku\n"));
        assert_eq!(b.warnings.len(), 1);
    }
}
