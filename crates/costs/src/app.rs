//! Despacho dos subcomandos para o `costs-core`.

use std::error::Error;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chrono::{Local, Utc};
use costs_core::backup::{BackupPlan, DEFAULT_KEEP, RemoteTarget, backup_db, run_backup};
use costs_core::collect::SystemRunner;
use costs_core::config::{self, Config, Host, Paths};
use costs_core::doctor::{prices_age_days, run_doctor};
use costs_core::history::{HistoryFilter, history};
use costs_core::native::{Sources, grok_real};
use costs_core::prices::{self, MISSING_PRICES_MSG, PriceTable, PricesError, PricesFile};
use costs_core::quota;
use costs_core::remote::{CollectSetup, collect_agents, context_json};
use costs_core::report::{
    breakdown, dashboard,
    query::{Filters, valid_month},
    report_filtered,
};
use costs_core::repricing::repricing;
use costs_core::runs::{RunsFilter, runs};
use costs_core::service::{self, HostEnv};
use costs_core::store::{Store, utc_now};
use costs_core::sync::{SyncContext, run_sync};

use crate::cli::{Cli, Command, STDIN_MARK};
use crate::menubar::{self, MenuCtx};
use crate::quota_cmd;
use crate::service_cmd::{self, Request};
use crate::status::{self, StatusCtx};

type Outcome = Result<i32, Box<dyn Error>>;

/// Caminhos efetivos: padrão da plataforma com `--config`/`--db` por cima.
pub fn resolve_paths(cli: &Cli) -> Paths {
    let mut paths = config::default_paths();
    if let Some(c) = &cli.config {
        paths.config.clone_from(c);
    }
    if let Some(d) = &cli.db {
        paths.db.clone_from(d);
    }
    paths
}

fn load_cfg(cli: &Cli, paths: &Paths) -> Result<Config, Box<dyn Error>> {
    Ok(if cli.config.is_some() {
        config::load_config(&paths.config)?
    } else {
        config::load_config_or_default(&paths.config)?
    })
}

fn open_store(paths: &Paths) -> Result<Store, Box<dyn Error>> {
    if let Some(dir) = paths.db.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let rules = config::load_models_text(&paths.models)?;
    Ok(Store::open(&paths.db, &rules, utc_now())?)
}

fn epoch_now() -> f64 {
    Utc::now().timestamp_millis() as f64 / 1000.0
}

