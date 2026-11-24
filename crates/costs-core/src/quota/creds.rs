//! Contexto de fetch e leitura de credenciais (somente leitura, nunca logadas).

use std::path::PathBuf;

use serde_json::Value;

use std::path::Path;

use super::env::{Env, HttpClient};

const DEFAULT_TIMEOUT_S: u64 = 5;
const EXPIRY_MARGIN_S: i64 = 60;
const MS_THRESHOLD: i64 = 1_000_000_000_000;

/// Falha de um provider; o código vai para o campo `error` do cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Nenhum token/credencial encontrado.
    NoCred,
    /// Token encontrado, mas vencido (o harness não renovou).
    Expired,
    /// Provider de API key sem chave configurada.
    NoKey,
    /// A API recusou a credencial (401/403): login revogado ou chave inválida.
    Auth,
    Http,
    Parse,
}

impl Failure {
    pub fn code(self) -> &'static str {
        match self {
            Self::NoCred => "nocred",
            Self::Expired => "expired",
            Self::NoKey => "nokey",
            Self::Auth => "auth",
            Self::Http => "http",
            Self::Parse => "parse",
        }
    }

    /// Entre fontes de token sem sucesso, "vencido" diz mais que "não encontrado".
    pub fn most_specific(self, other: Self) -> Self {
        if self == Self::Expired || other == Self::Expired {
            Self::Expired
        } else {
            self
        }
    }
}

/// Dependências de um fetch.
pub struct Ctx<'a> {
    pub http: &'a dyn HttpClient,
    pub env: &'a dyn Env,
    pub now: i64,
    /// Roda o enriquecimento de créditos de reset do z.ai (limitado por `quota::fetch`).
    pub enrich_resets: bool,
    /// Cache de quota (`default_cache_dir` do chamador), para checkers locais.
    pub cache_dir: &'a Path,
}

impl Ctx<'_> {
    /// GET com sucesso só em 2xx e corpo não vazio (como `curl -f`).
    pub fn get(&self, url: &str, headers: &[(String, String)]) -> Result<String, Failure> {
        let timeout = self
            .env
            .var("AI_USAGE_HTTP_TIMEOUT")
            .and_then(|t| t.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_S);
        match self.http.get(url, headers, timeout) {
            Ok((s, body)) if (200..300).contains(&s) && !body.is_empty() => Ok(body),
            // Cloudflare rejects the HTTP client before the API authenticates it.
            Ok((403, body)) if body.trim() == "error code: 1010" => Err(Failure::Http),
            Ok((401 | 403, _)) => Err(Failure::Auth),
            _ => Err(Failure::Http),
        }
    }

    /// POST com corpo JSON: mesmo contrato de status do `get`.
    pub fn post(&self, url: &str, headers: &[(String, String)], body: &str) -> Result<String, Failure> {
        let timeout = self
            .env
            .var("AI_USAGE_HTTP_TIMEOUT")
            .and_then(|t| t.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_S);
        match self.http.post(url, headers, body, timeout) {
            Ok((s, body)) if (200..300).contains(&s) && !body.is_empty() => Ok(body),
            Ok((401 | 403, _)) => Err(Failure::Auth),
            _ => Err(Failure::Http),
        }
    }

    /// Harness atual (`AI_USAGE_HARNESS`): `pi`, `opencode`, ou vazio.
    pub fn harness(&self) -> String {
        self.env.var("AI_USAGE_HARNESS").unwrap_or_default()
    }

    pub fn home_file(&self, rel: &str) -> Option<PathBuf> {
        self.env.home().map(|h| h.join(rel))
    }

    pub fn pi_auth(&self) -> Option<Value> {
        read_json(self.home_file(".pi/agent/auth.json")?)
    }

    pub fn opencode_auth(&self) -> Option<Value> {
        read_json(self.home_file(".local/share/opencode/auth.json")?)
    }

    /// Expiração (s ou ms) válida: ausente/ilegível deixa o servidor decidir;
    /// com menos de 60 s restantes conta como expirada.
    pub fn exp_valid(&self, exp: Option<&Value>) -> bool {
        let e = match exp {
            Some(Value::Number(n)) => n.as_i64(),
            Some(Value::String(s)) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => s.parse().ok(),
            _ => None,
        };
        match e {
            None => true,
            Some(e) => {
                let e = if e > MS_THRESHOLD { e / 1000 } else { e };
                e > self.now + EXPIRY_MARGIN_S
            }
        }
    }

    /// Chave de API: arquivo do harness → env → arquivo pi → arquivo opencode.
    pub fn api_key(&self, json_name: &str, env_vars: &[&str]) -> Option<String> {
        self.api_keys(json_name, env_vars).into_iter().next()
    }

    /// Distinct credentials in the same priority order. Callers may retry another
    /// configured key after authentication rejection, never after a network error.
    pub fn api_keys(&self, json_name: &str, env_vars: &[&str]) -> Vec<String> {
        let pi = self.pi_auth().and_then(|v| str_at(&v, &[json_name, "key"]));
        let oc = self.opencode_auth().and_then(|v| str_at(&v, &[json_name, "key"]));
        let first = match self.harness().as_str() {
            "pi" => pi.clone(),
            "opencode" => oc.clone(),
            _ => None,
        };
        let candidates = first
            .into_iter()
            .chain(env_vars.iter().filter_map(|v| self.env.var(v)))
            .chain(pi)
            .chain(oc);
        let mut keys = Vec::new();
        for key in candidates {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    }
}

/// Cabeçalho `Authorization: Bearer` mais extras.
pub fn bearer(token: &str, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut h = vec![("Authorization".to_string(), format!("Bearer {token}"))];
    h.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    h
}

pub fn read_json(path: PathBuf) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// String não vazia no caminho dado.
pub fn str_at(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for k in path {
        cur = cur.get(k)?;
    }
    cur.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::quota::testutil::{FakeHttp, Fixture};

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        #[test]
        fn candidates_preserve_priority_without_duplicates(
            env_key in "[a-z]{1,6}", pi_key in "[a-z]{1,6}", oc_key in "[a-z]{1,6}", harness in 0usize..3,
        ) {
            let t = Fixture::new();
            t.with_env("AI_USAGE_HARNESS", ["", "pi", "opencode"][harness]);
            t.with_env("TEST_KEY", &env_key);
            t.write_home(".pi/agent/auth.json", &serde_json::json!({"test": {"key": pi_key}}).to_string());
            t.write_home(".local/share/opencode/auth.json", &serde_json::json!({"test": {"key": oc_key}}).to_string());
            let http = FakeHttp::new();
            let cache = t.cache_dir();
            let ctx = Ctx {
                http: &http,
                env: &t,
                now: t.now,
                enrich_resets: false,
                cache_dir: cache.as_path(),
            };
            let mut expected = vec![env_key.clone(), pi_key.clone(), oc_key.clone()];
            if harness != 0 {
                expected.insert(0, [env_key, pi_key, oc_key][harness].clone());
            }
            let mut unique = Vec::new();
            for key in expected {
                if !unique.contains(&key) { unique.push(key); }
            }
            prop_assert_eq!(ctx.api_key("test", &["TEST_KEY"]), unique.first().cloned());
            prop_assert_eq!(ctx.api_keys("test", &["TEST_KEY"]), unique);
        }
    }
}
