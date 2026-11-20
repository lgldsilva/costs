//! Preços por componente.
//!
//! Fonte principal: tabela do LiteLLM, só com as entradas do fornecedor oficial de cada
//! modelo (Anthropic, OpenAI, Z.AI, xAI, ...), que não oscilam com o roteamento.
//! Fallback: catálogo do OpenRouter, para modelos sem preço oficial na tabela.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::model::canon_model;
use crate::pricing::{CatalogEntry, How, Resolution, Resolver, normalize};
use crate::store::format_ts;

/// URL do catálogo público de modelos.
pub const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

/// Tabela de preços do LiteLLM (um arquivo, atualizado várias vezes por dia).
pub const LITELLM_PRICES_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// `litellm_provider` do fornecedor oficial -> vendor no formato de id do OpenRouter
/// (o mesmo que o `Resolver` usa para não casar modelo de um vendor com id de outro).
const FIRST_PARTY: [(&str, &str); 10] = [
    ("anthropic", "anthropic"),
    ("openai", "openai"),
    ("zai", "z-ai"),
    ("xai", "x-ai"),
    ("moonshot", "moonshotai"),
    ("deepseek", "deepseek"),
    ("gemini", "google"),
    ("meta", "meta"),
    ("dashscope", "qwen"),
    ("minimax", "minimax"),
];

/// Origem do preço gravado em `resolved`.
pub const SOURCE_LITELLM: &str = "litellm";
pub const SOURCE_OPENROUTER: &str = "openrouter";

/// Campos de preço mantidos (iguais aos do Python).
const PRICE_FIELDS: [&str; 4] = ["prompt", "completion", "input_cache_read", "input_cache_write"];

/// Timeout global da requisição (igual a `curl --max-time 30`).
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Mensagem quando o arquivo de preços não existe.
pub const MISSING_PRICES_MSG: &str = "prices.json ausente: rode `costs prices` antes";

#[derive(Debug, thiserror::Error)]
pub enum PricesError {
    #[error("{MISSING_PRICES_MSG}")]
    Missing,
    #[error("falha HTTP: {0}")]
    Http(String),
    #[error("resposta do OpenRouter inválida: {0}")]
    Parse(String),
    #[error("models.toml inválido: {0}")]
    Toml(String),
    #[error("erro de E/S em {path}: {source}")]
    Io { path: String, source: std::io::Error },
}

/// Cliente HTTP mínimo, substituível em testes.
pub trait HttpClient {
    fn get(&self, url: &str) -> Result<String, String>;
}

/// Implementação real baseada em `ureq`.
pub struct UreqClient;

impl HttpClient for UreqClient {
    fn get(&self, url: &str) -> Result<String, String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .build()
            .into();
        agent
            .get(url)
            .call()
            .map_err(|e| e.to_string())?
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())
    }
}

/// Preço por token de um modelo; campo ausente vale 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    #[serde(default)]
    pub prompt: f64,
    #[serde(default)]
    pub completion: f64,
    #[serde(default)]
    pub input_cache_read: f64,
    #[serde(default)]
    pub input_cache_write: f64,
}

/// Conteúdo de `prices.json` (também vai no contexto de `costs collect`).
///
/// `models` é indexado pelo **nome canônico** do modelo (o mesmo que vai para o
/// banco). `by_id` mantém a referência pelo id do OpenRouter e `resolved` diz
/// como cada nome foi resolvido (override, exato, normalizado ou irmão).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricesFile {
    #[serde(default)]
    pub fetched_at: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub models: HashMap<String, ModelPrice>,
    #[serde(default)]
    pub by_id: HashMap<String, ModelPrice>,
    #[serde(default)]
    pub resolved: HashMap<String, ResolvedPrice>,
}

/// Como um nome canônico foi ligado a um id de catálogo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPrice {
    pub id: String,
    pub how: String,
    /// Catálogo de onde veio o preço (`litellm` ou `openrouter`); vazio em arquivos antigos.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
}

/// Preço de um modelo com a procedência da resolução.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceEntry {
    pub price: ModelPrice,
    pub id: String,
    /// Preço aproximado (irmão mais próximo): custo sai como `fallback`.
    pub fallback: bool,
}

/// Tabela consultada na coleta: nome canônico -> preço.
#[derive(Debug, Clone, Default)]
pub struct PriceTable {
    entries: HashMap<String, PriceEntry>,
}

impl PriceTable {
    /// Monta a tabela a partir do `prices.json` (v2).
    pub fn new(file: &PricesFile) -> Self {
        let entries = file
            .models
            .iter()
            .filter(|(_, price)| price.prompt >= 0.0 && price.completion >= 0.0)
            .map(|(model, price)| {
                let resolved = file.resolved.get(model);
                let entry = PriceEntry {
                    price: *price,
                    id: resolved.map(|r| r.id.clone()).unwrap_or_default(),
                    fallback: resolved.is_some_and(|r| r.how == How::Sibling.as_str()),
                };
                (model.clone(), entry)
            })
            .collect();
        Self { entries }
    }

