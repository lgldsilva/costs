//! Modelo de dados: linhas normalizadas, nomes canônicos, provedores e parsers.

use std::collections::HashMap;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const DEFAULT_BASIS: &str = "estimated";
const ALLOCATED: &str = "allocated";
/// Custo informado pelo próprio agente: o `repricing` não sobrescreve.
pub const REPORTED: &str = "reported";
const UNKNOWN_MODEL: &str = "?";
const ANY_MODEL: &str = "*";

/// Falhas ao carregar regras ou interpretar o JSON diário dos coletores.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("JSON inválido: {0}")]
    Json(#[from] serde_json::Error),
    #[error("TOML inválido: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("regex inválida em models.toml: {0}")]
    Regex(#[from] regex::Error),
    #[error("campo obrigatório ausente: {0}")]
    MissingField(&'static str),
}

/// Linha normalizada de uso.
///
/// Também é o formato das linhas no protocolo de `costs collect` (`remote`): mudar
/// campos aqui exige subir `remote::COLLECT_PROTOCOL_VERSION`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub host: String,
    pub agent: String,
    pub date: String,
    /// Nome canônico.
    pub model: String,
    pub model_raw: String,
    pub provider: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost_usd: Option<f64>,
    pub cost_basis: String,
}

/// Regras `(padrão, provedor)` em ordem de prioridade.
pub type Rules = Vec<(Regex, String)>;

#[derive(Deserialize)]
struct RawRule {
    #[serde(rename = "match")]
    pattern: String,
    provider: String,
}

#[derive(Deserialize, Default)]
struct RawRules {
    #[serde(default)]
    rules: Vec<RawRule>,
}

/// Carrega as `[[rules]]` a partir do texto TOML de `models.toml`.
pub fn load_rules(toml_text: &str) -> Result<Rules, ParseError> {
    let raw: RawRules = toml::from_str(toml_text)?;
    raw.rules
        .into_iter()
        .map(|r| Ok((Regex::new(&r.pattern)?, r.provider)))
        .collect()
}

/// Nome canônico: tira prefixo `[pi] `, rota `z-ai/`, sufixo `:cloud` e `-free`.
pub fn canon_model(raw: &str) -> String {
    let mut m = raw.trim();
    m = m.strip_prefix("[pi] ").unwrap_or(m);
    m = m.split(':').next().unwrap_or(m);
    m = m.rsplit('/').next().unwrap_or(m);
    let lower = m.to_lowercase();
    lower.strip_suffix("-free").unwrap_or(&lower).trim().to_string()
}

/// Provedor: primeira regra que casa; senão `openai` para codex; senão `outro`.
pub fn provider_of(canon: &str, agent: &str, rules: &Rules) -> String {
    if let Some((_, provider)) = rules.iter().find(|(p, _)| p.is_match(canon)) {
        return provider.clone();
    }
    if agent == "codex" { "openai" } else { "outro" }.to_string()
}

fn int_of(v: Option<&Value>) -> i64 {
    v.and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

fn float_of(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

fn tokens(src: &Value) -> [i64; 4] {
    ["inputTokens", "outputTokens", "cacheReadTokens", "cacheCreationTokens"].map(|k| int_of(src.get(k)).max(0))
}

struct Ctx<'a> {
    host: &'a str,
    agent: &'a str,
    rules: &'a Rules,
}

impl Ctx<'_> {
    fn row(&self, date: &str, raw_model: &str, src: &Value, cost: Option<f64>, basis: &str) -> Row {
        let [input, output, cache_read, cache_write] = tokens(src);
        let model = canon_model(raw_model);
        Row {
            host: self.host.to_string(),
            agent: self.agent.to_string(),
            date: date.to_string(),
            provider: provider_of(&model, self.agent, self.rules),
            model,
            model_raw: raw_model.to_string(),
            input,
            output,
            cache_read,
            cache_write,
            cost_usd: cost,
            cost_basis: basis.to_string(),
        }
    }
}

fn str_field<'a>(v: &'a Value, key: &'static str) -> Result<&'a str, ParseError> {
    v.get(key).and_then(Value::as_str).ok_or(ParseError::MissingField(key))
}

fn models_map(v: &Value) -> Option<&Map<String, Value>> {
    v.get("models").and_then(Value::as_object)
}

/// Custo presente no JSON diário veio do agente (ex.: `costUsdTicks` do Grok).
fn basis_of(cost: Option<f64>) -> &'static str {
    if cost.is_some() { REPORTED } else { DEFAULT_BASIS }
}

