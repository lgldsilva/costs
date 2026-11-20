//! Definição da linha de comando (clap).

use std::path::PathBuf;

use clap::builder::PossibleValuesParser;
use clap::{Parser, Subcommand};
use costs_core::backup::DEFAULT_KEEP;
use costs_core::report::GroupBy;

/// Diretório remoto padrão do `backup --remote`.
pub const DEFAULT_REMOTE_DIR: &str = "backups/costs";

/// Rótulo padrão do `collect --host`.
pub const DEFAULT_COLLECT_HOST: &str = "local";
/// Valor de `--context` que significa stdin.
pub const STDIN_MARK: &str = "-";

const DEFAULT_RUNS_LIMIT: i64 = 20;
const GROUPS: [&str; 8] = [
    "host",
    "agent",
    "provider",
    "model",
    "provider+model",
    "date",
    "date+model",
    "date+host+agent+provider+model",
];

#[derive(Debug, Parser)]
#[command(name = "costs", version = costs_core::version(), about = "Acompanhamento de custos de uso de IA")]
pub struct Cli {
    /// Caminho do arquivo de configuração
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Caminho do banco SQLite
    #[arg(long, global = true, value_name = "PATH")]
    pub db: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Coleta de todos os hosts e grava
    Sync {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        /// Substitui os dias reenviados em vez de somar ao histórico (correção explícita)
        #[arg(long)]
        replace: bool,
    },
    /// Total agregado
    Report {
        /// YYYY-MM
        #[arg(long)]
        month: Option<String>,
        #[arg(long, default_value = "host", value_parser = PossibleValuesParser::new(GROUPS), value_name = "DIM")]
        by: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        last_days: Option<u32>,
        #[arg(long, value_parser = ["last-30-days", "this-week", "last-week"])]
        period: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long, visible_alias = "harness")]
        agent: Option<String>,
        #[arg(long, visible_alias = "machine")]
        host: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value = "text", value_parser = ["text", "html"])]
        format: String,
        #[arg(long, value_name = "PATH")]
        output: Option<PathBuf>,
        #[arg(long)]
        open: bool,
    },
    /// Custo por componente (entrada, saída, cache)
    Breakdown {
        /// YYYY-MM
        #[arg(long)]
        month: Option<String>,
    },
    /// Janelas de quota e saldo por provedor
    Quota {
        /// Busca na API antes de mostrar (sempre; para botões explícitos)
        #[arg(long, conflicts_with = "if_stale")]
        refresh: bool,
        /// Busca na API só o que passou do TTL do cache (para timers e abertura de UI)
        #[arg(long)]
        if_stale: bool,
    },
    /// Saída no formato do SwiftBar (legado)
    Menubar {
        /// Tempo que você tem para usar (ex.: 5h, 2d); padrão: quota.horizon
        #[arg(long)]
        horizon: Option<String>,
    },
    /// Resumo em JSON para o widget do Noctalia (só leitura: sem rede, sem migração)
    Status {
        /// YYYY-MM (padrão: mês atual)
        #[arg(long)]
        month: Option<String>,
    },
    /// Baixa preços por componente do OpenRouter
    Prices,
    /// Recalcula o custo de linhas já gravadas (dry-run por padrão)
    Repricing {
        /// YYYY-MM
        #[arg(long)]
        month: Option<String>,
        /// Recalcula também as linhas que já têm custo estimado (nunca `allocated`)
        #[arg(long)]
        all: bool,
        /// Grava no banco (sem isto só simula)
        #[arg(long)]
        apply: bool,
    },
    /// Verifica o costs dos hosts ssh, grok e frescor de prices.json
    Doctor {
        #[arg(long)]
        host: Option<String>,
    },
    /// Histórico append-only do que cada coleta viu (inclusive dias que encolheram)
    History {
        #[arg(long, default_value_t = 30)]
        limit: i64,
        #[arg(long)]
        model: Option<String>,
    },
    /// Histórico de coletas
    Runs {
        #[arg(long, default_value_t = DEFAULT_RUNS_LIMIT)]
        limit: i64,
        /// Somente falhas
        #[arg(long)]
        errors: bool,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        agent: Option<String>,
    },
    /// Cópia do banco com retenção e envio opcional por ssh
    Backup {
        #[arg(long, default_value_t = DEFAULT_KEEP)]
        keep: usize,
        /// Host ssh que recebe a cópia
        #[arg(long)]
        remote: Option<String>,
        #[arg(long, default_value = DEFAULT_REMOTE_DIR)]
        remote_dir: String,
        /// Diretório local das cópias (padrão: diretório de backups)
        #[arg(long)]
        dest: Option<PathBuf>,
    },
    /// Coleta os agentes deste host e imprime JSON (usado pelo sync via ssh)
    Collect {
        /// Rótulo do host gravado nas linhas
        #[arg(long, default_value = DEFAULT_COLLECT_HOST)]
        host: String,
        /// Agente a coletar (repetível)
        #[arg(long = "agent", required = true, value_name = "AGENTE")]
        agents: Vec<String>,
        /// `-`: lê o contexto JSON (models e prices) do stdin em vez dos arquivos locais
        #[arg(long, value_parser = PossibleValuesParser::new([STDIN_MARK]), value_name = "-")]
        context: Option<String>,
    },
    /// Migra o banco v1 para v2 (com backup)
    Migrate,
    /// Gera o agendamento de sync/backup (dry-run por padrão; imprime os comandos)
    InstallService {
        /// Mostra o plano sem gravar (comportamento padrão)
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        /// Grava os arquivos de agendamento
        #[arg(long)]
        apply: bool,
    },
    /// Imprime os comandos de remoção (--apply apaga os arquivos gerados)
    UninstallService {
        #[arg(long)]
        apply: bool,
    },
}

impl Command {
    /// Dimensão do `report --by` já validada pelo clap.
    pub fn group_by(by: &str) -> GroupBy {
        GroupBy::parse(by).unwrap_or(GroupBy::Host)
    }
}
