//! Comando sync: coleta os hosts em paralelo e grava no banco (a thread principal escreve).

use std::io::Write;
use std::thread;

use chrono::{DateTime, Utc};

use crate::collect::{CollectError, Collector, Runner};
use crate::config::Host;
use crate::model::{Row, Rules};
use crate::store::{DEFAULT_PRUNE_DAYS, Store, StoreError, format_ts};

const UNREACHABLE_MAX: usize = 260;
const ERROR_MAX: usize = 280;

/// Resultado de um agente; erro nunca derruba os outros.
struct AgentResult {
    agent: String,
    outcome: Result<Vec<Row>, String>,
}

/// Coletor nativo: (agente, host) -> linhas.
pub type NativeCollector<'a> = &'a (dyn Fn(&str, &str) -> Result<Vec<Row>, String> + Sync);

/// Dependências do sync.
pub struct SyncContext<'a> {
    pub runner: &'a dyn Runner,
    /// Contexto JSON enviado aos hosts ssh (`remote::context_json`); vazio se não houver.
    pub context: &'a str,
    pub rules: &'a Rules,
    pub native: NativeCollector<'a>,
    pub agent_filter: Option<&'a str>,
    /// Substitui os dias reenviados (padrão: merge, que nunca encolhe um dia).
    pub replace: bool,
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn error_message(e: CollectError) -> String {
    match e {
        CollectError::Unreachable(m) => format!("host inacessivel, pulando o resto: {}", truncate(&m, UNREACHABLE_MAX)),
        CollectError::Failed(m) => truncate(&m, ERROR_MAX),
    }
}

/// Host local: coletores nativos neste processo. Host ssh: `costs collect` remoto.
fn collect_host(ctx: &SyncContext, host: &Host) -> Vec<AgentResult> {
    let agents: Vec<String> = host
        .agents
        .iter()
        .filter(|a| ctx.agent_filter.is_none_or(|f| f == a.as_str()))
        .cloned()
        .collect();
    if host.is_native() {
        return agents
            .into_iter()
            .map(|agent| {
                let outcome = (ctx.native)(&agent, &host.name).map_err(|m| truncate(&m, ERROR_MAX));
                AgentResult { agent, outcome }
            })
            .collect();
    }
    let collector = Collector {
        runner: ctx.runner,
        context: ctx.context,
        rules: ctx.rules,
    };
    collector
        .collect_remote(host, &agents)
        .into_iter()
        .map(|(agent, res)| AgentResult {
            agent,
            outcome: res.map_err(error_message),
        })
        .collect()
}

fn collect_hosts(ctx: &SyncContext, hosts: &[&Host]) -> Vec<Vec<AgentResult>> {
    thread::scope(|s| {
        let handles: Vec<_> = hosts.iter().map(|h| s.spawn(move || collect_host(ctx, h))).collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect()
    })
}

fn success_note(store: &Store, host: &Host, agent: &str, rows_empty: bool, kept: usize) -> Result<String, StoreError> {
    if rows_empty && store.has_usage(&host.name, agent)? {
        return Ok(" (fonte vazia: histórico mantido)".to_string());
    }
    if kept > 0 {
        return Ok(format!(" ({kept} mantidas: fonte menor)"));
    }
    Ok(String::new())
}

fn store_success(
    store: &Store,
    ctx: &SyncContext,
    host: &Host,
    agent: &str,
    rows: &[Row],
    now: DateTime<Utc>,
    collected_at: &str,
) -> Result<usize, StoreError> {
    let dates: std::collections::HashSet<String> = rows.iter().map(|r| r.date.clone()).collect();
    if ctx.replace {
        store.replace_days(&host.name, agent, &dates, collected_at)?;
    }
    let merged = store.merge_rows(rows, collected_at)?;
    store.log_run(now, &host.name, agent, "ok", None, rows.len() as i64)?;
    Ok(merged.kept)
}

