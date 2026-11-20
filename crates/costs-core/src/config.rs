//! Configuração: hosts, agentes, assinaturas, quota, horizonte e modelos.
//!
//! As funções de parse recebem TEXTO; só `load_*` tocam o disco. Os padrões
//! embutidos (`defaults/`) valem quando o arquivo do usuário não existe.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use directories::{BaseDirs, ProjectDirs};
use regex::Regex;
use serde::Deserialize;

/// `config.toml` embutido (fallback).
pub const DEFAULT_CONFIG_TOML: &str = include_str!("../defaults/config.toml");
/// `models.toml` embutido (fallback).
pub const DEFAULT_MODELS_TOML: &str = include_str!("../defaults/models.toml");

/// Agentes usados quando o `config.toml` não define `agents`.
pub const DEFAULT_AGENTS: [&str; 6] = ["claude", "codex", "opencode", "pi", "zcode", "kimi"];

const DEFAULT_HORIZON: &str = "24h";
const SECS_PER_MINUTE: f64 = 60.0;
const SECS_PER_HOUR: f64 = 3600.0;
const SECS_PER_DAY: f64 = 86400.0;
/// Variável de ambiente que concentra config e dados num único diretório.
pub const HOME_ENV: &str = "COSTS_HOME";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("toml invalido: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("horizonte invalido: {0:?} (ex.: 5h, 2d)")]
    Horizon(String),
    #[error("host desconhecido: {0}")]
    UnknownHost(String),
    #[error("erro lendo {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKind {
    Local,
    Ssh,
}

/// Origem dos dados de uso de um host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostSource {
    /// Coletores nativos rodando neste processo (só hosts locais).
    Native,
    /// `costs collect` no host remoto, via ssh (padrão de `kind = "ssh"`).
    Remote,
}

/// Mensagem de migração para configs antigas com `source = "ccusage"`.
const CCUSAGE_REMOVED_MSG: &str =
    "source ccusage foi removido; instale o costs em ~/.local/bin/costs no host e apague a chave source";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub name: String,
    pub kind: HostKind,
    pub target: Option<String>,
    pub enabled: bool,
    pub agents: Vec<String>,
    pub path_prefix: Option<String>,
    pub source: HostSource,
}