/// `prices.json` para o custo estimado; sem o arquivo o custo fica nulo (só avisa).
fn load_prices_file(paths: &Paths, err: &mut dyn Write) -> Result<Option<PricesFile>, Box<dyn Error>> {
    match prices::read_prices(&paths.prices) {
        Ok(f) => Ok(Some(f)),
        Err(PricesError::Missing) => {
            writeln!(
                err,
                "AVISO prices.json ausente: custo estimado fica vazio ({MISSING_PRICES_MSG})"
            )?;
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

/// Com mais que isso o `sync` baixa os preços de novo antes de coletar.
const PRICES_REFRESH_AFTER_DAYS: f64 = 1.0;

/// Atualiza `prices.json` com mais de um dia (ou ilegível) antes do sync. Falha só avisa
/// (segue com o arquivo atual); sem arquivo não baixa (quem cria é `costs prices`). Um
/// arquivo ilegível travaria todos os syncs, então ele é regenerado aqui.
fn refresh_stale_prices(paths: &Paths, http: &dyn prices::HttpClient, err: &mut dyn Write) -> std::io::Result<()> {
    let age = match prices::read_prices(&paths.prices) {
        Ok(file) => match prices_age_days(&file.fetched_at, utc_now()).filter(|a| *a >= PRICES_REFRESH_AFTER_DAYS) {
            Some(age) => format!("tinham {age:.1} dias"),
            None => return Ok(()),
        },
        Err(PricesError::Parse(_)) => "arquivo ilegível".to_string(),
        Err(_) => return Ok(()),
    };
    let Ok(models) = config::load_models_text(&paths.models) else {
        return Ok(());
    };
    match prices::fetch_prices(http, &models, &used_models(paths), &paths.prices, utc_now()) {
        Ok(r) => {
            for w in &r.warnings {
                writeln!(err, "AVISO {w}")?;
            }
            let carried = if r.carried > 0 {
                format!("; {} mantidos do arquivo anterior", r.carried)
            } else {
                String::new()
            };
            writeln!(err, "preços atualizados: {} modelos ({age}{carried})", r.resolved)
        }
        Err(e) => writeln!(err, "AVISO preços ({age}); atualização falhou: {e}"),
    }
}

/// Entradas da coleta: contexto JSON para os hosts ssh e regras/preços dos nativos.
struct SyncInputs {
    /// Vazio se nenhum host é remoto.
    context: String,
    setup: CollectSetup,
}

/// Monta o contexto só se há host remoto e a tabela de preços só se há host nativo
/// (o remoto monta a sua a partir do contexto).
fn sync_inputs(hosts: &[&Host], models: &str, prices: Option<PricesFile>) -> Result<SyncInputs, Box<dyn Error>> {
    let any_native = hosts.iter().any(|h| h.is_native());
    let any_remote = hosts.iter().any(|h| !h.is_native());
    let context = if any_remote {
        context_json(models, prices.as_ref())?
    } else {
        String::new()
    };
    let setup = CollectSetup::new(models, prices.filter(|_| any_native))?;
    Ok(SyncInputs { context, setup })
}

fn cmd_sync(
    cli: &Cli,
    paths: &Paths,
    (host, agent): (Option<&str>, Option<&str>),
    replace: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Outcome {
    let Some(_lock) = instance_lock(paths, "sync")? else {
        // Timer, Noctalia, SwiftBar e terminal podem disparar juntos: o segundo não repete as
        // varreduras nem disputa o banco. Não é falha (o timer não deve marcar erro).
        writeln!(out, "{SYNC_BUSY_MSG}")?;
        return Ok(0);
    };
    let cfg = load_cfg(cli, paths)?;
    let hosts = config::select_hosts(&cfg.hosts, host)?;
    let models = config::load_models_text(&paths.models)?;
    let prices = if hosts.is_empty() {
        None
    } else {
        refresh_stale_prices(paths, &prices::UreqClient, err)?;
        load_prices_file(paths, err)?
    };
    let SyncInputs { context, setup } = sync_inputs(&hosts, &models, prices)?;
    let sources = Sources {
        rules: &setup.rules,
        prices: setup.prices.as_ref(),
        grok: grok_real,
    };
    let native = |agent: &str, host: &str| sources.collect(agent, host);
    let ctx = SyncContext {
        runner: &SystemRunner,
        context: &context,
        rules: &setup.rules,
        native: &native,
        agent_filter: agent,
        replace,
    };
    let store = open_store(paths)?;
    Ok(run_sync(&store, &ctx, &hosts, utc_now(), out, err)?)
}

struct CollectArgs<'a> {
    host: &'a str,
    agents: &'a [String],
    from_stdin: bool,
}

/// `costs collect`: regras e preços do contexto (stdin) ou dos arquivos locais; erros
/// por agente vão no JSON, então o código de saída é 0.
fn cmd_collect(
    paths: &Paths,
    a: &CollectArgs,
    input: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Outcome {
    let Some(_lock) = instance_lock(paths, "collect")? else {
        writeln!(err, "{COLLECT_BUSY_MSG}")?;
        return Ok(COLLECT_BUSY_CODE);
    };
    let setup = if a.from_stdin {
        let mut text = String::new();
        input.read_to_string(&mut text)?;
        CollectSetup::from_context(&text)?
    } else {
        let models = config::load_models_text(&paths.models)?;
        CollectSetup::new(&models, load_prices_file(paths, err)?)?
    };
    let sources = Sources {
        rules: &setup.rules,
        prices: setup.prices.as_ref(),
        grok: grok_real,
    };
    let output = collect_agents(a.host, a.agents, &|agent, host| sources.collect(agent, host));
    writeln!(out, "{}", output.to_json()?)?;
    Ok(0)
}

/// Saída do `collect` quando outra coleta segura o lock (stderr: o stdout é o protocolo).
const COLLECT_BUSY_CODE: i32 = 3;
const COLLECT_BUSY_MSG: &str = "outro costs collect ainda roda neste host; coleta pulada";

const SYNC_BUSY_MSG: &str = "outro costs sync ainda roda; pulado";
const BACKUP_BUSY_MSG: &str = "outro costs backup ainda roda; pulado";

/// Instância única por comando (`<nome>.lock` ao lado do banco). No `collect` vale também no
/// modo `--context`: quando o ssh do sync estoura o timeout, o processo remoto preso em I/O
/// de disco (estado D) não morre, e sem o lock cada tick do timer empilhava mais um
/// disputando o mesmo disco. O lock é do kernel e some com o processo, mesmo num `kill -9`.
fn instance_lock(paths: &Paths, name: &str) -> Result<Option<std::fs::File>, Box<dyn Error>> {
    let lock_path = paths.db.with_file_name(format!("{name}.lock"));
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("não criei {}: {e}", dir.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("não abri {}: {e}", lock_path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("lock de {}: {e}", lock_path.display()).into()),
    }
}

fn cmd_breakdown(paths: &Paths, month: Option<&str>, out: &mut dyn Write, err: &mut dyn Write) -> Outcome {
    let file = match prices::read_prices(&paths.prices) {
        Ok(t) => t,
        Err(PricesError::Missing) => return Err(MISSING_PRICES_MSG.into()),
        Err(e) => return Err(e.into()),
    };
    let table = PriceTable::new(&file);
    let store = open_store(paths)?;
    let result = breakdown(store.connection(), month, &table, &paths.prices.display().to_string())?;
    for w in &result.warnings {
        writeln!(err, "{w}")?;
    }
    write!(out, "{}", result.text)?;
    Ok(0)
}

/// Modelos com uso gravado no banco (para o `prices` resolver todos).
fn used_models(paths: &Paths) -> Vec<String> {
    Store::open_readonly(&paths.db)
        .and_then(|s| s.distinct_models())
        .unwrap_or_default()
}

fn cmd_prices(paths: &Paths, out: &mut dyn Write, err: &mut dyn Write) -> Outcome {
    if let Some(dir) = paths.prices.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let models = config::load_models_text(&paths.models)?;
    let targets = used_models(paths);
    let report = prices::fetch_prices(&prices::UreqClient, &models, &targets, &paths.prices, utc_now())?;
    for w in &report.warnings {
        writeln!(err, "AVISO {w}")?;
    }
    writeln!(
        out,
        "ok: {} modelos com preço em {} ({} oficiais via LiteLLM, {} via OpenRouter; \
         {} override, {} exatos, {} normalizados, {} irmãos)",
        report.resolved,
        paths.prices.display(),
        report.by_source[0],
        report.by_source[1],
        report.by_how[0],
        report.by_how[1],
        report.by_how[2],
        report.by_how[3]
    )?;
    if !report.missing.is_empty() {
        writeln!(
            out,
            "sem preço ({}): {}",
            report.missing.len(),
            report.missing.join(", ")
        )?;
        for m in &report.missing {
            writeln!(err, "SEM PRECO: {m}")?;
        }
    }
    if let Ok(store) = Store::open_readonly(&paths.db) {
        let (rows, models, tokens) = store.unpriced()?;
        if rows > 0 {
            writeln!(
                out,
                "cobertura: {rows} linhas sem preço em {models} modelos ({tokens} tokens); \
                 rode `costs repricing` depois do próximo sync"
            )?;
        }
    }
    Ok(0)
}

struct RepricingArgs {
    month: Option<String>,
    all: bool,
    apply: bool,
}

fn cmd_repricing(paths: &Paths, a: &RepricingArgs, out: &mut dyn Write) -> Outcome {
    let file = match prices::read_prices(&paths.prices) {
        Ok(t) => t,
        Err(PricesError::Missing) => return Err(MISSING_PRICES_MSG.into()),
        Err(e) => return Err(e.into()),
    };
    let table = PriceTable::new(&file);
    let store = open_store(paths)?;
    if a.apply {
        let backup = backup_db(&paths.db, &paths.backup_dir, DEFAULT_KEEP, utc_now())?;
        writeln!(out, "backup: {}", backup.display())?;
    }
    let report = repricing(
        store.connection(),
        &table,
        a.month.as_deref(),
        a.all,
        a.apply,
        utc_now(),
    )?;
    writeln!(
        out,
        "linhas: {} ({} com preço, {} gratuitas, {} sem mudança{}); custo estimado {}",
        report.rows,
        report.priced,
        report.free,
        report.unchanged,
        if a.apply { "" } else { ", simulação" },
        format_usd(report.added_usd)
    )?;
    if !report.missing.is_empty() {
        let names: Vec<String> = report
            .missing
            .iter()
            .map(|(m, t)| format!("{m} ({t} tokens)"))
            .collect();
        writeln!(out, "sem preço ({}): {}", names.len(), names.join(", "))?;
    }
    if !a.apply {
        writeln!(out, "simulação: use --apply para gravar")?;
    }
    Ok(0)
}

fn format_usd(v: f64) -> String {
    format!("US$ {v:.2}")
}

fn cmd_quota(cli: &Cli, paths: &Paths, mode: quota_cmd::Fetch, out: &mut dyn Write) -> Outcome {
    let cfg = load_cfg(cli, paths)?;
    let providers = &cfg.quota.providers;
    let cache_dir = quota::default_cache_dir(&quota::RealEnv);
    let now = Utc::now().timestamp();
    let entries = quota_cmd::entries(providers, mode, now, &cache_dir);
    quota_cmd::render(out, providers, &entries, epoch_now())?;
    Ok(0)
}

fn cmd_menubar(cli: &Cli, paths: &Paths, horizon: Option<&str>, out: &mut dyn Write) -> Outcome {
    let cfg = load_cfg(cli, paths)?;
    let horizon_s = match horizon {
        Some(h) => config::parse_horizon(h)?,
        None => cfg.quota.horizon,
    };
    let store = open_store(paths)?;
    let cache_dir = quota::default_cache_dir(&quota::RealEnv);
    let entries = quota_cmd::entries(&cfg.quota.providers, quota_cmd::Fetch::Cache, 0, &cache_dir);
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("costs"));
    let month = Local::now().format("%Y-%m").to_string();
    let ctx = MenuCtx {
        store: &store,
        cfg: &cfg,
        entries: &entries,
        now: epoch_now(),
        month: &month,
        horizon_s,
        exe: &exe.display().to_string(),
    };
    menubar::render(out, &ctx)?;
    Ok(0)
}

fn cmd_status(cli: &Cli, paths: &Paths, month: Option<&str>, out: &mut dyn Write) -> Outcome {
    let cfg = load_cfg(cli, paths)?;
    let store = Store::open_readonly(&paths.db)?;
    let cache_dir = quota::default_cache_dir(&quota::RealEnv);
    let entries = quota_cmd::entries(&cfg.quota.providers, quota_cmd::Fetch::Cache, 0, &cache_dir);
    let owned;
    let month = match month {
        Some(m) => m,
        None => {
            owned = Local::now().format("%Y-%m").to_string();
            &owned
        }
    };
    let ctx = StatusCtx {
        store: &store,
        cfg: &cfg,
        entries: &entries,
        now: epoch_now(),
        month,
    };
    status::render(out, &ctx)?;
    Ok(0)
}

fn cmd_doctor(cli: &Cli, paths: &Paths, host: Option<&str>, out: &mut dyn Write) -> Outcome {
    let cfg = load_cfg(cli, paths)?;
    let hosts = config::select_hosts(&cfg.hosts, host)?;
    Ok(run_doctor(
        &SystemRunner,
        &hosts,
        &paths.prices,
        &paths.db,
        utc_now(),
        out,
    )?)
}

fn cmd_runs(paths: &Paths, filter: &RunsFilter, out: &mut dyn Write) -> Outcome {
    let store = open_store(paths)?;
    write!(out, "{}", runs(store.connection(), filter)?)?;
    Ok(0)
}

struct BackupArgs<'a> {
    keep: usize,
    remote: Option<&'a str>,
    remote_dir: &'a str,
    dest: Option<&'a Path>,
}

fn cmd_backup(paths: &Paths, a: &BackupArgs, out: &mut dyn Write, err: &mut dyn Write) -> Outcome {
    let Some(_lock) = instance_lock(paths, "backup")? else {
        writeln!(out, "{BACKUP_BUSY_MSG}")?;
        return Ok(0);
    };
    let plan = BackupPlan {
        db: &paths.db,
        dest_dir: a.dest.unwrap_or(&paths.backup_dir),
        keep: a.keep,
        remote: a.remote.map(|remote| RemoteTarget {
            remote,
            dir: a.remote_dir,
        }),
    };
    Ok(run_backup(&SystemRunner, plan, utc_now(), out, err)?)
}

fn cmd_migrate(paths: &Paths, out: &mut dyn Write) -> Outcome {
    open_store(paths)?;
    writeln!(
        out,
        "ok: {} no schema v{}",
        paths.db.display(),
        costs_core::store::SCHEMA_VERSION
    )?;
    Ok(0)
}

fn service_request(paths: &Paths, apply: bool) -> Request {
    let data_dir = paths.db.parent().map(Path::to_path_buf).unwrap_or_default();
    Request {
        os: service::current(),
        env: HostEnv::detect(),
        exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("costs")),
        log_dir: data_dir.join("logs"),
        apply,
    }
}

fn cmd_report(cli: &Cli, paths: &Paths, args: &ReportArgs, out: &mut dyn Write) -> Outcome {
    if args.month.is_some_and(|m| !valid_month(m)) {
        return Err("invalid month: expected YYYY-MM (01–12)".into());
    }
    if args.open && args.format != "html" {
        return Err("--open requires --format html".into());
    }
    let range = costs_core::report::dates::resolve(
        args.month,
        args.last_days,
        args.period,
        args.from,
        args.to,
        Local::now().date_naive(),
    )?;
    let store = open_store(paths)?;
    let filters = Filters {
        range,
        provider: args.provider.clone(),
        agent: args.agent.clone(),
        host: args.host.clone(),
        model: args.model.clone(),
    };
    let text = if args.format == "html" {
        let cfg = load_cfg(cli, paths)?;
        let current_month;
        let month = match args.month {
            Some(month) => month,
            None => {
                current_month = Local::now().format("%Y-%m").to_string();
                &current_month
            }
        };
        dashboard::html(store.connection(), month, &filters, &cfg)?
    } else {
        report_filtered(store.connection(), Command::group_by(args.by), None, &filters)?
    };
    let destination = report_destination(args)?;
    if let Some(path) = destination {
        costs_core::report::output::write_report(&path, text.as_bytes())
            .map_err(|e| format!("write report {}: {e}", path.display()))?;
        if args.open {
            open_report(&path)?;
        }
    } else {
        write!(out, "{text}")?;
    }
    Ok(0)
}

struct ReportArgs<'a> {
    last_days: Option<u32>,
    period: Option<&'a str>,
    from: Option<&'a str>,
    to: Option<&'a str>,
    month: Option<&'a str>,
    by: &'a str,
    provider: &'a Option<String>,
    agent: &'a Option<String>,
    host: &'a Option<String>,
    model: &'a Option<String>,
    format: &'a str,
    output: &'a Option<PathBuf>,
    open: bool,
}