    /// Preço do modelo (nome cru; é canonizado antes da busca).
    pub fn lookup(&self, raw_model: &str) -> Option<&PriceEntry> {
        self.entries.get(&canon_model(raw_model))
    }

    /// Quantos modelos têm preço.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Tokens por componente.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// Custo por componente (entrada, saída, cache de leitura e de escrita).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Components {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

/// Custo por componente = tokens x preço por token (preço base, sem faixas).
pub fn components(usage: Usage, price: &ModelPrice) -> Components {
    let input = usage.input * price.prompt;
    let output = usage.output * price.completion;
    let cache_read = usage.cache_read * price.input_cache_read;
    let cache_write = usage.cache_write * price.input_cache_write;
    Components {
        input,
        output,
        cache_read,
        cache_write,
        total: input + output + cache_read + cache_write,
    }
}

#[derive(Deserialize)]
struct PriceMapDoc {
    #[serde(default)]
    price_map: Vec<PriceMapEntry>,
}

#[derive(Deserialize)]
struct PriceMapEntry {
    model: String,
    openrouter: String,
}

/// Nome canônico do modelo -> id no OpenRouter, da seção `[[price_map]]`.
pub fn load_price_map(models_toml: &str) -> Result<HashMap<String, String>, PricesError> {
    let doc: PriceMapDoc = toml::from_str(models_toml).map_err(|e| PricesError::Toml(e.to_string()))?;
    Ok(doc.price_map.into_iter().map(|e| (e.model, e.openrouter)).collect())
}

/// Preço como número: aceita string ("0.000001") ou número, como `float()` do Python.
fn price_value(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
    // "NaN"/"inf" passariam pelo `< 0` e virariam `null` no JSON gravado, que não relê.
    .filter(|f: &f64| f.is_finite())
}

/// Entrada de catálogo com o preço já normalizado.
fn catalog_entry(m: &Value) -> Option<CatalogEntry> {
    let id = m.get("id")?.as_str()?.to_string();
    // `:batch`/`:free` são rotas, não modelos; `~vendor/...` é alias móvel.
    if id.is_empty() || id.contains(':') || id.starts_with('~') {
        return None;
    }
    let mut price = ModelPrice::default();
    if let Some(p) = m.get("pricing").and_then(Value::as_object) {
        let field = |name: &str| p.get(name).and_then(price_value).unwrap_or(0.0);
        price = ModelPrice {
            prompt: field("prompt"),
            completion: field("completion"),
            input_cache_read: field("input_cache_read"),
            input_cache_write: field("input_cache_write"),
        };
    }
    let created = m.get("created").and_then(Value::as_u64).unwrap_or(0);
    // `-1` é o preço "variável" (roteadores auto); não serve para estimar.
    if price.prompt < 0.0 || price.completion < 0.0 {
        return None;
    }
    Some(CatalogEntry { id, created, price })
}

/// Entrada do LiteLLM do fornecedor oficial, com id no formato `vendor/slug`.
fn litellm_entry(key: &str, v: &Value) -> Option<CatalogEntry> {
    let provider = v.get("litellm_provider")?.as_str()?;
    let vendor = FIRST_PARTY.iter().find(|(p, _)| *p == provider)?.1;
    let field = |name: &str| v.get(name).and_then(price_value);
    // Sem preço de entrada e saída não serve para estimar (ex.: entradas de assinatura).
    let (prompt, completion) = (field("input_cost_per_token")?, field("output_cost_per_token")?);
    if prompt < 0.0 || completion < 0.0 {
        return None;
    }
    let slug = key.rsplit('/').next().filter(|s| !s.is_empty())?;
    Some(CatalogEntry {
        id: format!("{vendor}/{slug}"),
        created: 0,
        price: ModelPrice {
            prompt,
            completion,
            input_cache_read: field("cache_read_input_token_cost").unwrap_or(0.0),
            input_cache_write: field("cache_creation_input_token_cost").unwrap_or(0.0),
        },
    })
}

fn litellm_catalog(body: &str) -> Result<Vec<CatalogEntry>, PricesError> {
    let root: Value = serde_json::from_str(body).map_err(|e| PricesError::Parse(e.to_string()))?;
    let map = root
        .as_object()
        .ok_or_else(|| PricesError::Parse("tabela do LiteLLM não é um objeto".into()))?;
    let mut entries: Vec<CatalogEntry> = map.iter().filter_map(|(k, v)| litellm_entry(k, v)).collect();
    // A mesma chave pode aparecer com e sem prefixo (`claude-x`, `anthropic/claude-x`).
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries.dedup_by(|a, b| a.id == b.id);
    Ok(entries)
}

fn openrouter_catalog(body: &str) -> Result<Vec<CatalogEntry>, PricesError> {
    let root: Value = serde_json::from_str(body).map_err(|e| PricesError::Parse(e.to_string()))?;
    let catalog = root
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| PricesError::Parse("campo `data` ausente".into()))?;
    Ok(catalog.iter().filter_map(catalog_entry).collect())
}