/// JSON diário `{"daily": [...]}` (formato diário, compatível com ccusage `daily --json`);
/// é o que o coletor do Grok emite.
///
/// Com `modelBreakdowns`: custo por modelo. Sem: `models` e custo só no dia;
/// um modelo usa o nome, vários viram `*`.
pub fn parse_daily(host: &str, agent: &str, text: &str, rules: &Rules) -> Result<Vec<Row>, ParseError> {
    let data: Value = serde_json::from_str(text)?;
    let ctx = Ctx { host, agent, rules };
    let mut rows = Vec::new();
    let days = data.get("daily").and_then(Value::as_array);
    for day in days.into_iter().flatten() {
        let date = str_field(day, "date")?;
        let breakdown = day
            .get("modelBreakdowns")
            .and_then(Value::as_array)
            .filter(|b| !b.is_empty());
        if let Some(breakdown) = breakdown {
            for m in breakdown {
                let name = m.get("modelName").and_then(Value::as_str).unwrap_or(UNKNOWN_MODEL);
                let cost = float_of(m.get("cost"));
                rows.push(ctx.row(date, name, m, cost, basis_of(cost)));
            }
            continue;
        }
        let model = match models_map(day) {
            Some(m) if m.len() == 1 => m.keys().next().map_or(ANY_MODEL, String::as_str),
            _ => ANY_MODEL,
        };
        let cost = match day.get("costUSD") {
            Some(c) => float_of(Some(c)),
            None => float_of(day.get("totalCost")),
        };
        rows.push(ctx.row(date, model, day, cost, basis_of(cost)));
    }
    Ok(merge_same_model(rows))
}

/// Precedência ao juntar parcelas do mesmo dia/modelo canônico. `reported` vence as
/// estimativas para o `repricing` não apagar a parcela informada pelo agente; um preço
/// aproximado (`fallback`) vence `estimated` e marca a linha para o breakdown.
pub(crate) fn stronger_basis(old: Option<&str>, new: &str) -> String {
    old.filter(|o| basis_rank(o) >= basis_rank(new))
        .unwrap_or(new)
        .to_string()
}

/// Força de uma base de custo: maior vence ao juntar parcelas ou ao regravar um dia.
pub(crate) fn basis_rank(basis: &str) -> u8 {
    match basis {
        ALLOCATED => 5,
        REPORTED => 4,
        "fallback" => 3,
        DEFAULT_BASIS => 2,
        "free" => 1,
        _ => 0,
    }
}

/// Custos iguais a menos de ruído de soma em ponto flutuante (a ordem das parcelas muda
/// entre processos); `None` só é igual a `None`.
pub fn cost_eq(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => (x - y).abs() <= COST_EPSILON * x.abs().max(y.abs()).max(1.0),
        (None, None) => true,
        _ => false,
    }
}

const COST_EPSILON: f64 = 1e-9;