fn record_results(
    store: &Store,
    ctx: &SyncContext,
    hosts: &[&Host],
    collected: Vec<Vec<AgentResult>>,
    now: DateTime<Utc>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<usize, StoreError> {
    let collected_at = format_ts(now);
    let mut failures = 0;
    for (host, results) in hosts.iter().zip(collected) {
        for AgentResult { agent, outcome } in results {
            match outcome {
                Err(msg) => {
                    failures += 1;
                    store.log_run(now, &host.name, &agent, "error", Some(&msg), 0)?;
                    writeln!(err, "ERRO  {:<12} {:<10} {msg}", host.name, agent).ok();
                }
                Ok(rows) => {
                    let kept = store_success(store, ctx, host, &agent, &rows, now, &collected_at)?;
                    let note = success_note(store, host, &agent, rows.is_empty(), kept)?;
                    writeln!(
                        out,
                        "ok    {:<12} {:<10} {:>4} linhas{note}",
                        host.name,
                        agent,
                        rows.len()
                    )
                    .ok();
                }
            }
        }
    }
    Ok(failures)
}

/// Coleta (em paralelo por host), grava e devolve o código de saída (1 se algo falhou).
pub fn run_sync(
    store: &Store,
    ctx: &SyncContext,
    hosts: &[&Host],
    now: DateTime<Utc>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, StoreError> {
    let collected = collect_hosts(ctx, hosts);
    let failures = record_results(store, ctx, hosts, collected, now, out, err)?;
    store.prune_runs(DEFAULT_PRUNE_DAYS, now)?;
    Ok(i32::from(failures > 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::fake::{FakeRunner, exit, ok};
    use crate::config::{HostKind, HostSource};
    use crate::model::load_rules;
    use crate::remote::{collect_agents, context_json};
    use chrono::TimeZone;

    const MODELS: &str = "[[rules]]\nmatch = \"^claude\"\nprovider = \"anthropic\"\n";

    fn ssh_host(name: &str, agents: &[&str]) -> Host {
        Host {
            name: name.into(),
            kind: HostKind::Ssh,
            target: Some(name.into()),
            enabled: true,
            agents: agents.iter().map(|s| (*s).to_string()).collect(),
            path_prefix: None,
            source: HostSource::Remote,
        }
    }

    fn no_native(_: &str, _: &str) -> Result<Vec<Row>, String> {
        panic!("coletor nativo nao deve rodar para host ssh")
    }

    fn local_host(agents: &[&str]) -> Host {
        Host {
            kind: HostKind::Local,
            target: None,
            source: HostSource::Native,
            ..ssh_host("mac", agents)
        }
    }

    fn row(host: &str, agent: &str, date: &str) -> Row {
        Row {
            host: host.into(),
            agent: agent.into(),
            date: date.into(),
            model: "claude-sonnet-4".into(),
            model_raw: "claude-sonnet-4".into(),
            provider: "outro".into(),
            input: 3,
            output: 2,
            cache_read: 0,
            cache_write: 0,
            cost_usd: Some(0.5),
            cost_basis: "estimated".into(),
        }
    }

    /// Resposta do `costs collect` remoto: `claude` com uma linha, os demais com erro.
    fn remote_payload(agents: &[&str]) -> String {
        let agents: Vec<String> = agents.iter().map(|s| (*s).to_string()).collect();
        collect_agents("rotulo-remoto", &agents, &|agent, host| match agent {
            "claude" => Ok(vec![row(host, agent, "2026-05-01")]),
            _ => Err("boom".to_string()),
        })
        .to_json()
        .unwrap()
    }

    fn setup() -> (tempfile::TempDir, Store, Rules, DateTime<Utc>) {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 5, 2, 12, 0, 0).unwrap();
        let store = Store::open(&dir.path().join("s.sqlite"), "", now).unwrap();
        (dir, store, load_rules(MODELS).unwrap(), now)
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.connection().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn ctx<'a>(
        runner: &'a FakeRunner,
        rules: &'a Rules,
        native: NativeCollector<'a>,
        agent_filter: Option<&'a str>,
    ) -> SyncContext<'a> {
        SyncContext {
            runner,
            context: "{\"version\":1}",
            rules,
            native,
            agent_filter,
            replace: false,
        }
    }

    #[test]
    fn native_local_host_isolates_agents_and_skips_runner() {
        let (_d, store, rules, now) = setup();
        let runner = FakeRunner::new(Box::new(|_, _, _| panic!("runner nao deve ser chamado")));
        let native = |agent: &str, host: &str| match agent {
            "claude" | "grok" => Ok(vec![row(host, agent, "2026-05-01"), row(host, agent, "2026-05-02")]),
            _ => Err("falhou".to_string()),
        };
        let host = local_host(&["claude", "pi", "grok"]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let rc = run_sync(
            &store,
            &ctx(&runner, &rules, &native, None),
            &[&host],
            now,
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(rc, 1);
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("claude        2 linhas") && out.contains("grok          2 linhas"),
            "{out}"
        );
        assert!(String::from_utf8(err).unwrap().contains("falhou"));
        assert_eq!(count(&store, "SELECT COUNT(*) FROM usage WHERE host='mac'"), 4);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM runs WHERE status='error'"), 1);
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn remote_host_end_to_end_isolates_errors_and_relabels_rows() {
        let (_d, store, rules, now) = setup();
        let payload = remote_payload(&["claude", "pi"]);
        let runner = FakeRunner::new(Box::new(move |_, _, _| ok(&payload)));
        let context = context_json(MODELS, None).unwrap();
        let sync_ctx = SyncContext {
            context: &context,
            ..ctx(&runner, &rules, &no_native, None)
        };
        let host = ssh_host("srv", &["claude", "pi"]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let rc = run_sync(&store, &sync_ctx, &[&host], now, &mut out, &mut err).unwrap();
        assert_eq!(rc, 1);
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with("ok    srv          claude        1 linhas"), "{out}");
        assert_eq!(String::from_utf8(err).unwrap(), "ERRO  srv          pi         boom\n");
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM usage WHERE host='srv' AND agent='claude' AND provider='anthropic'"
            ),
            1
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM usage"), 1);
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM runs WHERE status='error' AND message='boom'"
            ),
            1
        );
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2.as_deref(), Some(context.as_str()));
        assert!(calls[0].1.last().unwrap().contains("collect --host 'srv'"));
    }

    #[test]
    fn sync_parallel_hosts_unreachable_and_filter() {
        let (_d, store, rules, now) = setup();
        let payload = remote_payload(&["claude"]);
        let runner = FakeRunner::new(Box::new(move |_, args, _| {
            if args.contains(&"down".to_string()) {
                exit(255, "refused")
            } else {
                ok(&payload)
            }
        }));
        let sync_ctx = ctx(&runner, &rules, &no_native, Some("claude"));
        let (a, b) = (ssh_host("up", &["claude", "pi"]), ssh_host("down", &["claude", "pi"]));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let rc = run_sync(&store, &sync_ctx, &[&a, &b], now, &mut out, &mut err).unwrap();
        assert_eq!(rc, 1);
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("host inacessivel, pulando o resto: refused")
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM runs"), 2); // pi filtrado
        // reexecucao: replace_days nao duplica
        let rc = run_sync(&store, &sync_ctx, &[&a], now, &mut Vec::new(), &mut Vec::new()).unwrap();
        assert_eq!(rc, 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM usage"), 1);
    }

    #[test]
    fn recolha_menor_nao_encolhe_o_dia_e_replace_forca() {
        let (_d, store, rules, now) = setup();
        let big = collect_agents("x", &["claude".to_string()], &|h, a| Ok(vec![row(h, a, "2026-05-01")]))
            .to_json()
            .unwrap();
        let mut small_row = row("x", "claude", "2026-05-01");
        small_row.input = 1;
        small_row.output = 1;
        small_row.cost_usd = Some(0.01);
        let small = collect_agents("x", &["claude".to_string()], &|_, _| Ok(vec![small_row.clone()]))
            .to_json()
            .unwrap();

        let host = ssh_host("srv", &["claude"]);
        let runner_big = FakeRunner::new(Box::new(move |_, _, _| ok(&big)));
        let runner_small = FakeRunner::new(Box::new(move |_, _, _| ok(&small)));

        let mut out = Vec::new();
        run_sync(
            &store,
            &ctx(&runner_big, &rules, &no_native, None),
            &[&host],
            now,
            &mut out,
            &mut Vec::new(),
        )
        .unwrap();
        assert!((store.month_spend("2026-05").unwrap() - 0.5).abs() < 1e-9);

        // mesma coleta, agora com a fonte menor: mantém 0.5 e registra a tentativa
        let mut out = Vec::new();
        run_sync(
            &store,
            &ctx(&runner_small, &rules, &no_native, None),
            &[&host],
            now,
            &mut out,
            &mut Vec::new(),
        )
        .unwrap();
        assert!((store.month_spend("2026-05").unwrap() - 0.5).abs() < 1e-9);
        assert!(String::from_utf8(out).unwrap().contains("1 mantidas: fonte menor"));

        // --replace aplica a correção de propósito
        let mut replace_ctx = ctx(&runner_small, &rules, &no_native, None);
        replace_ctx.replace = true;
        run_sync(&store, &replace_ctx, &[&host], now, &mut Vec::new(), &mut Vec::new()).unwrap();
        assert!((store.month_spend("2026-05").unwrap() - 0.01).abs() < 1e-9);
    }

    #[test]
    fn remote_protocol_mismatch_is_logged_per_agent() {
        let (_d, store, rules, now) = setup();
        let runner = FakeRunner::new(Box::new(|_, _, _| ok(r#"{"version":99,"agents":{}}"#)));
        let host = ssh_host("srv", &["claude", "pi"]);
        let mut err = Vec::new();
        let rc = run_sync(
            &store,
            &ctx(&runner, &rules, &no_native, None),
            &[&host],
            now,
            &mut Vec::new(),
            &mut err,
        )
        .unwrap();
        assert_eq!(rc, 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM runs WHERE status='error'"), 2);
        assert!(String::from_utf8(err).unwrap().contains("v99"));
    }
}