/// Resultado de `costs prices`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FetchReport {
    /// Nomes com preço gravado.
    pub resolved: usize,
    /// Nomes sem preço no catálogo (nem irmão).
    pub missing: Vec<String>,
    /// Quantos por forma de resolução: override, exato, normalizado, irmão.
    pub by_how: [usize; 4],
    /// Quantos por origem: LiteLLM (oficial), OpenRouter.
    pub by_source: [usize; 2],
    /// Catálogo que falhou ao baixar (o outro foi usado sozinho).
    pub warnings: Vec<String>,
    /// Nomes que mantiveram o preço do `prices.json` anterior porque o catálogo deles falhou.
    pub carried: usize,
}

fn how_index(how: How) -> usize {
    match how {
        How::Override => 0,
        How::Exact => 1,
        How::Normalized => 2,
        How::Sibling => 3,
    }
}

struct PriceResolvers<'a> {
    official: Vec<CatalogEntry>,
    routed: Vec<CatalogEntry>,
    rules: crate::model::Rules,
    explicit: HashMap<String, String>,
    official_by_id: HashMap<&'a str, &'a CatalogEntry>,
    official_by_norm: HashMap<String, &'a CatalogEntry>,
    routed_by_id: HashMap<&'a str, &'a CatalogEntry>,
}

impl<'a> PriceResolvers<'a> {
    fn new(official: &'a [CatalogEntry], routed: &'a [CatalogEntry], models_toml: &str) -> Result<Self, PricesError> {
        let explicit = load_price_map(models_toml)?;
        let rules = crate::model::load_rules(models_toml).map_err(|e| PricesError::Toml(e.to_string()))?;
        let official_by_id = official.iter().map(|e| (e.id.as_str(), e)).collect();
        let official_by_norm = official.iter().map(|e| (normalize(&e.id), e)).collect();
        let routed_by_id = routed.iter().map(|e| (e.id.as_str(), e)).collect();
        Ok(Self {
            official: official.to_vec(),
            routed: routed.to_vec(),
            rules,
            explicit,
            official_by_id,
            official_by_norm,
            routed_by_id,
        })
    }

    fn official_hit(&self, model: &str) -> Option<(&CatalogEntry, Resolution)> {
        match self.explicit.get(model) {
            Some(id) => self.official_by_norm.get(&normalize(id)).map(|entry| {
                (
                    *entry,
                    Resolution {
                        id: entry.id.clone(),
                        how: How::Override,
                    },
                )
            }),
            None => Resolver::new(&self.official, &HashMap::new(), &self.rules)
                .resolve(model)
                .filter(|resolution| !resolution.how.is_fallback())
                .and_then(|resolution| Some((*self.official_by_id.get(resolution.id.as_str())?, resolution))),
        }
    }

    fn route(&self, model: &str) -> Result<(&CatalogEntry, Resolution), Option<Resolution>> {
        let resolver = Resolver::new(&self.routed, &self.explicit, &self.rules);
        let Some(resolution) = resolver.resolve(model) else {
            return Err(None);
        };
        match self.routed_by_id.get(resolution.id.as_str()) {
            Some(entry) => Ok((*entry, resolution)),
            None => Err(Some(resolution)),
        }
    }
}

fn ordered_models<'a>(targets: &'a [String], explicit: &'a HashMap<String, String>) -> Vec<String> {
    let mut ordered = targets.to_vec();
    ordered.extend(explicit.keys().cloned());
    ordered.sort();
    ordered.dedup();
    ordered
}

fn record_missing(report: &mut FetchReport, model: &str, route: Option<&Resolution>) {
    match route {
        Some(resolution) => report
            .missing
            .push(format!("{model} -> {} (fora do catalogo)", resolution.id)),
        None => report.missing.push(model.to_owned()),
    }
}

fn encode_price(entry: &CatalogEntry) -> Map<String, Value> {
    let mut prices = Map::new();
    for field in PRICE_FIELDS {
        let value = match field {
            "prompt" => entry.price.prompt,
            "completion" => entry.price.completion,
            "input_cache_read" => entry.price.input_cache_read,
            _ => entry.price.input_cache_write,
        };
        if value != 0.0 {
            prices.insert(field.to_string(), json!(value));
        }
    }
    prices
}

#[derive(Default)]
struct PriceOutput {
    models: Map<String, Value>,
    by_id: Map<String, Value>,
    resolved: Map<String, Value>,
}

