//! Protocolo de `costs collect`: contexto (stdin) e resposta (stdout) trocados entre o
//! `costs` local e o `costs` instalado no host remoto.
//!
//! Contexto: `{"version":3,"models":"<models.toml>","prices":<prices.json>|null}`.
//! Resposta: `{"version":3,"agents":{"claude":{"rows":[...]},"kimi":{"error":".."}}}`.

use std::any::Any;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Row, Rules, load_rules, provider_of};
use crate::prices::PricesFile;
use crate::source::PriceTable;

/// Versão do protocolo; sobe quando contexto, resposta ou `Row` mudam de forma.
///
/// v2: `prices.json` passou a ser indexado pelo nome canônico do modelo (e não
/// pelo id do OpenRouter via `price_map`); um `costs` v1 no host remoto leria a
/// tabela errada e gravaria custo nulo.
///
/// v3: o Grok passou a separar o cache da entrada e o custo informado pelo agente
/// virou base `reported`; um v2 remoto gravaria a entrada do Grok duplicada.
pub const COLLECT_PROTOCOL_VERSION: u32 = 3;

/// Falha ao montar as regras e preços da coleta.
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("contexto invalido: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "contexto no protocolo v{0}, mas este costs (o que roda o collect) entende so a \
         v{COLLECT_PROTOCOL_VERSION}: atualize o costs mais antigo (local ou remoto)"
    )]
    Version(u32),
    #[error("models.toml invalido: {0}")]
    Models(String),
}

/// Só a versão, lida antes do resto (o formato pode mudar entre versões).
fn check_version(text: &str) -> Result<u32, serde_json::Error> {
    use serde::de::Error as _;

    let value: Value = serde_json::from_str(text)?;
    let object = value
        .as_object()
        .ok_or_else(|| serde_json::Error::custom("contexto nao e um objeto"))?;
    object
        .get("version")
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .ok_or_else(|| serde_json::Error::custom("campo `version` ausente ou invalido"))
}

#[derive(Serialize, Deserialize)]
struct WireContext {
    version: u32,
    models: String,
    prices: Option<PricesFile>,
}

/// Contexto enviado ao host remoto: o remoto não precisa de config, models nem prices.
pub fn context_json(models_toml: &str, prices: Option<&PricesFile>) -> Result<String, serde_json::Error> {
    serde_json::to_string(&WireContext {
        version: COLLECT_PROTOCOL_VERSION,
        models: models_toml.to_string(),
        prices: prices.cloned(),
    })
}

/// Regras de provedor e tabela de preços usadas pelos coletores.
pub struct CollectSetup {
    pub rules: Rules,
    pub prices: Option<PriceTable>,
}

impl CollectSetup {
    /// A partir do texto de `models.toml` e do `prices.json`; sem preços o custo fica nulo.
    pub fn new(models_toml: &str, prices: Option<PricesFile>) -> Result<Self, ContextError> {
        let rules = load_rules(models_toml).map_err(|e| ContextError::Models(e.to_string()))?;
        Ok(Self {
            rules,
            prices: prices.map(|file| PriceTable::new(&file)),
        })
    }

    /// A partir do contexto JSON recebido por stdin.
    pub fn from_context(text: &str) -> Result<Self, ContextError> {
        let version = check_version(text)?;
        if version != COLLECT_PROTOCOL_VERSION {
            return Err(ContextError::Version(version));
        }
        let ctx: WireContext = serde_json::from_str(text)?;
        Self::new(&ctx.models, ctx.prices)
    }
}

/// Resultado de um agente na resposta: `{"rows":[...]}` ou `{"error":".."}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentOutcome {
    Rows(Vec<Row>),
    Error(String),
}

/// Resposta de `costs collect`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectOutput {
    pub version: u32,
    pub agents: BTreeMap<String, AgentOutcome>,
}