impl Host {
    /// Coleta nativa: host local (os coletores rodam neste processo).
    pub fn is_native(&self) -> bool {
        self.kind == HostKind::Local && self.source == HostSource::Native
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Subscription {
    pub name: String,
    pub provider: Option<String>,
    pub monthly_usd: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaConfig {
    pub ai_usage: Option<PathBuf>,
    /// Horizonte em segundos.
    pub horizon: f64,
    pub providers: Vec<String>,
    pub cost_map: BTreeMap<String, Vec<String>>,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            ai_usage: None,
            horizon: parse_horizon(DEFAULT_HORIZON).unwrap_or(SECS_PER_DAY),
            providers: Vec::new(),
            cost_map: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub agents: Vec<String>,
    pub hosts: Vec<Host>,
    pub subscriptions: Vec<Subscription>,
    pub quota: QuotaConfig,
}

/// Regras de provedor e mapa de preços (strings; a regex é compilada em `model`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelsConfig {
    /// (match, provider)
    pub rules: Vec<(String, String)>,
    /// (model, openrouter)
    pub price_map: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config: PathBuf,
    pub models: PathBuf,
    pub db: PathBuf,
    pub prices: PathBuf,
    pub backup_dir: PathBuf,
}

// ---------- parse ----------

#[derive(Deserialize)]
struct RawConfig {
    agents: Option<Vec<String>>,
    #[serde(default)]
    hosts: Vec<RawHost>,
    #[serde(default)]
    subscriptions: Vec<RawSub>,
    quota: Option<RawQuota>,
}

#[derive(Deserialize)]
struct RawHost {
    name: String,
    kind: Option<String>,
    target: Option<String>,
    agents: Option<Vec<String>>,
    path_prefix: Option<String>,
    enabled: Option<bool>,
    source: Option<String>,
}

#[derive(Deserialize)]
struct RawSub {
    name: String,
    provider: Option<String>,
    monthly_usd: Number,
}

#[derive(Deserialize)]
struct RawQuota {
    ai_usage: Option<String>,
    horizon: Option<Number2>,
    #[serde(default)]
    providers: Vec<String>,
    #[serde(default)]
    cost_map: BTreeMap<String, OneOrMany>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Number {
    Int(i64),
    Float(f64),
}

impl Number {
    fn get(&self) -> f64 {
        match self {
            Number::Int(i) => *i as f64,
            Number::Float(f) => *f,
        }
    }
}

/// Horizonte: número puro (horas) ou texto (`5h`, `2d`).
#[derive(Deserialize)]
#[serde(untagged)]
enum Number2 {
    Num(Number),
    Text(String),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct RawModels {
    #[serde(default)]
    rules: Vec<RawRule>,
    #[serde(default)]
    price_map: Vec<RawPrice>,
}

#[derive(Deserialize)]
struct RawRule {
    #[serde(rename = "match")]
    pattern: String,
    provider: String,
}

#[derive(Deserialize)]
struct RawPrice {
    model: String,
    openrouter: String,
}

static HORIZON_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+(?:\.\d+)?)([mhd]?)$").expect("regex de horizonte valida"));

/// Tempo disponível: `5h`, `2d`, `90m` ou segundos. Devolve segundos.
pub fn parse_horizon(text: &str) -> Result<f64, ConfigError> {
    let err = || ConfigError::Horizon(text.to_string());
    let caps = HORIZON_RE.captures(text.trim()).ok_or_else(err)?;
    let value: f64 = caps[1].parse().map_err(|_| err())?;
    let unit = match &caps[2] {
        "m" => SECS_PER_MINUTE,
        "h" => SECS_PER_HOUR,
        "d" => SECS_PER_DAY,
        _ => 1.0,
    };
    Ok(value * unit)
}

fn parse_host(h: RawHost, default_agents: &[String]) -> Result<Host, ConfigError> {
    let kind = match h.kind.as_deref().unwrap_or("local") {
        "local" => HostKind::Local,
        "ssh" => HostKind::Ssh,
        other => {
            return Err(ConfigError::Invalid(format!(
                "host {}: kind invalido {other:?}",
                h.name
            )));
        }
    };
    if kind == HostKind::Ssh && h.target.as_deref().is_none_or(str::is_empty) {
        return Err(ConfigError::Invalid(format!("host {}: kind ssh exige target", h.name)));
    }
    let source = match (h.source.as_deref(), kind) {
        (None | Some("native"), HostKind::Local) => HostSource::Native,
        (None | Some("remote"), HostKind::Ssh) => HostSource::Remote,
        (Some("ccusage"), _) => {
            return Err(ConfigError::Invalid(format!("host {}: {CCUSAGE_REMOVED_MSG}", h.name)));
        }
        (Some("native"), HostKind::Ssh) => {
            return Err(ConfigError::Invalid(format!(
                "host {}: source native so vale para kind local",
                h.name
            )));
        }
        (Some("remote"), HostKind::Local) => {
            return Err(ConfigError::Invalid(format!(
                "host {}: source remote so vale para kind ssh",
                h.name
            )));
        }
        (Some(other), _) => {
            return Err(ConfigError::Invalid(format!(
                "host {}: source invalido {other:?}",
                h.name
            )));
        }
    };
    Ok(Host {
        name: h.name,
        kind,
        target: h.target,
        enabled: h.enabled.unwrap_or(true),
        agents: h.agents.unwrap_or_else(|| default_agents.to_vec()),
        path_prefix: h.path_prefix,
        source,
    })
}

fn parse_quota(raw: Option<RawQuota>) -> Result<QuotaConfig, ConfigError> {
    let Some(q) = raw else {
        return Ok(QuotaConfig::default());
    };
    let horizon = match q.horizon {
        None => parse_horizon(DEFAULT_HORIZON)?,
        Some(Number2::Num(n)) => n.get() * SECS_PER_HOUR, // número puro = horas
        Some(Number2::Text(t)) => parse_horizon(&t)?,
    };
    let cost_map = q
        .cost_map
        .into_iter()
        .map(|(k, v)| {
            let ids = match v {
                OneOrMany::One(s) => vec![s],
                OneOrMany::Many(v) => v,
            };
            (k, ids)
        })
        .collect();
    Ok(QuotaConfig {
        ai_usage: q.ai_usage.map(|p| expand_tilde(&p)),
        horizon,
        providers: q.providers,
        cost_map,
    })
}

/// Interpreta o texto do `config.toml`.
pub fn parse_config(text: &str) -> Result<Config, ConfigError> {
    let raw: RawConfig = toml::from_str(text)?;
    let agents = raw
        .agents
        .unwrap_or_else(|| DEFAULT_AGENTS.iter().map(|s| s.to_string()).collect());
    let hosts = raw
        .hosts
        .into_iter()
        .map(|h| parse_host(h, &agents))
        .collect::<Result<Vec<_>, _>>()?;
    let subscriptions = raw
        .subscriptions
        .into_iter()
        .map(|s| Subscription {
            name: s.name,
            provider: s.provider,
            monthly_usd: s.monthly_usd.get(),
        })
        .collect();
    Ok(Config {
        agents,
        hosts,
        subscriptions,
        quota: parse_quota(raw.quota)?,
    })
}

/// Interpreta o texto do `models.toml`.
pub fn parse_models(text: &str) -> Result<ModelsConfig, ConfigError> {
    let raw: RawModels = toml::from_str(text)?;
    Ok(ModelsConfig {
        rules: raw.rules.into_iter().map(|r| (r.pattern, r.provider)).collect(),
        price_map: raw.price_map.into_iter().map(|p| (p.model, p.openrouter)).collect(),
    })
}

/// Sem nome: só hosts habilitados. Com nome: qualquer host com esse nome.
pub fn select_hosts<'a>(hosts: &'a [Host], name: Option<&str>) -> Result<Vec<&'a Host>, ConfigError> {
    let Some(name) = name else {
        return Ok(hosts.iter().filter(|h| h.enabled).collect());
    };
    let chosen: Vec<&Host> = hosts.iter().filter(|h| h.name == name).collect();
    if chosen.is_empty() {
        return Err(ConfigError::UnknownHost(name.to_string()));
    }
    Ok(chosen)
}

// ---------- caminhos e carga ----------

/// Expande `~` inicial com o diretório home.
pub fn expand_tilde(path: &str) -> PathBuf {
    let home = BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    expand_tilde_with(path, home.as_deref())
}

fn expand_tilde_with(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix('~'), home) {
        (Some(""), Some(h)) => h.to_path_buf(),
        (Some(rest), Some(h)) if rest.starts_with('/') => h.join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}

/// Todos os caminhos dentro de um único diretório (modo `COSTS_HOME`).
pub fn paths_in(root: &Path) -> Paths {
    Paths {
        config: root.join("config.toml"),
        models: root.join("models.toml"),
        db: root.join("store.sqlite"),
        prices: root.join("prices.json"),
        backup_dir: root.join("backups"),
    }
}

/// Caminhos padrão: `COSTS_HOME` se definido, senão diretórios da plataforma.
pub fn default_paths() -> Paths {
    if let Some(root) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
        return paths_in(&expand_tilde(&root.to_string_lossy()));
    }
    match ProjectDirs::from("com", "lgldsilva", "costs") {
        Some(d) => {
            let (cfg, data) = (d.config_dir(), d.data_local_dir());
            Paths {
                config: cfg.join("config.toml"),
                models: cfg.join("models.toml"),
                db: data.join("store.sqlite"),
                prices: data.join("prices.json"),
                backup_dir: data.join("backups"),
            }
        }
        None => paths_in(Path::new(".costs")),
    }
}