fn record_price(
    report: &mut FetchReport,
    output: &mut PriceOutput,
    model: &str,
    entry: &CatalogEntry,
    resolution: Resolution,
    source: &str,
) {
    let prices = encode_price(entry);
    let id_key = if source == SOURCE_LITELLM {
        format!("{SOURCE_LITELLM}:{}", resolution.id)
    } else {
        resolution.id.clone()
    };
    output.models.insert(model.to_owned(), Value::Object(prices.clone()));
    output.by_id.insert(id_key, Value::Object(prices));
    output.resolved.insert(
        model.to_owned(),
        json!({"id": resolution.id, "how": resolution.how.as_str(), "source": source}),
    );
    report.resolved += 1;
    report.by_how[how_index(resolution.how)] += 1;
    report.by_source[usize::from(source == SOURCE_OPENROUTER)] += 1;
}

/// Carried price lookup: `(model, only_from_failed)` -> previous `(price, resolution)`.
type CarryFn<'a> = dyn Fn(&str, bool) -> Option<(ModelPrice, ResolvedPrice)> + 'a;

fn resolve_and_record(
    model: &str,
    resolvers: &PriceResolvers<'_>,
    report: &mut FetchReport,
    output: &mut PriceOutput,
    carry: &CarryFn<'_>,
) {
    if let Some((price, res)) = carry(model, true) {
        record_carried(report, output, model, price, res);
        return;
    }
    if let Some((entry, resolution)) = resolvers.official_hit(model) {
        record_price(report, output, model, entry, resolution, SOURCE_LITELLM);
        return;
    }
    let (entry, resolution) = match resolvers.route(model) {
        Ok(hit) => hit,
        Err(route) => {
            match carry(model, false) {
                Some((price, res)) => record_carried(report, output, model, price, res),
                None => record_missing(report, model, route.as_ref()),
            }
            return;
        }
    };
    record_price(report, output, model, entry, resolution, SOURCE_OPENROUTER);
}

/// Preço vindo do arquivo anterior: registra igual, mas conta em `carried` — o preço
/// não vem de nenhum catálogo desta rodada.
fn record_carried(
    report: &mut FetchReport,
    output: &mut PriceOutput,
    model: &str,
    price: ModelPrice,
    res: ResolvedPrice,
) {
    let prices = price_object(&price);
    let id_key = if res.source == SOURCE_LITELLM {
        format!("{SOURCE_LITELLM}:{}", res.id)
    } else {
        res.id.clone()
    };
    output.models.insert(model.to_owned(), Value::Object(prices.clone()));
    output.by_id.insert(id_key, Value::Object(prices));
    output.resolved.insert(
        model.to_owned(),
        json!({"id": res.id, "how": res.how, "source": res.source}),
    );
    report.resolved += 1;
    report.carried += 1;
}

/// Baixa os catálogos e grava preço para cada nome canônico pedido.
///
/// `targets` são os nomes canônicos que aparecem no banco; as chaves de
/// `[[price_map]]` entram sempre (override contra o OpenRouter). Para os demais, o preço
/// oficial do LiteLLM (nome exato ou normalizado) vence; sem ele, vale o OpenRouter com
/// toda a resolução (inclusive irmão). Um catálogo fora do ar não impede o outro; os dois
/// fora devolvem o erro do OpenRouter. Nome sem correspondência vai para `missing`.
pub fn fetch_prices(
    http: &dyn HttpClient,
    models_toml: &str,
    targets: &[String],
    out: &Path,
    now: DateTime<Utc>,
) -> Result<FetchReport, PricesError> {
    let mut report = FetchReport::default();
    let official = http
        .get(LITELLM_PRICES_URL)
        .map_err(PricesError::Http)
        .and_then(|b| litellm_catalog(&b));
    let routed = http
        .get(OPENROUTER_MODELS_URL)
        .map_err(PricesError::Http)
        .and_then(|b| openrouter_catalog(&b));
    let (official, routed, failed) = match (official, routed) {
        (Err(_), Err(e)) => return Err(e),
        (Ok(o), Err(e)) => {
            report
                .warnings
                .push(format!("OpenRouter indisponível ({e}): só LiteLLM"));
            (o, Vec::new(), Some(SOURCE_OPENROUTER))
        }
        (Err(e), Ok(r)) => {
            report
                .warnings
                .push(format!("LiteLLM indisponível ({e}): só OpenRouter"));
            (Vec::new(), r, Some(SOURCE_LITELLM))
        }
        (Ok(o), Ok(r)) => (o, r, None),
    };
    // Com um catálogo fora, o arquivo anterior segura o preço do que dependia dele: sem
    // isso os modelos só daquele catálogo sumiriam (custo NULL) e os demais trocariam de
    // fonte por um dia, até a próxima atualização.
    let previous = failed.and_then(|_| read_prices(out).ok());
    let carry = |model: &str, only_from_failed: bool| -> Option<(ModelPrice, ResolvedPrice)> {
        let prev = previous.as_ref()?;
        let res = prev.resolved.get(model)?;
        if only_from_failed && Some(res.source.as_str()) != failed {
            return None;
        }
        Some((*prev.models.get(model)?, res.clone()))
    };

    let resolvers = PriceResolvers::new(&official, &routed, models_toml)?;
    let mut output = PriceOutput::default();
    for model in ordered_models(targets, &resolvers.explicit) {
        resolve_and_record(&model, &resolvers, &mut report, &mut output, &carry);
    }
    let payload = json!({
        "fetched_at": format_ts(now),
        "source": format!("{LITELLM_PRICES_URL} + {OPENROUTER_MODELS_URL}"),
        "models": output.models,
        "by_id": output.by_id,
        "resolved": output.resolved,
    });
    let text = serde_json::to_string_pretty(&payload).map_err(|e| PricesError::Parse(e.to_string()))?;
    write_atomic(out, &text)?;
    Ok(report)
}

