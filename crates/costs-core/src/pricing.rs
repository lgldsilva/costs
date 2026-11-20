//! Resolução de nome canônico de modelo para id do catálogo do OpenRouter.
//!
//! O nome canônico do banco (`claude-opus-5-5`) não é o id do OpenRouter
//! (`anthropic/claude-opus-5.5`). A resolução tenta, nesta ordem:
//! 1. `[[price_map]]` do `models.toml` (override explícito);
//! 2. slug exato do catálogo (`glm-5.3-flash` -> `z-ai/glm-5.3-flash`);
//! 3. variantes: prefixo de harness (`antigravity-`) e sufixos de rota
//!    (`-high`, `-fast`, `-build`, `-preview`, `-highspeed`, `-thinking`, `-256k`),
//!    com `.` e `-` equivalentes;
//! 4. irmão mais próximo do mesmo vendor (prefixo comum ou token em comum) —
//!    marcado como `sibling` para o custo sair como `fallback`.
//!
//! O vendor vem da regra de provedor do `models.toml` (`xai` -> `x-ai`), o que
//! evita casar um modelo Anthropic com um id de outro provedor.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::model::{Rules, canon_model, provider_of};
use crate::prices::ModelPrice;

/// Marcas textuais que não identificam família de modelo.
const GENERIC_TOKENS: [&str; 13] = [
    "pro", "flash", "preview", "free", "max", "mini", "code", "high", "fast", "build", "thinking", "latest", "turbo",
];

/// Prefixos de harness que não existem no catálogo.
const HARNESS_PREFIXES: [&str; 1] = ["antigravity-"];

/// Sufixos de variante que não existem no catálogo, do mais longo para o mais curto.
const VARIANT_SUFFIXES: [&str; 7] = [
    "-thinking",
    "-highspeed",
    "-high",
    "-build",
    "-fast",
    "-preview",
    "-256k",
];

/// Prefixo comum mínimo para aceitar um irmão.
const SIBLING_MIN_PREFIX: usize = 4;

/// Como o id foi encontrado.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum How {
    /// `[[price_map]]` explícito no `models.toml`.
    Override,
    /// Slug idêntico ao nome canônico.
    Exact,
    /// Slug obtido normalizando prefixo/sufixo/`.`.
    Normalized,
    /// Modelo mais parecido do mesmo vendor (preço aproximado).
    Sibling,
}

impl How {
    /// Nome gravado em `prices.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Exact => "exact",
            Self::Normalized => "normalized",
            Self::Sibling => "sibling",
        }
    }

    /// Preço aproximado (custo sai como `fallback`).
    pub fn is_fallback(self) -> bool {
        matches!(self, Self::Sibling)
    }
}

/// Id encontrado e como foi encontrado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub id: String,
    pub how: How,
}

/// Entrada do catálogo usada na resolução.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    pub id: String,
    /// Unix timestamp de criação no OpenRouter (desempate).
    pub created: u64,
    pub price: ModelPrice,
}

/// Minúsculas e `.`/`-` equivalentes: `claude-opus-5.5` == `claude-opus-5-5`.
pub fn normalize(s: &str) -> String {
    s.to_lowercase().replace('.', "-")
}

/// Parte final do id (`anthropic/claude-opus-5.5` -> `claude-opus-5.5`).
fn slug(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

fn vendor_of_id(id: &str) -> Option<&str> {
    id.split_once('/').map(|(v, _)| v)
}

/// Vendor do catálogo correspondente ao provedor de custo do `models.toml`.
pub fn vendor_of_provider(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "anthropic" => "anthropic",
        "openai" => "openai",
        "zhipu" => "z-ai",
        "moonshot" => "moonshotai",
        "deepseek" => "deepseek",
        "xai" => "x-ai",
        "google" => "google",
        "minimax" => "minimax",
        "alibaba" => "qwen",
        "meta" => "meta",
        "xiaomi" => "xiaomi",
        "nvidia" => "nvidia",
        "cohere" => "cohere",
        _ => return None,
    })
}

/// Candidatos do nome canônico, do mais fiel ao mais genérico.
fn candidates(canon: &str) -> Vec<String> {
    let mut out = vec![canon.to_string()];
    let mut cur = canon.to_string();
    for prefix in HARNESS_PREFIXES {
        if let Some(rest) = cur.strip_prefix(prefix).filter(|r| !r.is_empty()) {
            cur = rest.to_string();
            out.push(cur.clone());
        }
    }
    loop {
        let stripped = VARIANT_SUFFIXES
            .iter()
            .find_map(|suffix| cur.strip_suffix(suffix).filter(|r| !r.is_empty()));
        match stripped {
            Some(base) => {
                cur = base.to_string();
                out.push(cur.clone());
            }
            None => break,
        }
    }
    out
}

fn tokens(s: &str) -> HashSet<&str> {
    s.split('-')
        .filter(|t| !t.is_empty() && !GENERIC_TOKENS.contains(t))
        .collect()
}