fn read_or_default(path: &Path, default: &str) -> Result<String, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(default.to_string()),
        Err(source) => Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Lê e interpreta um `config.toml` (erro se o arquivo não existir).
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_config(&text)
}

/// Config do usuário, ou o padrão embutido se o arquivo não existir.
pub fn load_config_or_default(path: &Path) -> Result<Config, ConfigError> {
    parse_config(&read_or_default(path, DEFAULT_CONFIG_TOML)?)
}

/// Texto do `models.toml` do usuário, ou o padrão embutido.
pub fn load_models_text(path: &Path) -> Result<String, ConfigError> {
    read_or_default(path, DEFAULT_MODELS_TOML)
}

/// Modelos do usuário, ou o padrão embutido se o arquivo não existir.
pub fn load_models(path: &Path) -> Result<ModelsConfig, ConfigError> {
    parse_models(&load_models_text(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_source_defaults_and_overrides() {
        let cfg = parse_config(
            "[[hosts]]\nname = \"a\"\n\n[[hosts]]\nname = \"b\"\nsource = \"native\"\n\n[[hosts]]\nname = \"c\"\nkind = \"ssh\"\ntarget = \"c\"\n",
        )
        .unwrap();
        assert_eq!(cfg.hosts[0].source, HostSource::Native);
        assert!(cfg.hosts[0].is_native());
        assert_eq!(cfg.hosts[1].source, HostSource::Native);
        assert_eq!(cfg.hosts[2].source, HostSource::Remote);
        assert!(!cfg.hosts[2].is_native());
        let bad_ssh = "[[hosts]]\nname = \"c\"\nkind = \"ssh\"\ntarget = \"c\"\nsource = \"native\"\n";
        assert!(parse_config(bad_ssh).is_err());
        assert!(parse_config("[[hosts]]\nname = \"a\"\nsource = \"x\"\n").is_err());
        let ssh_remote = "[[hosts]]\nname = \"c\"\nkind = \"ssh\"\ntarget = \"c\"\nsource = \"remote\"\n";
        assert_eq!(parse_config(ssh_remote).unwrap().hosts[0].source, HostSource::Remote);
        let e = parse_config("[[hosts]]\nname = \"a\"\nsource = \"remote\"\n").unwrap_err();
        assert!(e.to_string().contains("source remote so vale para kind ssh"), "{e}");
    }

    #[test]
    fn source_ccusage_is_rejected_with_migration_hint() {
        for kind in ["kind = \"local\"", "kind = \"ssh\"\ntarget = \"s\""] {
            let text = format!("[[hosts]]\nname = \"x\"\n{kind}\nsource = \"ccusage\"\n");
            let e = parse_config(&text).unwrap_err().to_string();
            assert!(e.starts_with("host x: source ccusage foi removido"), "{e}");
            assert!(e.contains("~/.local/bin/costs"), "{e}");
        }
    }

    #[test]
    fn hosts_and_default_agents() {
        let cfg =
            parse_config("[[hosts]]\nname = \"mac\"\n\n[[hosts]]\nname = \"srv\"\nkind = \"ssh\"\ntarget = \"srv\"\n")
                .unwrap();
        assert_eq!(cfg.hosts[0].kind, HostKind::Local);
        assert!(cfg.hosts[0].enabled);
        assert_eq!(cfg.hosts[1].target.as_deref(), Some("srv"));
        assert!(cfg.hosts[1].agents.contains(&"claude".to_string()));
    }

    #[test]
    fn top_level_agents_are_default_and_host_overrides() {
        let cfg = parse_config(
            "agents = [\"a\"]\n[[hosts]]\nname = \"x\"\n[[hosts]]\nname = \"y\"\nagents = [\"b\"]\npath_prefix = \"p\"\nenabled = false\n",
        )
        .unwrap();
        assert_eq!(cfg.hosts[0].agents, ["a"]);
        assert_eq!(cfg.hosts[1].agents, ["b"]);
        assert_eq!(cfg.hosts[1].path_prefix.as_deref(), Some("p"));
        assert!(!cfg.hosts[1].enabled);
    }

    #[test]
    fn ssh_without_target_fails() {
        let e = parse_config("[[hosts]]\nname = \"x\"\nkind = \"ssh\"\n").unwrap_err();
        assert_eq!(e.to_string(), "host x: kind ssh exige target");
    }

    #[test]
    fn invalid_kind_fails() {
        let e = parse_config("[[hosts]]\nname = \"x\"\nkind = \"ftp\"\n").unwrap_err();
        assert_eq!(e.to_string(), "host x: kind invalido \"ftp\"");
    }

    #[test]
    fn invalid_toml_fails() {
        assert!(matches!(parse_config("[[hosts"), Err(ConfigError::Toml(_))));
    }

    #[test]
    fn select_hosts_filters_and_errors() {
        let cfg = parse_config("[[hosts]]\nname = \"a\"\n[[hosts]]\nname = \"b\"\nenabled = false\n").unwrap();
        let on = select_hosts(&cfg.hosts, None).unwrap();
        assert_eq!(on.len(), 1);
        assert_eq!(on[0].name, "a");
        assert_eq!(select_hosts(&cfg.hosts, Some("b")).unwrap()[0].name, "b");
        let e = select_hosts(&cfg.hosts, Some("zz")).unwrap_err();
        assert_eq!(e.to_string(), "host desconhecido: zz");
    }

    #[test]
    fn subscriptions_load_and_missing_is_empty() {
        let cfg = parse_config("[[subscriptions]]\nname = \"OpenCode Go\"\nmonthly_usd = 10\n").unwrap();
        assert_eq!(
            cfg.subscriptions,
            [Subscription {
                name: "OpenCode Go".into(),
                provider: None,
                monthly_usd: 10.0
            }]
        );
        assert!(
            parse_config("agents = [\"claude\"]\n")
                .unwrap()
                .subscriptions
                .is_empty()
        );
    }

    #[test]
    fn parse_horizon_units() {
        assert_eq!(parse_horizon("5h").unwrap(), 5.0 * 3600.0);
        assert_eq!(parse_horizon("2d").unwrap(), 2.0 * 86400.0);
        assert_eq!(parse_horizon("90m").unwrap(), 5400.0);
        assert_eq!(parse_horizon(" 30 ").unwrap(), 30.0);
        assert_eq!(parse_horizon("1.5h").unwrap(), 5400.0);
        let e = parse_horizon("abc").unwrap_err();
        assert_eq!(e.to_string(), "horizonte invalido: \"abc\" (ex.: 5h, 2d)");
    }

    #[test]
    fn horizon_from_config() {
        assert_eq!(
            parse_config("[quota]\nhorizon = 24\n").unwrap().quota.horizon,
            24.0 * 3600.0
        );
        assert_eq!(parse_config("[quota]\nhorizon = 1.5\n").unwrap().quota.horizon, 5400.0);
        assert_eq!(
            parse_config("[quota]\nhorizon = \"2d\"\n").unwrap().quota.horizon,
            2.0 * 86400.0
        );
        assert_eq!(parse_config("[quota]\n").unwrap().quota.horizon, 24.0 * 3600.0);
        assert_eq!(parse_config("").unwrap().quota.horizon, 24.0 * 3600.0);
        assert!(parse_config("[quota]\nhorizon = \"x\"\n").is_err());
    }

    #[test]
    fn quota_cost_map_accepts_string_or_array() {
        let cfg = parse_config(
            "[quota]\nai_usage = \"/x/ai\"\nproviders = [\"claude\"]\n[quota.cost_map]\nclaude = \"anthropic\"\nxai = [\"xai\", \"cursor\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.quota.ai_usage, Some(PathBuf::from("/x/ai")));
        assert_eq!(cfg.quota.providers, ["claude"]);
        assert_eq!(cfg.quota.cost_map["claude"], ["anthropic"]);
        assert_eq!(cfg.quota.cost_map["xai"], ["xai", "cursor"]);
    }

    #[test]
    fn tilde_expansion() {
        let home = Path::new("/h");
        assert_eq!(expand_tilde_with("~/a/b", Some(home)), PathBuf::from("/h/a/b"));
        assert_eq!(expand_tilde_with("~", Some(home)), PathBuf::from("/h"));
        assert_eq!(expand_tilde_with("~x/a", Some(home)), PathBuf::from("~x/a"));
        assert_eq!(expand_tilde_with("/a", Some(home)), PathBuf::from("/a"));
        assert_eq!(expand_tilde_with("~/a", None), PathBuf::from("~/a"));
        let q = parse_config("[quota]\nai_usage = \"~/z\"\n").unwrap().quota;
        assert!(q.ai_usage.is_some());
    }

    #[test]
    fn paths_in_root() {
        let p = paths_in(Path::new("/r"));
        assert_eq!(p.config, PathBuf::from("/r/config.toml"));
        assert_eq!(p.models, PathBuf::from("/r/models.toml"));
        assert_eq!(p.db, PathBuf::from("/r/store.sqlite"));
        assert_eq!(p.prices, PathBuf::from("/r/prices.json"));
        assert_eq!(p.backup_dir, PathBuf::from("/r/backups"));
    }

    #[test]
    fn default_paths_are_absolute_or_env() {
        // Não altera o ambiente (set_var é unsafe); valida apenas a forma.
        let p = default_paths();
        assert!(p.config.ends_with("config.toml"));
        assert!(p.db.ends_with("store.sqlite"));
    }

    #[test]
    fn embedded_defaults_parse() {
        let cfg = parse_config(DEFAULT_CONFIG_TOML).unwrap();
        assert!(cfg.hosts.iter().any(|h| h.name == "local"));
        let m = parse_models(DEFAULT_MODELS_TOML).unwrap();
        assert!(!m.rules.is_empty());
        assert!(!m.price_map.is_empty());
    }

    #[test]
    fn user_files_win_and_missing_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths_in(dir.path());
        let cfg = load_config_or_default(&p.config).unwrap();
        assert!(cfg.hosts.iter().any(|h| h.name == "local"));
        assert!(!load_models(&p.models).unwrap().rules.is_empty());
        assert!(matches!(load_config(&p.config), Err(ConfigError::Io { .. })));

        std::fs::write(&p.config, "[[hosts]]\nname = \"only\"\n").unwrap();
        std::fs::write(
            &p.models,
            "[[rules]]\nmatch = \"^x\"\nprovider = \"px\"\n[[price_map]]\nmodel = \"m\"\nopenrouter = \"o/m\"\n",
        )
        .unwrap();
        assert_eq!(load_config(&p.config).unwrap().hosts.len(), 1);
        assert_eq!(load_config_or_default(&p.config).unwrap().hosts[0].name, "only");
        let m = load_models(&p.models).unwrap();
        assert_eq!(m.rules, [("^x".to_string(), "px".to_string())]);
        assert_eq!(m.price_map, [("m".to_string(), "o/m".to_string())]);
    }

    #[test]
    fn io_error_other_than_not_found_propagates() {
        let dir = tempfile::tempdir().unwrap();
        // Ler um diretório como arquivo não é NotFound.
        assert!(matches!(load_models_text(dir.path()), Err(ConfigError::Io { .. })));
    }
}