/// Campos de preço não nulos, no formato do `prices.json`.
fn price_object(price: &ModelPrice) -> Map<String, Value> {
    let mut prices = Map::new();
    for f in PRICE_FIELDS {
        let value = match f {
            "prompt" => price.prompt,
            "completion" => price.completion,
            "input_cache_read" => price.input_cache_read,
            _ => price.input_cache_write,
        };
        if value != 0.0 {
            prices.insert(f.to_string(), json!(value));
        }
    }
    prices
}

fn write_atomic(out: &Path, text: &str) -> Result<(), PricesError> {
    let io_err = |source| PricesError::Io {
        path: out.display().to_string(),
        source,
    };
    let dir = out.parent().filter(|p| !p.as_os_str().is_empty());
    let mut tmp = match dir {
        Some(d) => tempfile_in(d),
        None => tempfile_in(Path::new(".")),
    }
    .map_err(io_err)?;
    tmp.0.write_all(text.as_bytes()).map_err(io_err)?;
    tmp.0.sync_all().map_err(io_err)?;
    fs::rename(&tmp.1, out).map_err(io_err)
}

/// Cria `<dir>/.prices.json.tmp.<pid>` exclusivo.
fn tempfile_in(dir: &Path) -> std::io::Result<(fs::File, std::path::PathBuf)> {
    let path = dir.join(format!(".prices.json.tmp.{}", std::process::id()));
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    Ok((file, path))
}