impl CollectOutput {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// Coletor de um agente: (agente, host) -> linhas ou mensagem de erro.
pub type AgentCollect<'a> = dyn Fn(&str, &str) -> Result<Vec<Row>, String> + 'a;

fn panic_text(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("?")
}

/// Um agente; pânico no coletor vira erro do agente em vez de derrubar a coleta toda.
fn collect_one(agent: &str, host: &str, collect: &AgentCollect<'_>) -> AgentOutcome {
    match catch_unwind(AssertUnwindSafe(|| collect(agent, host))) {
        Ok(Ok(rows)) => AgentOutcome::Rows(rows),
        Ok(Err(msg)) => AgentOutcome::Error(msg),
        Err(payload) => AgentOutcome::Error(format!("panic no coletor: {}", panic_text(payload.as_ref()))),
    }
}

/// Coleta cada agente com `collect(agente, host)`; falha (ou pânico) de um não afeta os outros.
pub fn collect_agents(host: &str, agents: &[String], collect: &AgentCollect<'_>) -> CollectOutput {
    let agents = agents
        .iter()
        .map(|agent| (agent.clone(), collect_one(agent, host, collect)))
        .collect();
    CollectOutput {
        version: COLLECT_PROTOCOL_VERSION,
        agents,
    }
}

/// Interpreta a resposta do host remoto. `Err` vale para todos os agentes do host.
pub fn parse_response(text: &str, host: &str) -> Result<BTreeMap<String, AgentOutcome>, String> {
    let invalid = |e: serde_json::Error| format!("saida remota invalida: {e}");
    let version = check_version(text.trim()).map_err(invalid)?;
    if version != COLLECT_PROTOCOL_VERSION {
        return Err(format!(
            "costs em {host} usa o protocolo v{version}, o local usa v{COLLECT_PROTOCOL_VERSION}: \
             atualize o ~/.local/bin/costs dos dois lados para a mesma versao"
        ));
    }
    let out: CollectOutput = serde_json::from_str(text.trim()).map_err(invalid)?;
    Ok(out.agents)
}

/// Linhas recebidas: host e agente são os configurados aqui e o provedor sai das regras
/// locais (o remoto pode ter regras velhas ou rótulo diferente).
pub fn adopt_rows(mut rows: Vec<Row>, host: &str, agent: &str, rules: &Rules) -> Vec<Row> {
    for r in &mut rows {
        r.host = host.to_string();
        r.agent = agent.to_string();
        r.provider = provider_of(&r.model, agent, rules);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prices::ModelPrice;
    use proptest::prelude::*;
    use std::collections::HashMap;

    const MODELS: &str = "[[rules]]\nmatch = \"^claude\"\nprovider = \"anthropic\"\n\
                          [[price_map]]\nmodel = \"claude-x\"\nopenrouter = \"anthropic/claude-x\"\n";

    fn prices() -> PricesFile {
        PricesFile {
            fetched_at: "2026-05-01T00:00:00".into(),
            source: "s".into(),
            models: HashMap::from([(
                "claude-x".to_string(),
                ModelPrice {
                    prompt: 1e-6,
                    ..ModelPrice::default()
                },
            )]),
            by_id: HashMap::new(),
            resolved: HashMap::from([(
                "claude-x".to_string(),
                crate::prices::ResolvedPrice {
                    id: "anthropic/claude-x".into(),
                    how: "exact".into(),
                    source: "litellm".into(),
                },
            )]),
        }
    }

    fn row(model: &str, cost: Option<f64>) -> Row {
        Row {
            host: "remoto".into(),
            agent: "claude".into(),
            date: "2026-05-01".into(),
            model: model.into(),
            model_raw: model.into(),
            provider: "outro".into(),
            input: 10,
            output: 2,
            cache_read: 3,
            cache_write: 4,
            cost_usd: cost,
            cost_basis: "estimated".into(),
        }
    }

    #[test]
    fn context_round_trip_builds_rules_and_prices() {
        let text = context_json(MODELS, Some(&prices())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["version"], COLLECT_PROTOCOL_VERSION);
        assert_eq!(v["models"], MODELS);
        let setup = CollectSetup::from_context(&text).unwrap();
        assert_eq!(setup.rules.len(), 1);
        let table = setup.prices.unwrap();
        assert_eq!(table.lookup("claude-x").map(|p| p.price.prompt), Some(1e-6));
    }

    #[test]
    fn context_with_null_prices_has_no_table() {
        let text = format!(
            r#"{{"version":3,"models":{},"prices":null}}"#,
            serde_json::to_string(MODELS).unwrap()
        );
        let setup = CollectSetup::from_context(&text).unwrap();
        assert!(setup.prices.is_none());
        assert_eq!(setup.rules.len(), 1);
        assert!(context_json("", None).unwrap().ends_with(r#""prices":null}"#));
    }

    #[test]
    fn context_with_other_version_is_rejected() {
        let e = CollectSetup::from_context(r#"{"version":1,"anything":true}"#)
            .err()
            .unwrap();
        assert!(matches!(e, ContextError::Version(1)));
        assert!(e.to_string().contains("atualize o costs mais antigo"), "{e}");
        let local = format!("v{COLLECT_PROTOCOL_VERSION}");
        assert!(e.to_string().contains("v1") && e.to_string().contains(&local), "{e}");
    }

    #[test]
    fn context_malformed_or_bad_models_is_an_error() {
        for bad in ["", "nao json", "{}", r#"{"version":3}"#] {
            let e = CollectSetup::from_context(bad).err().unwrap();
            assert!(matches!(e, ContextError::Json(_)), "{bad}");
        }
        let bad_toml = r#"{"version":3,"models":"[[rules","prices":null}"#;
        let e = CollectSetup::from_context(bad_toml).err().unwrap();
        assert!(e.to_string().starts_with("models.toml invalido"), "{e}");
        let bad_regex = r#"{"version":3,"models":"[[rules]]\nmatch = \"(\"\nprovider = \"x\"\n","prices":null}"#;
        assert!(matches!(
            CollectSetup::from_context(bad_regex),
            Err(ContextError::Models(_))
        ));
    }

    #[test]
    fn table_comes_from_canonical_keys_not_price_map() {
        // `price_map` deixou de ser lido na coleta: o preço já vem resolvido por nome.
        let models = "price_map = 3\n";
        let setup = CollectSetup::new(models, Some(prices())).unwrap();
        assert_eq!(setup.prices.unwrap().len(), 1);
        assert!(CollectSetup::new(models, None).unwrap().prices.is_none());
    }

    #[test]
    fn collect_output_has_rows_and_errors_per_agent() {
        let agents = ["claude".to_string(), "kimi".to_string()];
        let out = collect_agents("srv", &agents, &|agent, host| {
            assert_eq!(host, "srv");
            match agent {
                "claude" => Ok(vec![row("claude-x", Some(0.5))]),
                _ => Err("sem logs".into()),
            }
        });
        let v: serde_json::Value = serde_json::from_str(&out.to_json().unwrap()).unwrap();
        assert_eq!(v["version"], COLLECT_PROTOCOL_VERSION);
        assert_eq!(v["agents"]["kimi"], serde_json::json!({"error": "sem logs"}));
        let first = &v["agents"]["claude"]["rows"][0];
        assert_eq!(first["model"], "claude-x");
        assert_eq!(first["cost_usd"], 0.5);
        assert_eq!(first["cache_write"], 4);
    }

    #[test]
    fn panic_in_one_collector_does_not_abort_the_others() {
        let agents = ["claude".to_string(), "kimi".to_string(), "pi".to_string()];
        let out = collect_agents("srv", &agents, &|agent, _| match agent {
            "kimi" => panic!("indice fora"),
            "pi" => std::panic::panic_any(format!("dono {agent}")),
            _ => Ok(vec![row("claude-x", None)]),
        });
        assert_eq!(out.agents["claude"], AgentOutcome::Rows(vec![row("claude-x", None)]));
        assert_eq!(
            out.agents["kimi"],
            AgentOutcome::Error("panic no coletor: indice fora".into())
        );
        assert_eq!(
            out.agents["pi"],
            AgentOutcome::Error("panic no coletor: dono pi".into())
        );
        assert_eq!(panic_text(&7_u8), "?");
    }

    #[test]
    fn parse_response_ok_version_mismatch_and_malformed() {
        let out = collect_agents("h", &["pi".to_string()], &|_, _| Ok(vec![row("m", None)]));
        let map = parse_response(&format!("{}\n", out.to_json().unwrap()), "h").unwrap();
        assert_eq!(map["pi"], AgentOutcome::Rows(vec![row("m", None)]));

        let e = parse_response(r#"{"version":9,"agents":{}}"#, "srv").unwrap_err();
        assert!(e.contains("srv") && e.contains("v9") && e.contains("atualize"), "{e}");

        for bad in [
            "",
            "lixo",
            "[1]",
            r#"{"agents":{}}"#,
            r#"{"version":3,"agents":{"pi":{"x":1}}}"#,
        ] {
            let e = parse_response(bad, "h").unwrap_err();
            assert!(e.starts_with("saida remota invalida"), "{bad}: {e}");
        }
    }

    #[test]
    fn adopt_rows_relabels_and_recomputes_provider() {
        let rules = load_rules(MODELS).unwrap();
        let mut foreign = row("claude-x", Some(1.0));
        foreign.host = "outro-nome".into();
        foreign.agent = "pi".into();
        foreign.provider = "xyz".into();
        let rows = adopt_rows(vec![foreign, row("gpt-9", None)], "srv", "codex", &rules);
        assert_eq!(rows[0].host, "srv");
        assert_eq!(rows[0].agent, "codex");
        assert_eq!(rows[0].provider, "anthropic");
        assert_eq!(rows[1].provider, "openai"); // codex sem regra
        assert_eq!(rows[0].cost_usd, Some(1.0));
    }

    fn arb_row() -> impl Strategy<Value = Row> {
        (
            ("\\PC{0,12}", "[a-z]{1,8}", "\\PC{0,10}", "\\PC{0,16}"),
            (any::<i64>(), any::<i64>(), any::<i64>(), any::<i64>()),
            proptest::option::of(proptest::num::f64::NORMAL | proptest::num::f64::ZERO),
            "[a-z]{0,10}",
        )
            .prop_map(|((host, agent, date, model), (i, o, cr, cw), cost, basis)| Row {
                host,
                agent,
                date,
                model_raw: format!("raw/{model}"),
                model,
                provider: "p".into(),
                input: i,
                output: o,
                cache_read: cr,
                cache_write: cw,
                cost_usd: cost,
                cost_basis: basis,
            })
    }

    proptest! {
        #[test]
        fn wire_round_trip_preserves_rows(
            batches in prop::collection::btree_map("[a-z]{1,6}", prop::collection::vec(arb_row(), 0..8), 0..4),
        ) {
            let agents: Vec<String> = batches.keys().cloned().collect();
            let out = collect_agents("h", &agents, &|agent, _| Ok(batches[agent].clone()));
            let parsed = parse_response(&out.to_json().unwrap(), "h").unwrap();
            prop_assert_eq!(parsed.len(), batches.len());
            for (agent, rows) in &batches {
                prop_assert_eq!(&parsed[agent], &AgentOutcome::Rows(rows.clone()));
            }
        }
    }
}