fn report_destination(args: &ReportArgs<'_>) -> Result<Option<PathBuf>, Box<dyn Error>> {
    if args.output.is_none() && args.open {
        return Ok(Some(costs_core::report::output::default_report_path()?));
    }
    Ok(args.output.as_ref().cloned())
}

fn open_report(path: &Path) -> Result<(), Box<dyn Error>> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let status = std::process::Command::new(opener)
        .arg(std::fs::canonicalize(path)?)
        .status()?;
    if !status.success() {
        return Err("report generated, but browser opener failed".into());
    }
    Ok(())
}

/// Executa o subcomando e devolve o código de saída (`input` é o stdin).
pub fn run(cli: &Cli, input: &mut dyn Read, out: &mut dyn Write, err: &mut dyn Write) -> Outcome {
    let paths = resolve_paths(cli);
    match &cli.command {
        Command::Sync { host, agent, replace } => {
            cmd_sync(cli, &paths, (host.as_deref(), agent.as_deref()), *replace, out, err)
        }
        Command::Report {
            last_days,
            period,
            from,
            to,
            month,
            by,
            provider,
            agent,
            host,
            model,
            format,
            output,
            open,
        } => cmd_report(
            cli,
            &paths,
            &ReportArgs {
                last_days: *last_days,
                period: period.as_deref(),
                from: from.as_deref(),
                to: to.as_deref(),
                month: month.as_deref(),
                by,
                provider,
                agent,
                host,
                model,
                format,
                output,
                open: *open,
            },
            out,
        ),
        Command::Breakdown { month } => cmd_breakdown(&paths, month.as_deref(), out, err),
        Command::Quota { refresh, if_stale } => {
            cmd_quota(cli, &paths, quota_cmd::Fetch::from_flags(*refresh, *if_stale), out)
        }
        Command::Menubar { horizon } => cmd_menubar(cli, &paths, horizon.as_deref(), out),
        Command::Status { month } => cmd_status(cli, &paths, month.as_deref(), out),
        Command::Prices => cmd_prices(&paths, out, err),
        Command::Repricing { month, all, apply } => cmd_repricing(
            &paths,
            &RepricingArgs {
                month: month.clone(),
                all: *all,
                apply: *apply,
            },
            out,
        ),
        Command::Doctor { host } => cmd_doctor(cli, &paths, host.as_deref(), out),
        Command::History { limit, model } => {
            let store = open_store(&paths)?;
            let filter = HistoryFilter {
                limit: *limit,
                model: model.clone(),
            };
            write!(out, "{}", history(store.connection(), &filter)?)?;
            Ok(0)
        }
        Command::Runs {
            limit,
            errors,
            host,
            agent,
        } => {
            let filter = RunsFilter {
                limit: *limit,
                errors: *errors,
                host: host.clone(),
                agent: agent.clone(),
            };
            cmd_runs(&paths, &filter, out)
        }
        Command::Backup {
            keep,
            remote,
            remote_dir,
            dest,
        } => {
            let args = BackupArgs {
                keep: *keep,
                remote: remote.as_deref(),
                remote_dir,
                dest: dest.as_deref(),
            };
            cmd_backup(&paths, &args, out, err)
        }
        Command::Collect { host, agents, context } => {
            let args = CollectArgs {
                host,
                agents,
                from_stdin: context.as_deref() == Some(STDIN_MARK),
            };
            cmd_collect(&paths, &args, input, out, err)
        }
        Command::Migrate => cmd_migrate(&paths, out),
        Command::InstallService { apply, .. } => service_cmd::install(&service_request(&paths, *apply), out),
        Command::UninstallService { apply } => service_cmd::uninstall(&service_request(&paths, *apply), out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn run_args(args: &[&str]) -> (i32, String, String) {
        run_with_stdin(args, "")
    }

    fn run_with_stdin(args: &[&str], stdin: &str) -> (i32, String, String) {
        let cli = Cli::try_parse_from(std::iter::once("costs").chain(args.iter().copied())).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = match run(&cli, &mut stdin.as_bytes(), &mut out, &mut err) {
            Ok(c) => c,
            Err(e) => {
                err.extend_from_slice(e.to_string().as_bytes());
                1
            }
        };
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    fn fixture() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("s.sqlite").display().to_string();
        (dir, db)
    }

    #[test]
    fn report_rejects_invalid_month_before_opening_db() {
        let (_d, db) = fixture();
        let (code, _, err) = run_args(&["--db", &db, "report", "--month", "2026-13"]);
        assert_eq!(code, 1);
        assert!(err.contains("YYYY-MM"));
        assert!(!Path::new(&db).exists());
    }

    #[test]
    fn html_output_is_offline_and_accepts_filter_aliases() {
        let (d, db) = fixture();
        let cfg = d.path().join("config.toml");
        std::fs::write(&cfg, "").unwrap();
        let output = d.path().join("report.html");
        let (code, _, err) = run_args(&[
            "--config",
            cfg.to_str().unwrap(),
            "--db",
            &db,
            "report",
            "--format",
            "html",
            "--output",
            output.to_str().unwrap(),
            "--harness",
            "codex",
            "--machine",
            "mac",
            "--provider",
            "codex",
            "--model",
            "<hostile>",
        ]);
        assert_eq!(code, 0, "{err}");
        let html = std::fs::read_to_string(output).unwrap();
        assert!(html.contains("lang=\"pt-BR\""));
        assert!(!html.contains("/*PAYLOAD*/"));
        assert!(!html.contains("<script src="));
    }

    #[test]
    fn status_on_missing_db_is_an_error() {
        let (_d, db) = fixture();
        let missing = format!("{db}-nao-existe");
        let (code, _, err) = run_args(&["--db", &missing, "status"]);
        assert_eq!(code, 1);
        assert!(!err.is_empty(), "erro tem mensagem");
    }

    #[test]
    fn status_reads_empty_db_without_network() {
        let (_d, db) = fixture();
        let (code, _, _) = run_args(&["--db", &db, "migrate"]);
        assert_eq!(code, 0);
        let (code, out, _) = run_args(&["--db", &db, "status", "--month", "2026-10"]);
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["month"], "2026-10");
        assert_eq!(v["spend_usd"], 0.0);
        assert_eq!(v["stale"], true);
    }

    #[test]
    fn report_and_runs_on_empty_db() {
        let (_d, db) = fixture();
        let (code, out, _) = run_args(&["--db", &db, "report", "--by", "provider"]);
        assert_eq!(code, 0);
        assert!(out.contains("sem dados"), "{out}");
        let (code, out, _) = run_args(&["--db", &db, "runs", "--errors"]);
        assert_eq!(code, 0);
        assert!(out.contains("sem coletas registradas com erro"), "{out}");
    }

    #[test]
    fn migrate_creates_current_schema() {
        let (_d, db) = fixture();
        let (code, out, _) = run_args(&["--db", &db, "migrate"]);
        assert_eq!(code, 0);
        assert!(out.contains(&format!("schema v{}", costs_core::store::SCHEMA_VERSION)));
    }

    #[test]
    fn unknown_config_is_an_error() {
        let (d, db) = fixture();
        let cfg = d.path().join("nope.toml").display().to_string();
        let (code, _, _) = run_args(&["--db", &db, "--config", &cfg, "doctor"]);
        assert_eq!(code, 1);
    }

    #[test]
    fn breakdown_without_prices_explains_how_to_fix() {
        let (d, db) = fixture();
        let (code, _, err) = run_args(&["--db", &db, "breakdown"]);
        // COSTS_HOME não está definido nos testes: o prices.json padrão pode existir ou não.
        assert!(code == 0 || err.contains("costs prices"), "{err}");
        drop(d);
    }

    #[test]
    fn backup_copies_the_db() {
        let (d, db) = fixture();
        run_args(&["--db", &db, "migrate"]);
        let dest = d.path().join("bk").display().to_string();
        let (code, out, _) = run_args(&["--db", &db, "backup", "--dest", &dest, "--keep", "2"]);
        assert_eq!(code, 0);
        assert!(out.starts_with("ok: "), "{out}");
    }

    #[test]
    fn collect_with_stdin_context_reports_errors_per_agent() {
        let ctx = &format!(
            r#"{{"version":{},"models":"","prices":null}}"#,
            costs_core::remote::COLLECT_PROTOCOL_VERSION
        );
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("c.db").display().to_string();
        let (code, out, _) = run_with_stdin(
            &[
                "collect",
                "--db",
                &db,
                "--host",
                "srv",
                "--agent",
                "nada",
                "--agent",
                "outro",
                "--context",
                "-",
            ],
            ctx,
        );
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["version"], costs_core::remote::COLLECT_PROTOCOL_VERSION);
        assert_eq!(v["agents"]["nada"]["error"], "agente sem coletor nativo: nada");
        assert!(v["agents"]["outro"]["error"].is_string());
    }

    #[test]
    fn collect_with_bad_context_fails() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("c.db").display().to_string();
        let (code, out, err) = run_with_stdin(&["collect", "--db", &db, "--agent", "nada", "--context", "-"], "lixo");
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(err.contains("contexto invalido"), "{err}");
        let (code, _, err) = run_with_stdin(
            &["collect", "--db", &db, "--agent", "nada", "--context", "-"],
            r#"{"version":7,"models":"","prices":null}"#,
        );
        assert_eq!(code, 1);
        assert!(err.contains("atualize o costs"), "{err}");
    }

    fn collect_local(paths: &Paths) -> (Outcome, String, String) {
        let agents = ["nada".to_string()];
        let args = CollectArgs {
            host: "local",
            agents: &agents,
            from_stdin: false,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let res = cmd_collect(paths, &args, &mut "".as_bytes(), &mut out, &mut err);
        (res, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn collect_without_context_uses_local_files() {
        let d = tempfile::tempdir().unwrap();
        let paths = config::paths_in(d.path());
        let (res, out, err) = collect_local(&paths);
        assert_eq!(res.unwrap(), 0);
        assert!(err.starts_with("AVISO prices.json ausente"), "{err}");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["agents"]["nada"]["error"], "agente sem coletor nativo: nada");

        std::fs::write(&paths.prices, r#"{"fetched_at":"x","source":"s","models":{}}"#).unwrap();
        let (res, _, err) = collect_local(&paths);
        assert_eq!(res.unwrap(), 0);
        assert!(err.is_empty(), "{err}");

        std::fs::write(&paths.models, "[[rules").unwrap();
        assert!(collect_local(&paths).0.is_err());
    }

    #[test]
    fn sync_and_backup_skip_while_another_instance_runs() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("store.sqlite");
        let paths = Paths {
            db: db.clone(),
            ..config::paths_in(d.path())
        };
        let db = db.display().to_string();
        for (name, args, msg) in [
            ("sync", vec!["--db", db.as_str(), "sync"], SYNC_BUSY_MSG),
            ("backup", vec!["--db", db.as_str(), "backup"], BACKUP_BUSY_MSG),
        ] {
            let held = instance_lock(&paths, name).unwrap().expect("primeiro lock");
            let (code, out, _) = run_args(&args);
            assert_eq!((code, out.trim()), (0, msg), "{name} ocupado é pulado sem falhar");
            drop(held);
        }
    }

    #[test]
    fn collect_skips_while_another_holds_the_lock() {
        let d = tempfile::tempdir().unwrap();
        let paths = config::paths_in(&d.path().join("sub"));
        let held = instance_lock(&paths, "collect").unwrap().expect("primeiro lock");
        let (res, out, err) = collect_local(&paths);
        assert_eq!(res.unwrap(), COLLECT_BUSY_CODE);
        assert!(out.is_empty(), "stdout é o protocolo do sync: {out}");
        assert_eq!(err.trim(), COLLECT_BUSY_MSG);
        drop(held);
        assert_eq!(collect_local(&paths).0.unwrap(), 0, "lock liberado com o processo");
    }

    #[test]
    fn corrupt_prices_json_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let paths = config::paths_in(d.path());
        std::fs::write(&paths.prices, "lixo").unwrap();
        let mut err = Vec::new();
        assert!(load_prices_file(&paths, &mut err).is_err());
        assert!(err.is_empty());
        assert!(collect_local(&paths).0.is_err());
    }

    fn cfg_hosts(text: &str) -> Vec<Host> {
        config::parse_config(text).unwrap().hosts
    }

    struct NoNetwork;
    impl prices::HttpClient for NoNetwork {
        fn get(&self, _url: &str) -> Result<String, String> {
            Err("offline".into())
        }
    }

    fn price_paths(dir: &Path, fetched_at: &str) -> Paths {
        let prices = dir.join("prices.json");
        std::fs::write(&prices, format!(r#"{{"fetched_at":"{fetched_at}","models":{{}}}}"#)).unwrap();
        let models = dir.join("models.toml");
        std::fs::write(&models, "").unwrap();
        Paths {
            config: dir.join("config.toml"),
            models,
            db: dir.join("nada.sqlite"),
            prices,
            backup_dir: dir.join("bk"),
        }
    }

    #[test]
    fn stale_prices_refresh_before_sync_and_failure_only_warns() {
        let d = tempfile::tempdir().unwrap();
        let fresh = price_paths(d.path(), &costs_core::store::format_ts(utc_now()));
        let mut err = Vec::new();
        refresh_stale_prices(&fresh, &NoNetwork, &mut err).unwrap();
        assert!(err.is_empty(), "fresco não baixa");
        let stale = price_paths(d.path(), "2026-01-01T00:00:00+00:00");
        refresh_stale_prices(&stale, &NoNetwork, &mut err).unwrap();
        let text = String::from_utf8(err).unwrap();
        assert!(text.contains("atualização falhou"), "{text}");
        assert!(prices::read_prices(&stale.prices).is_ok(), "arquivo antigo preservado");
    }

    /// Só o OpenRouter responde, com um modelo.
    struct OnlyOpenRouter;
    impl prices::HttpClient for OnlyOpenRouter {
        fn get(&self, url: &str) -> Result<String, String> {
            if url.contains("openrouter") {
                Ok(
                    r#"{"data":[{"id":"a/m","created":1,"pricing":{"prompt":"0.000001","completion":"0.000002"}}]}"#
                        .into(),
                )
            } else {
                Err("offline".into())
            }
        }
    }

    #[test]
    fn prices_ilegivel_e_regenerado_e_avisos_aparecem() {
        let d = tempfile::tempdir().unwrap();
        let paths = price_paths(d.path(), "x");
        std::fs::write(&paths.prices, r#"{"models":{"m":{"prompt":null}}}"#).unwrap();
        assert!(prices::read_prices(&paths.prices).is_err(), "pré-condição: ilegível");
        let mut err = Vec::new();
        refresh_stale_prices(&paths, &OnlyOpenRouter, &mut err).unwrap();
        let text = String::from_utf8(err).unwrap();
        assert!(text.contains("AVISO LiteLLM indisponível"), "{text}");
        assert!(text.contains("arquivo ilegível"), "{text}");
        assert!(prices::read_prices(&paths.prices).is_ok(), "regenerado");
    }

    fn prices_file() -> PricesFile {
        serde_json::from_str(r#"{"fetched_at":"x","source":"s","models":{}}"#).unwrap()
    }

    const LOCAL: &str = "[[hosts]]\nname = \"mac\"\n";
    const REMOTE: &str = "[[hosts]]\nname = \"vm\"\nkind = \"ssh\"\ntarget = \"vm\"\n";
    const MODELS: &str = "[[rules]]\nmatch = \"^claude\"\nprovider = \"anthropic\"\n";

    #[test]
    fn sync_inputs_local_only_has_table_and_no_context() {
        let hosts = cfg_hosts(LOCAL);
        let refs: Vec<&Host> = hosts.iter().collect();
        let inputs = sync_inputs(&refs, MODELS, Some(prices_file())).unwrap();
        assert!(inputs.context.is_empty());
        assert!(inputs.setup.prices.is_some());
        assert_eq!(inputs.setup.rules.len(), 1);
    }

    #[test]
    fn sync_inputs_remote_only_sends_context_without_local_table() {
        let hosts = cfg_hosts(REMOTE);
        let refs: Vec<&Host> = hosts.iter().collect();
        let inputs = sync_inputs(&refs, MODELS, Some(prices_file())).unwrap();
        assert!(inputs.setup.prices.is_none());
        let v: serde_json::Value = serde_json::from_str(&inputs.context).unwrap();
        assert_eq!(v["version"], costs_core::remote::COLLECT_PROTOCOL_VERSION);
        assert_eq!(v["models"], MODELS);
        assert_eq!(v["prices"]["source"], "s");
        // price_map não é mais lido na coleta: o preço já vem resolvido por nome.
        assert!(sync_inputs(&refs, "price_map = 3\n", Some(prices_file())).is_ok());
        let v: serde_json::Value = serde_json::from_str(&sync_inputs(&refs, MODELS, None).unwrap().context).unwrap();
        assert!(v["prices"].is_null());
    }

    #[test]
    fn sync_inputs_mixed_has_both_and_rejects_bad_models() {
        let hosts = cfg_hosts(&format!("{LOCAL}\n{REMOTE}"));
        let refs: Vec<&Host> = hosts.iter().collect();
        let inputs = sync_inputs(&refs, MODELS, Some(prices_file())).unwrap();
        assert!(!inputs.context.is_empty());
        assert!(inputs.setup.prices.is_some());
        assert!(sync_inputs(&refs, "price_map = 3\n", Some(prices_file())).is_ok());
        assert!(sync_inputs(&refs, "[[rules", None).is_err());
    }

    #[test]
    fn keep_zero_is_rejected() {
        let (_d, db) = fixture();
        let (code, _, err) = run_args(&["--db", &db, "backup", "--keep", "0"]);
        assert_eq!(code, 1);
        assert!(err.contains("--keep"));
    }
}