fn common_prefix(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// Similaridade entre canônico e slug: (prefixo comum, tokens em comum).
fn score(canon: &str, slug: &str) -> (usize, usize) {
    let prefix = common_prefix(canon, slug);
    let overlap = tokens(canon).intersection(&tokens(slug)).count();
    (prefix, overlap)
}

/// Resolvedor: catálogo + overrides + regras de provedor.
pub struct Resolver<'a> {
    entries: Vec<&'a CatalogEntry>,
    by_slug: HashMap<String, Vec<&'a CatalogEntry>>,
    explicit: &'a HashMap<String, String>,
    rules: &'a Rules,
}

impl<'a> Resolver<'a> {
    /// Indexa o catálogo (que já deve vir sem rotas `:batch`/`:free`/`~`).
    pub fn new(catalog: &'a [CatalogEntry], explicit: &'a HashMap<String, String>, rules: &'a Rules) -> Self {
        let mut by_slug: HashMap<String, Vec<&CatalogEntry>> = HashMap::new();
        for e in catalog {
            by_slug.entry(normalize(slug(&e.id))).or_default().push(e);
        }
        Self {
            entries: catalog.iter().collect(),
            by_slug,
            explicit,
            rules,
        }
    }

    fn vendor(&self, canonical: &str) -> Option<&'static str> {
        vendor_of_provider(&provider_of(canonical, "pricing", self.rules))
    }

    /// Mais recente entre os candidatos (determinístico).
    fn newest<'e>(list: impl IntoIterator<Item = &'e CatalogEntry>) -> Option<&'e CatalogEntry> {
        list.into_iter()
            .max_by(|a, b| a.created.cmp(&b.created).then_with(|| b.id.cmp(&a.id)))
    }

    /// Slug exato dentro do vendor (ou único, quando o vendor é desconhecido).
    fn by_slug<'e>(&'e self, cand: &str, vendor: Option<&str>) -> Option<&'e CatalogEntry> {
        let hits = self.by_slug.get(&normalize(cand))?;
        let mut matched: Vec<&CatalogEntry> = match vendor {
            Some(v) => hits
                .iter()
                .copied()
                .filter(|e| vendor_of_id(&e.id) == Some(v))
                .collect(),
            None => hits.clone(),
        };
        if matched.is_empty() {
            return None;
        }
        if matched.len() > 1 && vendor.is_none() {
            // Sem vendor, só aceita quando todas as rotas são do mesmo id de preço.
            matched.sort_by(|a, b| a.id.cmp(&b.id));
        }
        Self::newest(matched)
    }

    /// Irmão mais próximo dentro do vendor.
    fn sibling(&self, canon: &str, vendor: Option<&str>) -> Option<&'a CatalogEntry> {
        let vendor = vendor?;
        let mut best: Option<(&CatalogEntry, (usize, usize))> = None;
        for e in &self.entries {
            if vendor_of_id(&e.id) != Some(vendor) {
                continue;
            }
            let sc = score(canon, &normalize(slug(&e.id)));
            if sc.0 < SIBLING_MIN_PREFIX && sc.1 == 0 {
                continue;
            }
            let better = match best {
                None => true,
                Some((cur, cur_sc)) => sc > cur_sc || (sc == cur_sc && (e.created, &cur.id) > (cur.created, &e.id)),
            };
            if better {
                best = Some((e, sc));
            }
        }
        best.map(|(e, _)| e)
    }

    /// Resolve o nome canônico; `None` quando o catálogo não tem nada parecido.
    pub fn resolve(&self, canonical: &str) -> Option<Resolution> {
        if let Some(id) = self.explicit.get(canonical) {
            return Some(Resolution {
                id: id.clone(),
                how: How::Override,
            });
        }
        let canon = normalize(canonical);
        let vendor = self.vendor(canonical);
        for (i, cand) in candidates(&canon).iter().enumerate() {
            if let Some(entry) = self.by_slug(cand, vendor) {
                let how = if i == 0 { How::Exact } else { How::Normalized };
                return Some(Resolution {
                    id: entry.id.clone(),
                    how,
                });
            }
        }
        self.sibling(&canon, vendor).map(|entry| Resolution {
            id: entry.id.clone(),
            how: How::Sibling,
        })
    }
}

/// O modelo bruto é de rota gratuita (`:free` ou sufixo `-free`)?
///
/// Custo desses modelos é zero por definição; não se aplica preço de catálogo.
pub fn is_free_model(raw: &str) -> bool {
    let m = raw.trim().to_lowercase();
    m.ends_with("-free") || m.contains(":free")
}