/// Soma linhas com mesmo (data, modelo canônico). Custo `None` só se todas as
/// parcelas forem `None`; base `allocated` se alguma parcela for.
pub fn merge_same_model(rows: Vec<Row>) -> Vec<Row> {
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    let mut out: Vec<Row> = Vec::new();
    for r in rows {
        let key = (r.date.clone(), r.model.clone());
        let Some(&i) = index.get(&key) else {
            index.insert(key, out.len());
            out.push(r);
            continue;
        };
        let cur = &mut out[i];
        cur.cost_usd = match (cur.cost_usd, r.cost_usd) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
        cur.cost_basis = stronger_basis(Some(&cur.cost_basis), &r.cost_basis);
        cur.input += r.input;
        cur.output += r.output;
        cur.cache_read += r.cache_read;
        cur.cache_write += r.cache_write;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    const RULES_TOML: &str = r#"
[[rules]]
match = "^antigravity-claude"
provider = "anthropic"
[[rules]]
match = "^(claude|anthropic)"
provider = "anthropic"
[[rules]]
match = "^(gpt|o[0-9]|codex|chatgpt)"
provider = "openai"
[[rules]]
match = "^(glm|z-ai)"
provider = "zhipu"
[[rules]]
match = "^(kimi|k3|moonshot)"
provider = "moonshot"
[[rules]]
match = "^(gemini|antigravity-gemini)"
provider = "google"
"#;

    fn rules() -> Rules {
        load_rules(RULES_TOML).unwrap()
    }

    fn claude_json() -> String {
        json!({"daily": [{
            "date": "2026-10-01", "inputTokens": 100, "outputTokens": 20,
            "cacheReadTokens": 300, "cacheCreationTokens": 5, "totalCost": 3.0,
            "modelBreakdowns": [
                {"modelName": "claude-opus-4-6", "inputTokens": 80, "outputTokens": 15,
                 "cacheReadTokens": 200, "cacheCreationTokens": 5, "cost": 2.5},
                {"modelName": "claude-haiku", "inputTokens": 20, "outputTokens": 5,
                 "cacheReadTokens": 100, "cacheCreationTokens": 0, "cost": 0.5},
            ],
        }]})
        .to_string()
    }

    fn codex_daily_json() -> String {
        json!({"daily": [{
            "date": "2026-09-18", "inputTokens": 89334, "outputTokens": 16003,
            "cacheReadTokens": 1712896, "cacheCreationTokens": 0, "costUSD": 0.07,
            "models": {"gpt-5.6-luna": {"inputTokens": 89334}},
        }, {
            "date": "2026-09-19", "inputTokens": 10, "outputTokens": 2,
            "cacheReadTokens": 0, "cacheCreationTokens": 0, "costUSD": 0.01,
            "models": {"gpt-a": {}, "gpt-b": {}},
        }]})
        .to_string()
    }

    fn find<'a>(rows: &'a [Row], model: &str) -> &'a Row {
        rows.iter().find(|r| r.model == model).unwrap()
    }

    #[test]
    fn claude_breakdown_one_row_per_model() {
        let rows = parse_daily("mac", "claude", &claude_json(), &rules()).unwrap();
        assert_eq!(rows.len(), 2);
        let opus = find(&rows, "claude-opus-4-6");
        assert_eq!(opus.cost_usd, Some(2.5));
        assert_eq!((opus.input, opus.output, opus.cache_read), (80, 15, 200));
        assert_eq!((opus.host.as_str(), opus.agent.as_str()), ("mac", "claude"));
        assert_eq!(opus.cost_basis, "reported", "custo do JSON diário vem do agente");
    }

    #[test]
    fn codex_daily_single_model_uses_name_and_day_cost() {
        let rows = parse_daily("srv", "codex", &codex_daily_json(), &rules()).unwrap();
        assert_eq!(rows[0].model, "gpt-5.6-luna");
        assert_eq!(rows[0].cost_usd, Some(0.07));
        assert_eq!(rows[0].cache_read, 1_712_896);
    }

    #[test]
    fn codex_daily_many_models_become_star() {
        let rows = parse_daily("srv", "codex", &codex_daily_json(), &rules()).unwrap();
        assert_eq!(rows[1].model, "*");
        assert_eq!(rows[1].cost_usd, Some(0.01));
    }

    #[test]
    fn empty_daily_returns_empty() {
        assert!(
            parse_daily("mac", "pi", r#"{"daily": []}"#, &rules())
                .unwrap()
                .is_empty()
        );
        assert!(parse_daily("mac", "pi", "{}", &rules()).unwrap().is_empty());
    }

    #[test]
    fn missing_cost_stays_none_not_zero() {
        let text = json!({"daily": [{"date": "2026-01-01", "inputTokens": 1, "outputTokens": 1,
            "modelBreakdowns": [{"modelName": "m"}]}]})
        .to_string();
        let rows = parse_daily("mac", "kimi", &text, &rules()).unwrap();
        assert_eq!(rows[0].cost_usd, None);
        assert_eq!(rows[0].cost_basis, "estimated");
    }

    #[test]
    fn cost_eq_ignora_ruido_de_soma_mas_nao_diferenca_real() {
        assert!(cost_eq(Some(0.1 + 0.2), Some(0.3)));
        assert!(cost_eq(Some(1234.5), Some(1234.5 + 1e-7)), "relativo ao valor");
        assert!(!cost_eq(Some(1.0), Some(1.0001)));
        assert!(!cost_eq(Some(0.0), Some(1e-6)));
        assert!(cost_eq(None, None));
        assert!(!cost_eq(Some(0.0), None));
    }

    #[test]
    fn stronger_basis_reported_beats_estimates_but_not_allocated() {
        assert_eq!(stronger_basis(Some("estimated"), "reported"), "reported");
        assert_eq!(stronger_basis(Some("reported"), "fallback"), "reported");
        assert_eq!(stronger_basis(Some("reported"), "allocated"), "allocated");
        assert_eq!(stronger_basis(Some("free"), "estimated"), "estimated");
        assert_eq!(stronger_basis(None, "free"), "free");
    }

    #[test]
    fn day_without_models_uses_total_cost_fallback() {
        let text = json!({"daily": [{"date": "d", "inputTokens": 4, "totalCost": 1.5}]}).to_string();
        let rows = parse_daily("mac", "x", &text, &rules()).unwrap();
        assert_eq!(rows[0].model, "*");
        assert_eq!(rows[0].cost_usd, Some(1.5));
        assert_eq!(rows[0].input, 4);
    }

    #[test]
    fn breakdown_without_model_name_is_question_mark() {
        let text = json!({"daily": [{"date": "d", "modelBreakdowns": [{"cost": 1.0}]}]}).to_string();
        let rows = parse_daily("mac", "x", &text, &rules()).unwrap();
        assert_eq!(rows[0].model, "?");
    }

    #[test]
    fn none_cost_survives_merge_only_if_all_none() {
        let text = json!({"daily": [{"date": "2026-10-09", "modelBreakdowns": [
            {"modelName": "x", "inputTokens": 1},
            {"modelName": "x-free", "inputTokens": 1},
        ]}]})
        .to_string();
        let rows = parse_daily("mac", "opencode", &text, &rules()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input, 2);
        assert_eq!(rows[0].cost_usd, None);
    }

    #[test]
    fn merge_mixed_none_and_value_sums() {
        let text = json!({"daily": [{"date": "d", "modelBreakdowns": [
            {"modelName": "x", "cost": 1.0}, {"modelName": "x-free"},
        ]}]})
        .to_string();
        let rows = parse_daily("mac", "opencode", &text, &rules()).unwrap();
        assert_eq!(rows[0].cost_usd, Some(1.0));
    }

    #[test]
    fn negative_tokens_clamped_to_zero() {
        let text = json!({"daily": [{"date": "d", "modelBreakdowns": [
            {"modelName": "m", "inputTokens": -5, "outputTokens": 3},
        ]}]})
        .to_string();
        let rows = parse_daily("mac", "x", &text, &rules()).unwrap();
        assert_eq!((rows[0].input, rows[0].output), (0, 3));
    }

    #[test]
    fn invalid_json_and_missing_date_are_errors() {
        assert!(matches!(
            parse_daily("m", "x", "nope", &rules()),
            Err(ParseError::Json(_))
        ));
        let text = json!({"daily": [{"inputTokens": 1}]}).to_string();
        assert!(matches!(
            parse_daily("m", "x", &text, &rules()),
            Err(ParseError::MissingField("date"))
        ));
    }

    #[test]
    fn canon_strips_prefix_route_and_free() {
        assert_eq!(canon_model("[pi] gpt-5.6-luna"), "gpt-5.6-luna");
        assert_eq!(canon_model("z-ai/glm-5.2"), "glm-5.2");
        assert_eq!(canon_model("deepseek-v4-flash-free"), "deepseek-v4-flash");
        assert_eq!(canon_model("minimax-m3:cloud"), "minimax-m3");
        assert_eq!(canon_model("Kimi-For-Coding"), "kimi-for-coding");
    }

    #[test]
    fn canon_model_trims_after_colon_split_and_is_idempotent() {
        let raw = "\u{bbe} :";
        let once = canon_model(raw);
        assert_eq!(once, "\u{bbe}");
        assert_eq!(canon_model(&once), once);
    }

    #[test]
    fn provider_by_rule() {
        let r = rules();
        assert_eq!(provider_of("claude-opus-5-5", "claude", &r), "anthropic");
        assert_eq!(
            provider_of("antigravity-claude-opus-4-6-thinking", "opencode", &r),
            "anthropic"
        );
        assert_eq!(provider_of("antigravity-gemini-3.5-flash", "opencode", &r), "google");
        assert_eq!(provider_of("glm-5.3", "zcode", &r), "zhipu");
        assert_eq!(provider_of("kimi-for-coding", "kimi", &r), "moonshot");
        assert_eq!(provider_of("space-bunny", "opencode", &r), "outro");
    }

    #[test]
    fn codex_without_match_is_openai() {
        assert_eq!(provider_of("modelo-novo", "codex", &rules()), "openai");
    }

    #[test]
    fn provider_independent_of_harness() {
        let r = rules();
        assert_eq!(
            provider_of("claude-haiku-5-5", "opencode", &r),
            provider_of("claude-haiku-5-5", "claude", &r)
        );
    }

    #[test]
    fn load_rules_errors_and_empty() {
        assert!(load_rules("").unwrap().is_empty());
        assert!(matches!(
            load_rules("[[rules]]\nmatch = \"(\"\nprovider = \"x\"\n"),
            Err(ParseError::Regex(_))
        ));
        assert!(matches!(load_rules("= ="), Err(ParseError::Toml(_))));
    }

    fn arb_row() -> impl Strategy<Value = Row> {
        (
            "[ab]",
            "[xy]",
            (0..1000i64, 0..1000i64, 0..1000i64, 0..1000i64),
            proptest::option::of(0.0..100.0f64),
            prop::bool::ANY,
        )
            .prop_map(|(date, model, (i, o, cr, cw), cost, alloc)| Row {
                host: "h".into(),
                agent: "a".into(),
                date,
                model: model.clone(),
                model_raw: model,
                provider: "p".into(),
                input: i,
                output: o,
                cache_read: cr,
                cache_write: cw,
                cost_usd: cost,
                cost_basis: if alloc { "allocated" } else { "estimated" }.into(),
            })
    }

    proptest! {
        #[test]
        fn merge_preserves_token_sums(rows in prop::collection::vec(arb_row(), 0..30)) {
            let sum = |rs: &[Row], f: fn(&Row) -> i64| rs.iter().map(f).sum::<i64>();
            let merged = merge_same_model(rows.clone());
            prop_assert_eq!(sum(&rows, |r| r.input), sum(&merged, |r| r.input));
            prop_assert_eq!(sum(&rows, |r| r.output), sum(&merged, |r| r.output));
            prop_assert_eq!(sum(&rows, |r| r.cache_read), sum(&merged, |r| r.cache_read));
            prop_assert_eq!(sum(&rows, |r| r.cache_write), sum(&merged, |r| r.cache_write));
        }

        #[test]
        fn canon_model_is_idempotent(raw in "\\PC{0,30}") {
            let once = canon_model(&raw);
            prop_assert_eq!(canon_model(&once), once);
        }
    }
}
