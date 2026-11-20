//! Coleta nativa dos agentes do host onde o `costs` roda: parsers dos logs e o `grok`.

use std::path::{Path, PathBuf};

use crate::model::{Row, Rules, parse_daily, provider_of};
use crate::source::{LocalDate, PriceTable, SourceError, claude, codex, kimi, local_date, opencode, pi, zcode};

/// Agente coletado pelo `grok usage` (os demais vêm dos logs).
pub const GROK_AGENT: &str = "grok";

/// Agentes com coletor nativo.
pub const NATIVE_AGENTS: [&str; 7] = ["claude", "codex", "pi", "opencode", "zcode", "kimi", GROK_AGENT];

/// Coletor do Grok: devolve o JSON diário (injetável nos testes).
pub type GrokRunner = fn() -> Result<String, String>;

/// Coletor real do Grok: sessões e binário `grok` do host.
pub fn grok_real() -> Result<String, String> {
    crate::grok::run_collect_real().map_err(|e| e.to_string())
}

fn home() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_default()
}

fn env_dir(name: &str, fallback: impl FnOnce() -> PathBuf) -> PathBuf {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map_or_else(fallback, PathBuf::from)
}

/// Os parsers marcam tudo como "outro": recalcula o provedor pelas regras de `models.toml`.
fn with_providers(mut rows: Vec<Row>, agent: &str, rules: &Rules) -> Vec<Row> {
    for r in &mut rows {
        r.provider = provider_of(&r.model, agent, rules);
    }
    rows
}

type DirParser = fn(&Path, &str, LocalDate, Option<&PriceTable>) -> Result<Vec<Row>, SourceError>;

fn run(
    agent: &str,
    host: &str,
    rules: &Rules,
    prices: Option<&PriceTable>,
    day_of: LocalDate,
) -> Result<Vec<Row>, SourceError> {
    let home = home();
    let root = |var: &str, rel: &str| -> PathBuf { env_dir(var, || home.join(rel)) };
    let one = |f: DirParser, p: PathBuf| f(&p, host, day_of, prices);
    match agent {
        "claude" => {
            let env = std::env::var("CLAUDE_CONFIG_DIR").ok();
            let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
            let roots = claude::default_roots(env.as_deref(), Some(&home), xdg.as_deref());
            claude::daily_rows(&roots, host, rules, day_of, prices)
        }
        "codex" => codex::daily_rows(&codex::default_roots(), host, day_of, prices),
        "pi" => one(pi::daily_rows, root("PI_AGENT_DIR", ".pi/agent/sessions")),
        "opencode" => one(opencode::daily_rows, root("OPENCODE_DATA_DIR", ".local/share/opencode")),
        "zcode" => one(zcode::daily_rows, root("ZCODE_HOME", ".zcode")),
        "kimi" => one(kimi::daily_rows, root("KIMI_DATA_DIR", ".kimi-code")),
        _ => Ok(Vec::new()),
    }
    .map(|rows| with_providers(rows, agent, rules))
}

/// Coletores nativos do host com as regras e preços já carregados.
pub struct Sources<'a> {
    pub rules: &'a Rules,
    pub prices: Option<&'a PriceTable>,
    pub grok: GrokRunner,
}

impl Sources<'_> {
    /// Linhas diárias do agente. `Err` descreve a falha (isolada por agente).
    pub fn collect(&self, agent: &str, host: &str) -> Result<Vec<Row>, String> {
        if !NATIVE_AGENTS.contains(&agent) {
            return Err(format!("agente sem coletor nativo: {agent}"));
        }
        if agent == GROK_AGENT {
            return self.collect_grok(host);
        }
        run(agent, host, self.rules, self.prices, local_date).map_err(|e| e.to_string())
    }

    fn collect_grok(&self, host: &str) -> Result<Vec<Row>, String> {
        let text = (self.grok)()?;
        parse_daily(host, GROK_AGENT, &text, self.rules).map_err(|e| format!("saida do grok invalida: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::load_rules;

    fn row(model: &str) -> Row {
        Row {
            host: "h".into(),
            agent: "a".into(),
            date: "2026-05-01".into(),
            model: model.into(),
            model_raw: model.into(),
            provider: "outro".into(),
            input: 1,
            output: 1,
            cache_read: 0,
            cache_write: 0,
            cost_usd: None,
            cost_basis: "estimated".into(),
        }
    }

    const GROK_JSON: &str = r#"{"daily":[{"date":"2026-05-01","modelBreakdowns":[{"modelName":"grok-4","inputTokens":7,"outputTokens":3,"cost":0.25}]}]}"#;

    fn sources(rules: &Rules, grok: GrokRunner) -> Sources<'_> {
        Sources {
            rules,
            prices: None,
            grok,
        }
    }

    #[test]
    fn provider_is_recomputed_from_rules() {
        let rules = load_rules("[[rules]]\nmatch = \"^claude\"\nprovider = \"anthropic\"\n").unwrap();
        let rows = with_providers(vec![row("claude-sonnet-4"), row("zzz")], "pi", &rules);
        assert_eq!(rows[0].provider, "anthropic");
        assert_eq!(rows[1].provider, "outro");
        let codex = with_providers(vec![row("zzz")], "codex", &rules);
        assert_eq!(codex[0].provider, "openai");
    }

    #[test]
    fn unknown_agent_is_an_error() {
        let rules = load_rules("").unwrap();
        let s = sources(&rules, || panic!("grok nao deve rodar"));
        assert!(s.collect("nada", "h").unwrap_err().contains("nada"));
    }

    #[test]
    fn grok_uses_injected_runner_and_rules() {
        let rules = load_rules("[[rules]]\nmatch = \"^grok\"\nprovider = \"xai\"\n").unwrap();
        let rows = sources(&rules, || Ok(GROK_JSON.to_string()))
            .collect("grok", "srv")
            .unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!((r.host.as_str(), r.agent.as_str()), ("srv", "grok"));
        assert_eq!((r.model.as_str(), r.provider.as_str()), ("grok-4", "xai"));
        assert_eq!((r.input, r.output, r.cost_usd), (7, 3, Some(0.25)));
    }

    #[test]
    fn grok_runner_failure_and_bad_output_are_errors() {
        let rules = load_rules("").unwrap();
        let e = sources(&rules, || Err("grok ausente".into()))
            .collect("grok", "h")
            .unwrap_err();
        assert_eq!(e, "grok ausente");
        let e = sources(&rules, || Ok("lixo".into())).collect("grok", "h").unwrap_err();
        assert!(e.starts_with("saida do grok invalida"), "{e}");
    }
}