/// Nome canônico de um modelo (reexportado para quem só usa o resolver).
pub fn canonical(raw: &str) -> String {
    canon_model(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::load_rules;

    fn entry(id: &str, created: u64) -> CatalogEntry {
        CatalogEntry {
            id: id.into(),
            created,
            price: ModelPrice {
                prompt: 1e-6,
                ..Default::default()
            },
        }
    }

    fn catalog() -> Vec<CatalogEntry> {
        vec![
            entry("anthropic/claude-opus-5.5", 10),
            entry("anthropic/claude-opus-4.6", 9),
            entry("z-ai/glm-5.3-flash", 20),
            entry("z-ai/glm-5.3", 21),
            entry("openai/gpt-5.6-luna", 30),
            entry("openai/gpt-5.3-codex", 31),
            entry("openai/gpt-5.4-mini", 32),
            entry("google/gemini-3.8-flash", 40),
            entry("moonshotai/kimi-k3", 50),
            entry("moonshotai/kimi-k2.7-code", 51),
            entry("x-ai/grok-4.7", 60),
            entry("nvidia/nemotron-3-ultra-550b-a55b", 70),
            entry("qwen/qwen3.8-max-0902", 80),
            entry("qwen/qwen3.8-max-prime", 81),
        ]
    }

    fn rules() -> Rules {
        load_rules(
            "[[rules]]\nmatch = \"^(claude|anthropic)\"\nprovider = \"anthropic\"\n\
             [[rules]]\nmatch = \"^(gpt|o[0-9]|codex|chatgpt)\"\nprovider = \"openai\"\n\
             [[rules]]\nmatch = \"^(glm|z-ai)\"\nprovider = \"zhipu\"\n\
             [[rules]]\nmatch = \"^(kimi|k3|moonshot)\"\nprovider = \"moonshot\"\n\
             [[rules]]\nmatch = \"^grok\"\nprovider = \"xai\"\n\
             [[rules]]\nmatch = \"^(gemini|antigravity-gemini)\"\nprovider = \"google\"\n\
             [[rules]]\nmatch = \"^qwen\"\nprovider = \"alibaba\"\n\
             [[rules]]\nmatch = \"^nemotron\"\nprovider = \"nvidia\"\n",
        )
        .unwrap()
    }

    fn resolve(canon: &str) -> Option<Resolution> {
        let cat = catalog();
        let explicit = HashMap::new();
        let rules = rules();
        let r = Resolver::new(&cat, &explicit, &rules);
        r.resolve(canon)
    }

    #[test]
    fn exact_slug_with_dots_and_dashes() {
        assert_eq!(
            resolve("claude-opus-5-5"),
            Some(Resolution {
                id: "anthropic/claude-opus-5.5".into(),
                how: How::Exact
            })
        );
    }

    #[test]
    fn strips_harness_prefix_and_variant_suffix() {
        assert_eq!(
            resolve("antigravity-gemini-3.8-flash").unwrap().id,
            "google/gemini-3.8-flash"
        );
        assert_eq!(resolve("gpt-5.4-mini-fast").unwrap().id, "openai/gpt-5.4-mini");
        assert_eq!(resolve("grok-4.7-build-fast").unwrap().id, "x-ai/grok-4.7");
        let got = resolve("antigravity-gemini-3.8-flash-high").unwrap();
        assert_eq!(got.id, "google/gemini-3.8-flash");
        assert_eq!(got.how, How::Normalized);
    }

    #[test]
    fn explicit_override_wins() {
        let cat = catalog();
        let explicit = HashMap::from([("kimi-for-coding".to_string(), "moonshotai/kimi-k2.7-code".to_string())]);
        let rules = rules();
        let r = Resolver::new(&cat, &explicit, &rules);
        assert_eq!(
            r.resolve("kimi-for-coding"),
            Some(Resolution {
                id: "moonshotai/kimi-k2.7-code".into(),
                how: How::Override
            })
        );
    }

    #[test]
    fn sibling_prefers_longest_prefix_and_newest() {
        assert_eq!(resolve("qwen3.8-max-preview").unwrap().id, "qwen/qwen3.8-max-prime");
        assert_eq!(
            resolve("nemotron-3-ultra").unwrap().id,
            "nvidia/nemotron-3-ultra-550b-a55b"
        );
        assert_eq!(resolve("k3").unwrap().id, "moonshotai/kimi-k3");
    }

    #[test]
    fn vendor_restricts_siblings_and_unknown_provider_has_none() {
        // `space-bunny` não casa com provedor nenhum: sem vendor, sem irmão.
        assert_eq!(resolve("space-bunny"), None);
        // `claude-opus-9` só pode casar com um id anthropic, nunca com openai.
        assert_eq!(resolve("claude-opus-9").unwrap().id, "anthropic/claude-opus-5.5");
    }

    #[test]
    fn free_routes_are_zero_cost() {
        assert!(is_free_model("minimax/minimax-m3.1-flash:free"));
        assert!(is_free_model("[pi] something-free"));
        assert!(!is_free_model("minimax-m3"));
        assert!(!is_free_model("grok-4.7"));
    }

    #[test]
    fn unknown_model_without_similar_sibling_is_none() {
        assert_eq!(resolve("llama3.1"), None);
        assert_eq!(resolve("apodex-1.1-mini"), None);
    }

    #[test]
    fn normalize_keeps_versions_equivalent() {
        assert_eq!(normalize("Claude-Opus-5.5"), "claude-opus-5-5");
    }
}