/// Lê `prices.json`; erro claro quando o arquivo não existe.
pub fn read_prices(path: &Path) -> Result<PricesFile, PricesError> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(PricesError::Missing),
        Err(source) => {
            return Err(PricesError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    serde_json::from_str(&text).map_err(|e| PricesError::Parse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Só o OpenRouter responde (LiteLLM fora do ar), como antes da tabela oficial.
    struct Fake(Result<String, String>);
    impl HttpClient for Fake {
        fn get(&self, url: &str) -> Result<String, String> {
            match url {
                OPENROUTER_MODELS_URL => self.0.clone(),
                LITELLM_PRICES_URL => Err("offline".into()),
                other => panic!("URL inesperada: {other}"),
            }
        }
    }

    /// Os dois catálogos respondem.
    struct Both {
        litellm: Result<String, String>,
        openrouter: Result<String, String>,
    }
    impl HttpClient for Both {
        fn get(&self, url: &str) -> Result<String, String> {
            match url {
                OPENROUTER_MODELS_URL => self.openrouter.clone(),
                LITELLM_PRICES_URL => self.litellm.clone(),
                other => panic!("URL inesperada: {other}"),
            }
        }
    }

    const RULES: &str = "[[rules]]\nmatch = \"^glm\"\nprovider = \"zhipu\"\n\n\
                         [[rules]]\nmatch = \"^grok\"\nprovider = \"xai\"\n";

    const LITELLM: &str = r#"{
        "sample_spec": {"input_cost_per_token": 0.1},
        "zai/glm-5.3": {"litellm_provider": "zai", "input_cost_per_token": 1.4e-6,
            "output_cost_per_token": 4.4e-6, "cache_read_input_token_cost": 2.6e-7},
        "sail/zai-org/glm-5.3": {"litellm_provider": "sail", "input_cost_per_token": 1e-9,
            "output_cost_per_token": 1e-9},
        "chatgpt/gpt-9": {"litellm_provider": "chatgpt"},
        "openai/gpt-9": {"litellm_provider": "openai", "input_cost_per_token": 2e-6}
    }"#;

    const ROUTED: &str = r#"{"data": [
        {"id": "z-ai/glm-5.3", "created": 1, "pricing": {"prompt": "0.00000008", "completion": "0.0000001"}},
        {"id": "x-ai/grok-4.7", "created": 1, "pricing": {"prompt": "0.000002", "completion": "0.000006"}}
    ]}"#;

    const CATALOG: &str = r#"{"data": [
        {"id": "a/model", "created": 1, "pricing": {"prompt": "0.000001", "completion": "0.000002",
         "input_cache_read": "0.0000001", "input_cache_write": null, "image": "9"}},
        {"id": "a/model-large", "created": 2, "pricing": {"prompt": "0.000009"}},
        {"id": "a/model:free", "created": 3, "pricing": {"prompt": "0"}},
        {"id": "~a/latest", "created": 4, "pricing": {"prompt": "0.5"}},
        {"id": "b/nopricing", "created": 5}
    ]}"#;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
    }

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    const PRICE: ModelPrice = ModelPrice {
        prompt: 4e-6,
        completion: 2e-5,
        input_cache_read: 2e-7,
        input_cache_write: 5e-6,
    };

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn components_multiplica_tokens_pelo_preco() {
        let c = components(
            Usage {
                input: 1_000_000.0,
                output: 500_000.0,
                cache_read: 2_000_000.0,
                cache_write: 100_000.0,
            },
            &PRICE,
        );
        assert!(close(c.input, 4.0));
        assert!(close(c.output, 10.0));
        assert!(close(c.cache_read, 0.4));
        assert!(close(c.cache_write, 0.5));
        assert!(close(c.total, 14.9));
    }

    #[test]
    fn preco_faltando_vira_zero() {
        let price = ModelPrice {
            prompt: 1.0,
            ..Default::default()
        };
        let c = components(
            Usage {
                input: 10.0,
                output: 10.0,
                ..Default::default()
            },
            &price,
        );
        assert_eq!(c.output, 0.0);
        assert_eq!(c.total, 10.0);
    }

    #[test]
    fn price_map_le_modelos_do_toml() {
        let got = load_price_map("[[price_map]]\nmodel = \"a\"\nopenrouter = \"x/a\"\n").unwrap();
        assert_eq!(got, map(&[("a", "x/a")]));
        assert!(load_price_map("").unwrap().is_empty());
        assert!(matches!(load_price_map("[[price_map]\n"), Err(PricesError::Toml(_))));
    }

    fn targets(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn fetch_resolve_por_nome_canonico_e_lista_faltantes() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("prices.json");
        let models = "[[price_map]]\nmodel = \"explicit\"\nopenrouter = \"a/model-large\"\n";
        let want = targets(&[
            "model",
            "model-large",
            "model-large-fast",
            "explicit",
            "nopricing",
            "sumiu",
        ]);
        let report = fetch_prices(&Fake(Ok(CATALOG.into())), models, &want, &out, now()).unwrap();
        assert_eq!(report.resolved, 5);
        assert_eq!(report.missing, vec!["sumiu"]);
        assert_eq!(report.by_how, [1, 3, 1, 0]);

        let raw: Value = serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(raw["fetched_at"], "2026-01-02T03:04:05+00:00");
        assert!(raw["source"].as_str().unwrap().contains(OPENROUTER_MODELS_URL));
        assert_eq!(raw["resolved"]["model"]["source"], "openrouter");
        assert_eq!(report.by_source, [0, 5]);
        assert_eq!(report.warnings.len(), 1, "LiteLLM fora do ar vira aviso");
        // chave = nome canônico; id do catálogo fica em `resolved`
        assert_eq!(raw["models"]["model"]["prompt"], 1e-6);
        assert_eq!(raw["by_id"]["a/model"]["prompt"], 1e-6);
        assert_eq!(raw["resolved"]["model"]["id"], "a/model");
        assert_eq!(raw["resolved"]["model"]["how"], "exact");
        assert_eq!(raw["resolved"]["model-large-fast"]["how"], "normalized");
        assert_eq!(raw["resolved"]["explicit"]["how"], "override");
        assert!(raw["models"]["model"].get("input_cache_write").is_none());
        assert!(raw["models"]["model"].get("image").is_none());
        assert_eq!(raw["models"]["nopricing"], json!({}));
        // rotas `:free` e aliases `~` não entram no catálogo
        assert!(raw["by_id"].get("a/model:free").is_none());
        assert!(raw["by_id"].get("~a/latest").is_none());
    }

    #[test]
    fn fetch_e_leitura_ida_e_volta() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("prices.json");
        fetch_prices(&Fake(Ok(CATALOG.into())), "", &targets(&["model"]), &out, now()).unwrap();
        let file = read_prices(&out).unwrap();
        assert_eq!(file.fetched_at, "2026-01-02T03:04:05+00:00");
        let p = file.models["model"];
        assert_eq!(p.prompt, 1e-6);
        assert_eq!(p.completion, 2e-6);
        assert_eq!(p.input_cache_read, 1e-7);
        assert_eq!(p.input_cache_write, 0.0);
        let leftovers = fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 1, "sem temporários");

        let table = PriceTable::new(&file);
        let entry = table.lookup("a/model").unwrap();
        assert_eq!(entry.id, "a/model");
        assert!(!entry.fallback);
        assert!(table.lookup("desconhecido").is_none());
    }

    #[test]
    fn irm_mais_proximo_vira_fallback_na_tabela() {
        let file: PricesFile = serde_json::from_str(
            r#"{"models":{"k3":{"prompt":0.5}},
                "resolved":{"k3":{"id":"moonshotai/kimi-k3","how":"sibling"}}}"#,
        )
        .unwrap();
        let table = PriceTable::new(&file);
        assert!(table.lookup("k3").unwrap().fallback);
    }

    #[test]
    fn preco_variavel_negativo_nao_entra_no_catalogo() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("p.json");
        let body = r#"{"data":[{"id":"openrouter/auto","pricing":{"prompt":"-1","completion":"-1"}}]}"#;
        let report = fetch_prices(&Fake(Ok(body.into())), "", &targets(&["auto"]), &out, now()).unwrap();
        assert_eq!(report.resolved, 0);
        assert_eq!(report.missing, vec!["auto"]);
    }

    #[test]
    fn fetch_aceita_precos_numericos() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("p.json");
        let body = r#"{"data":[{"id":"a/m","pricing":{"prompt":0.5,"completion":"x"}}]}"#;
        fetch_prices(&Fake(Ok(body.into())), "", &targets(&["m"]), &out, now()).unwrap();
        let p = read_prices(&out).unwrap().models["m"];
        assert_eq!(p.prompt, 0.5);
        assert_eq!(p.completion, 0.0);
    }

    #[test]
    fn preco_oficial_do_litellm_vence_o_openrouter() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("p.json");
        let http = Both {
            litellm: Ok(LITELLM.into()),
            openrouter: Ok(ROUTED.into()),
        };
        let want = targets(&["glm-5.3", "grok-4.7-build", "gpt-9"]);
        let report = fetch_prices(&http, RULES, &want, &out, now()).unwrap();
        assert_eq!((report.resolved, report.by_source), (2, [1, 1]));
        assert_eq!(report.missing, vec!["gpt-9"], "entrada sem preço de saída não serve");
        let file = read_prices(&out).unwrap();
        let glm = file.models["glm-5.3"];
        assert_eq!(
            (glm.prompt, glm.input_cache_read),
            (1.4e-6, 2.6e-7),
            "Z.AI, não revendedor"
        );
        assert_eq!(file.resolved["glm-5.3"].source, "litellm");
        assert_eq!(file.resolved["glm-5.3"].id, "z-ai/glm-5.3");
        // sem preço oficial: OpenRouter normalizado (`-build`)
        assert_eq!(file.resolved["grok-4.7-build"].source, "openrouter");
        assert_eq!(file.resolved["grok-4.7-build"].how, "normalized");
        let raw: Value = serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
        assert!(raw["by_id"].get("litellm:z-ai/glm-5.3").is_some());
    }

    #[test]
    fn override_do_price_map_tambem_prefere_o_preco_oficial() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("p.json");
        let http = Both {
            litellm: Ok(LITELLM.into()),
            openrouter: Ok(ROUTED.into()),
        };
        let models = format!(
            "{RULES}\n[[price_map]]\nmodel = \"glm-x\"\nopenrouter = \"z-ai/glm.5.3\"\n\
             [[price_map]]\nmodel = \"grok-y\"\nopenrouter = \"x-ai/grok-4.7\"\n"
        );
        fetch_prices(&http, &models, &targets(&[]), &out, now()).unwrap();
        let file = read_prices(&out).unwrap();
        assert_eq!(file.resolved["glm-x"].how, "override");
        assert_eq!(
            file.resolved["glm-x"].source, "litellm",
            "id normalizado acha o oficial"
        );
        assert_eq!(file.models["glm-x"].prompt, 1.4e-6);
        assert_eq!(
            file.resolved["grok-y"].source, "openrouter",
            "sem oficial, vale o OpenRouter"
        );
    }

    #[test]
    fn so_litellm_quando_openrouter_cai() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("p.json");
        let http = Both {
            litellm: Ok(LITELLM.into()),
            openrouter: Err("HTTP 503".into()),
        };
        let report = fetch_prices(&http, RULES, &targets(&["glm-5.3", "grok-4.7"]), &out, now()).unwrap();
        assert_eq!(
            (report.resolved, report.missing.clone()),
            (1, vec!["grok-4.7".to_string()])
        );
        assert!(report.warnings[0].contains("OpenRouter"));
    }

    #[test]
    fn catalogo_fora_mantem_o_preco_anterior_do_que_dependia_dele() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("p.json");
        let want = targets(&["glm-5.3", "grok-4.7"]);
        let both = Both {
            litellm: Ok(LITELLM.into()),
            openrouter: Ok(ROUTED.into()),
        };
        fetch_prices(&both, RULES, &want, &out, now()).unwrap();

        // OpenRouter cai: grok-4.7 (só nele) fica com o preço de ontem, glm segue oficial
        let or_down = Both {
            litellm: Ok(LITELLM.into()),
            openrouter: Err("HTTP 503".into()),
        };
        let r = fetch_prices(&or_down, RULES, &want, &out, now()).unwrap();
        assert_eq!((r.resolved, r.carried, r.missing.len()), (2, 1, 0));
        let file = read_prices(&out).unwrap();
        assert_eq!(file.models["grok-4.7"].prompt, 2e-6);
        assert_eq!(file.resolved["grok-4.7"].source, "openrouter");
        assert_eq!(PriceTable::new(&file).len(), 2);

        // LiteLLM cai: glm mantém o preço oficial em vez de trocar pelo do revendedor
        let ll_down = Both {
            litellm: Err("offline".into()),
            openrouter: Ok(ROUTED.into()),
        };
        let r = fetch_prices(&ll_down, RULES, &want, &out, now()).unwrap();
        assert_eq!(r.carried, 1);
        let file = read_prices(&out).unwrap();
        assert_eq!(
            (file.models["glm-5.3"].prompt, file.resolved["glm-5.3"].source.as_str()),
            (1.4e-6, "litellm")
        );
        let raw: Value = serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
        assert!(raw["by_id"].get("litellm:z-ai/glm-5.3").is_some());
    }

    #[test]
    fn arquivo_antigo_sem_fonte_tambem_segura_o_que_ficaria_sem_preco() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("p.json");
        // formato anterior ao campo `source`
        fs::write(
            &out,
            r#"{"fetched_at":"x","models":{"grok-4.7":{"prompt":0.000002,"completion":0.000006}},
               "resolved":{"grok-4.7":{"id":"x-ai/grok-4.7","how":"exact"}}}"#,
        )
        .unwrap();
        let or_down = Both {
            litellm: Ok(LITELLM.into()),
            openrouter: Err("HTTP 503".into()),
        };
        let r = fetch_prices(&or_down, RULES, &targets(&["grok-4.7"]), &out, now()).unwrap();
        assert_eq!((r.carried, r.missing.len()), (1, 0));
        assert_eq!(read_prices(&out).unwrap().models["grok-4.7"].completion, 6e-6);
    }

    #[test]
    fn preco_nao_finito_e_descartado_e_o_arquivo_continua_legivel() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("p.json");
        let body = r#"{"data": [
            {"id": "a/nan", "created": 1, "pricing": {"prompt": "NaN", "completion": "0.000002"}},
            {"id": "a/inf", "created": 1, "pricing": {"prompt": "0.000001", "completion": "inf"}}
        ]}"#;
        fetch_prices(&Fake(Ok(body.into())), "", &targets(&["nan", "inf"]), &out, now()).unwrap();
        let file = read_prices(&out).expect("prices.json relê");
        assert_eq!((file.models["nan"].prompt, file.models["nan"].completion), (0.0, 2e-6));
        assert_eq!((file.models["inf"].prompt, file.models["inf"].completion), (1e-6, 0.0));
    }

    #[test]
    fn fetch_propaga_falhas() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("p.json");
        let pm = targets(&["model"]);
        let http = fetch_prices(&Fake(Err("HTTP 404".into())), "", &pm, &out, now());
        assert!(matches!(http, Err(PricesError::Http(m)) if m == "HTTP 404"));
        let bad = fetch_prices(&Fake(Ok("nope".into())), "", &pm, &out, now());
        assert!(matches!(bad, Err(PricesError::Parse(_))));
        let nodata = fetch_prices(&Fake(Ok("{}".into())), "", &pm, &out, now());
        assert!(matches!(nodata, Err(PricesError::Parse(_))));
        let bad_models = fetch_prices(&Fake(Ok(CATALOG.into())), "[[price_map]\n", &pm, &out, now());
        assert!(matches!(bad_models, Err(PricesError::Toml(_))));
        assert!(!out.exists());
    }

    #[test]
    fn leitura_sem_arquivo_da_mensagem_do_python() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_prices(&dir.path().join("prices.json")).unwrap_err();
        assert_eq!(err.to_string(), "prices.json ausente: rode `costs prices` antes");
    }

    #[test]
    fn leitura_de_json_invalido_ou_diretorio_falha() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.json");
        fs::write(&bad, "{").unwrap();
        assert!(matches!(read_prices(&bad), Err(PricesError::Parse(_))));
        assert!(matches!(read_prices(dir.path()), Err(PricesError::Io { .. })));
    }

    #[test]
    fn escrita_em_diretorio_inexistente_falha() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("nada").join("p.json");
        let r = fetch_prices(&Fake(Ok(CATALOG.into())), "", &[], &out, now());
        assert!(matches!(r, Err(PricesError::Io { .. })));
    }
}
