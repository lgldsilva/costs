//! Cota/saldo nativos de 7 providers (port de `ai-usage`): claude, codex, zai,
//! opencode-go, xai, deepseek, openrouter. Formato de cache v1 compatível.

mod cache;
mod claude;
mod codex;
mod creds;
mod deepseek;
mod entry;
mod env;
mod norm;
mod opencode_go;
mod openrouter;
#[cfg(test)]
mod testutil;
mod xai;
mod zai;
mod zai_resets;

use std::path::Path;

pub use cache::{LockGuard, default_cache_dir, lock_acquire};
pub use entry::{Balance, Entry, Partial, ResetCredits, Window};
pub use env::{Env, HttpClient, HttpError, RealEnv, UreqClient};

use creds::{Ctx, Failure};

/// Providers suportados.
pub const PROVIDERS: [&str; 7] = ["claude", "codex", "zai", "opencode-go", "xai", "deepseek", "openrouter"];

fn run_provider(provider: &str, ctx: &Ctx) -> Result<Partial, Failure> {
    match provider {
        "claude" => claude::fetch(ctx),
        "codex" => codex::fetch(ctx),
        "zai" => zai::fetch(ctx),
        "opencode-go" => opencode_go::fetch(ctx),
        "xai" => xai::fetch(ctx),
        "deepseek" => deepseek::fetch(ctx),
        "openrouter" => openrouter::fetch(ctx),
        _ => Err(Failure::NoCred),
    }
}

/// Cache sem rede (`get --json` sem refresh), aceitando `<id>@<tag>.json` do bash; ausente/ilegível/id desconhecido → `None`.
pub fn read_cached(provider: &str, cache_dir: &Path) -> Option<Entry> {
    PROVIDERS
        .contains(&provider)
        .then(|| cache::read_account(cache_dir, provider))?
}

/// Busca a cota: devolve o cache se fresco (e `!refresh`); senão pega o lock, consulta o
/// provider e grava. Falhas viram `ok=false` com o último dado bom preservado. Lock
/// ocupado devolve o cache atual. Provider desconhecido → `nocred`, sem gravar.
pub fn fetch(provider: &str, now: i64, http: &dyn HttpClient, env: &dyn Env, refresh: bool, cache_dir: &Path) -> Entry {
    if !PROVIDERS.contains(&provider) {
        return cache::apply_error(None, provider, Failure::NoCred.code(), now);
    }
    let prev = cache::read(cache_dir, provider);
    if !refresh && let Some(p) = prev.as_ref().filter(|p| !p.is_stale(now)) {
        return p.clone();
    }
    let Some(_lock) = cache::lock_acquire(cache_dir, provider, now) else {
        return prev.unwrap_or_else(|| cache::apply_error(None, provider, Failure::Http.code(), now));
    };
    let enrich_resets = provider == "zai" && zai_resets_due(prev.as_ref(), env, cache_dir, now);
    let ctx = Ctx {
        http,
        env,
        now,
        enrich_resets,
    };
    let entry = match run_provider(provider, &ctx) {
        Ok(p) => cache::apply_partial(prev.as_ref(), provider, p, now),
        Err(f) => cache::apply_error(prev.as_ref(), provider, f.code(), now),
    };
    // gravação é best-effort: a entrada em memória continua válida para o chamador
    let _ = cache::write(cache_dir, &entry);
    if enrich_resets && entry.ok {
        let _ = std::fs::write(cache_dir.join(ZAI_RESETS_STAMP), now.to_string());
    }
    entry
}

/// Marca do último enriquecimento de créditos do z.ai (epoch em segundos).
const ZAI_RESETS_STAMP: &str = "zai.resets-at";
/// Intervalo mínimo entre enriquecimentos: o fluxo manda a chave crua a um handshake
/// ofuscado, resolve proof-of-work e se identifica como o cliente zcode; repetido a cada
/// atualização da UI, centenas de vezes por dia, arrisca a chave que os agentes usam.
const ZAI_RESETS_EVERY_S: i64 = 6 * 3600;

/// Enriquecer agora? `COSTS_ZAI_RESETS=0` desliga; sem créditos no cache, sempre.
fn zai_resets_due(prev: Option<&Entry>, env: &dyn Env, cache_dir: &Path, now: i64) -> bool {
    if env.var("COSTS_ZAI_RESETS").as_deref() == Some("0") {
        return false;
    }
    if prev.is_none_or(|p| p.reset_credits.is_none()) {
        return true;
    }
    let last = std::fs::read_to_string(cache_dir.join(ZAI_RESETS_STAMP))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok());
    last.is_none_or(|t| now - t >= ZAI_RESETS_EVERY_S)
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn fresh_cache_skips_network_and_refresh_forces_it() {
        let t = Fixture::new();
        t.with_env("DEEPSEEK_API_KEY", "k");
        let ok = FakeHttp::new().route(
            "https://api.deepseek.com/",
            200,
            r#"{"balance_infos":[{"currency":"USD","total_balance":"3"}]}"#,
        );
        let first = t.run("deepseek", &ok);
        let cached = fetch("deepseek", t.now + 10, &FakeHttp::new(), &t, false, &t.cache_dir());
        assert_eq!(cached, first);
        let stale = fetch("deepseek", t.now + 400, &FakeHttp::new(), &t, false, &t.cache_dir());
        assert_eq!(stale.error.as_deref(), Some("http"));
        assert_eq!(stale.balance, first.balance, "último dado bom preservado");
        assert_eq!(stale.data_at, Some(t.now));
        assert_eq!(read_cached("deepseek", &t.cache_dir()), Some(stale));
    }

    #[test]
    fn unknown_provider_and_busy_lock() {
        let t = Fixture::new();
        assert_eq!(t.run("copilot", &FakeHttp::new()).error.as_deref(), Some("nocred"));
        assert!(read_cached("../x", &t.cache_dir()).is_none());
        let _g = lock_acquire(&t.cache_dir(), "zai", t.now).unwrap();
        assert_eq!(t.run("zai", &FakeHttp::new()).error.as_deref(), Some("http"));
    }

    #[test]
    fn cache_dir_resolution() {
        let t = Fixture::new();
        t.with_env("AI_USAGE_CACHE_DIR", "/a/b");
        assert_eq!(default_cache_dir(&t), Path::new("/a/b"));
        let t2 = Fixture::new();
        t2.with_env("XDG_CACHE_HOME", "/x");
        assert_eq!(default_cache_dir(&t2), Path::new("/x/ai-standards/usage"));
    }
}
