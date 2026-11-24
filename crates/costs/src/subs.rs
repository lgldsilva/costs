//! Assinatura resolvida dinamicamente por provedor: o plano que a quota detectou
//! (rótulo e estado da credencial) decide se há plano contratado; o preço vem do
//! `[[subscriptions]]` com filtro de plano, da tabela de preços públicos quando o
//! rótulo é conhecido, do config default do provedor ou do preço do plano único.

use costs_core::config::Subscription;
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubscriptionState {
    /// Plano pago detectado (rótulo) ou credencial válida: há plano contratado.
    Active,
    /// Plano gratuito detectado: nada contratado.
    Free,
    /// Provedor só-assinatura com a credencial recusada: assinatura não renovada.
    Lapsed,
    /// Sem rótulo e com a credencial falhando de forma ambígua: mantém o valor
    /// configurado (login desatualizado não diz nada sobre a assinatura).
    Unverified,
}

impl SubscriptionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Free => "free",
            Self::Lapsed => "lapsed",
            Self::Unverified => "unverified",
        }
    }
}

/// Preços mensais públicos (USD) dos planos que os checkers detectam por rótulo.
/// O `[[subscriptions]]` com `plan` tem prioridade; sem filtro, a tabela ganha do
/// config default para não propagar o preço de um plano antigo.
const PLAN_PRICES: &[(&str, &[(&str, f64)])] = &[
    // OpenAI: free tier do Codex CLI, ChatGPT Plus e Pro.
    ("codex", &[("free", 0.0), ("plus", 20.0), ("pro", 200.0)]),
    // Anthropic: Claude Pro e Max (rate_limit_tier do /api/oauth/profile).
    ("claude", &[("pro", 20.0), ("max 5x", 100.0), ("max 20x", 200.0)]),
    // xAI: SuperGrok, a assinatura mensal do grok CLI.
    ("xai", &[("supergrok", 30.0)]),
];

/// Provedores cujo produto é uma única assinatura: credencial válida já é plano
/// ativo, e credencial recusada é assinatura não renovada.
const SINGLE_PLAN_PRICES: &[(&str, f64)] = &[("opencode-go", 10.0)];

/// Assinatura de um provedor após o cruzamento quota × config.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Resolved {
    pub state: SubscriptionState,
    /// Preço mensal resolvido; `None` com estado `active` significa plano ativo
    /// sem preço conhecido.
    pub usd_month: Option<f64>,
    /// O preço veio do `[[subscriptions]]` do config, não da detecção.
    pub from_config: bool,
    /// Preço bruto do config aplicável (mesmo refutado, para o display).
    pub configured: Option<f64>,
}

impl Resolved {
    /// Valor que entra no total fixo mensal.
    pub fn counted_monthly(&self) -> f64 {
        match self.state {
            SubscriptionState::Free | SubscriptionState::Lapsed => 0.0,
            _ => self.usd_month.unwrap_or(0.0),
        }
    }

    pub fn source(&self) -> Option<&'static str> {
        match self.state {
            SubscriptionState::Free | SubscriptionState::Lapsed => None,
            _ => Some(if self.from_config { "config" } else { "detected" }),
        }
    }
}

/// Rótulo de plano do entry de quota: trim, vazio conta como ausente.
pub fn entry_plan(entry: &Value) -> Option<&str> {
    entry
        .get("plan")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
}

/// Entry de quota do provedor, na ordem de `quota.providers`.
pub fn entry_for<'a>(provider: &str, quota_providers: &[String], entries: &'a [Value]) -> Option<&'a Value> {
    quota_providers
        .iter()
        .position(|p| p == provider)
        .and_then(|i| entries.get(i))
}

fn auth_ok(entry: &Value) -> bool {
    entry.get("ok").and_then(Value::as_bool) == Some(true)
}

/// Credencial recusada pelo provedor (401/403) — diferente de login expirado.
fn refused(entry: &Value) -> bool {
    entry.get("error").and_then(Value::as_str) == Some("auth")
}

fn table_price(provider: &str, plan: &str) -> Option<f64> {
    PLAN_PRICES.iter().find(|(p, _)| *p == provider).and_then(|(_, plans)| {
        plans
            .iter()
            .find(|(label, _)| label.eq_ignore_ascii_case(plan))
            .map(|(_, price)| *price)
    })
}

/// Provedor com tabela de preços por rótulo (codex, claude, xai): rótulo pago
/// fora da tabela não herda o default do config, que se referia a outro plano.
fn has_price_table(provider: &str) -> bool {
    PLAN_PRICES.iter().any(|(p, _)| *p == provider)
}

fn single_plan_price(provider: &str) -> Option<f64> {
    SINGLE_PLAN_PRICES
        .iter()
        .find(|(p, _)| *p == provider)
        .map(|(_, price)| *price)
}

/// Preço do config para um rótulo de plano exato (`plan = "max 5x"`).
fn configured_for_plan(subs: &[Subscription], provider: &str, plan: &str) -> Option<f64> {
    subs.iter()
        .filter(|s| s.provider.as_deref() == Some(provider))
        .find(|s| s.plan.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(plan)))
        .map(|s| s.monthly_usd)
}

/// Preço do config sem filtro de plano (default do provedor).
fn configured_default(subs: &[Subscription], provider: &str) -> Option<f64> {
    subs.iter()
        .filter(|s| s.provider.as_deref() == Some(provider) && s.plan.is_none())
        .map(|s| s.monthly_usd)
        .next()
}

/// Resolve a assinatura de um provedor cruzando o entry de quota com as
/// assinaturas do config. Devolve `None` quando não há nada a declarar.
pub fn resolve(provider: &str, entry: Option<&Value>, subs: &[Subscription]) -> Option<Resolved> {
    let plan = entry.and_then(entry_plan);
    let configured = configured_default(subs, provider);
    let single = single_plan_price(provider);
    let state = match plan {
        Some(p) if p.eq_ignore_ascii_case("free") => SubscriptionState::Free,
        Some(_) => SubscriptionState::Active,
        None => match entry {
            Some(e) if auth_ok(e) => SubscriptionState::Active,
            Some(e) if refused(e) && single.is_some() => SubscriptionState::Lapsed,
            _ => SubscriptionState::Unverified,
        },
    };
    let specific = plan.and_then(|p| configured_for_plan(subs, provider, p));
    // Rótulo pago desconhecido em provedor com tabela: preço n/d em vez de
    // arrastar o default do config (que era de outro plano).
    let unknown_tabled_plan = plan.is_some_and(|p| {
        state == SubscriptionState::Active && has_price_table(provider) && table_price(provider, p).is_none()
    });
    let usd_month = match state {
        SubscriptionState::Free | SubscriptionState::Lapsed => None,
        SubscriptionState::Active | SubscriptionState::Unverified if unknown_tabled_plan => specific,
        SubscriptionState::Active | SubscriptionState::Unverified => specific
            .or_else(|| plan.and_then(|p| table_price(provider, p)))
            .or(configured)
            .or(single),
    };
    let resolved = Resolved {
        state,
        usd_month,
        from_config: specific.is_some() || (configured.is_some() && usd_month == configured),
        configured,
    };
    // `Unverified` sem preço é ausência de sinal: não há assinatura a declarar.
    (resolved.usd_month.is_some() || resolved.state != SubscriptionState::Unverified).then_some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use costs_core::config::Subscription;
    use serde_json::json;

    fn sub(provider: &str, monthly: f64) -> Subscription {
        Subscription {
            name: provider.to_owned(),
            provider: Some(provider.to_owned()),
            plan: None,
            monthly_usd: monthly,
        }
    }

    fn sub_plan(provider: &str, plan: &str, monthly: f64) -> Subscription {
        Subscription {
            plan: Some(plan.to_owned()),
            ..sub(provider, monthly)
        }
    }

    fn r(provider: &str, entry: Option<Value>, subs: &[Subscription]) -> Option<Resolved> {
        resolve(provider, entry.as_ref(), subs)
    }

    #[test]
    fn plan_label_drives_state_and_table_price() {
        let subs = [sub("codex", 15.0)];
        // Rótulo conhecido: tabela ganha do config default.
        let x = r("codex", Some(json!({"plan": "plus"})), &subs).unwrap();
        assert_eq!(x.state, SubscriptionState::Active);
        assert_eq!(x.usd_month, Some(20.0));
        assert_eq!(x.source(), Some("detected"));

        // Sem rótulo: config default vale.
        let x = r("codex", Some(json!({"ok": true})), &subs).unwrap();
        assert_eq!(x.usd_month, Some(15.0));
        assert_eq!(x.source(), Some("config"));

        // Config com filtro de plano tem prioridade sobre a tabela.
        let subs = [sub_plan("codex", "plus", 18.0), sub("codex", 15.0)];
        let x = r("codex", Some(json!({"plan": "plus"})), &subs).unwrap();
        assert_eq!(x.usd_month, Some(18.0));
        assert_eq!(x.source(), Some("config"));

        // Free não contrata nada, mesmo com config (que fica para o display).
        let x = r("codex", Some(json!({"plan": "free"})), &subs).unwrap();
        assert_eq!(x.state, SubscriptionState::Free);
        assert_eq!(x.counted_monthly(), 0.0);
        assert_eq!(x.usd_month, None);
        assert_eq!(x.configured, Some(15.0));
    }

    #[test]
    fn unknown_tabled_plan_label_does_not_inherit_config_default() {
        // Codex reporta um rótulo pago fora da tabela (ex.: `go`): o default do
        // config (preço do Plus antigo) não se aplica; override específico sim.
        let subs = [sub("codex", 20.0)];
        let x = r("codex", Some(json!({"plan": "go"})), &subs).unwrap();
        assert_eq!(x.state, SubscriptionState::Active);
        assert_eq!(x.usd_month, None);
        assert_eq!(x.counted_monthly(), 0.0);
        assert_eq!(x.configured, Some(20.0));

        let subs = [sub_plan("codex", "go", 8.0)];
        let x = r("codex", Some(json!({"plan": "go"})), &subs).unwrap();
        assert_eq!(x.usd_month, Some(8.0));

        // Provedor sem tabela (z.ai): default do config segue valendo.
        let subs = [sub("zai", 42.0)];
        let x = r("zai", Some(json!({"plan": "pro"})), &subs).unwrap();
        assert_eq!(x.usd_month, Some(42.0));
    }

    #[test]
    fn claude_plan_from_profile_beats_stale_config_price() {
        // Troca de Pro (US$ 20 no config) para Max 5x: preço público detectado.
        let subs = [sub("claude", 20.0)];
        let x = r("claude", Some(json!({"plan": "Max 5x", "ok": true})), &subs).unwrap();
        assert_eq!(x.state, SubscriptionState::Active);
        assert_eq!(x.usd_month, Some(100.0));
        assert_eq!(x.source(), Some("detected"));
        assert_eq!(x.configured, Some(20.0));

        let x = r("claude", Some(json!({"plan": "Pro"})), &subs).unwrap();
        assert_eq!(x.usd_month, Some(20.0));
        assert_eq!(x.source(), Some("config"), "preço igual: config");
    }

    #[test]
    fn subscription_only_provider_lapses_on_refused_credentials() {
        let subs = [sub("opencode-go", 10.0)];
        // 401/403 recusada: assinatura não renovada, não soma.
        let x = r("opencode-go", Some(json!({"ok": false, "error": "auth"})), &subs).unwrap();
        assert_eq!(x.state, SubscriptionState::Lapsed);
        assert_eq!(x.counted_monthly(), 0.0);
        assert_eq!(x.configured, Some(10.0));
        assert_eq!(x.source(), None);

        // Login expirado é ambíguo: mantém contando.
        let x = r("opencode-go", Some(json!({"ok": false, "error": "expired"})), &subs).unwrap();
        assert_eq!(x.state, SubscriptionState::Unverified);
        assert_eq!(x.counted_monthly(), 10.0);

        // Credencial válida: ativa com preço público.
        let x = r("opencode-go", Some(json!({"ok": true})), &[]).unwrap();
        assert_eq!(x.state, SubscriptionState::Active);
        assert_eq!(x.usd_month, Some(10.0));
    }

    #[test]
    fn refusal_in_plan_provider_stays_unverified() {
        // Provider com rótulos de plano não é "só-assinatura": 401 é ambíguo
        // (pode ser chave rotacionada), não derruba a assinatura.
        let subs = [sub("xai", 30.0)];
        let x = r("xai", Some(json!({"ok": false, "error": "auth"})), &subs).unwrap();
        assert_eq!(x.state, SubscriptionState::Unverified);
        assert_eq!(x.counted_monthly(), 30.0);
    }

    #[test]
    fn stale_plan_label_survives_auth_failure() {
        let x = r("codex", Some(json!({"ok": false, "error": "http", "plan": "pro"})), &[]).unwrap();
        assert_eq!(x.state, SubscriptionState::Active);
        assert_eq!(x.usd_month, Some(200.0));
    }

    #[test]
    fn label_is_case_insensitive() {
        let x = r("xai", Some(json!({"plan": "SuperGrok"})), &[]).unwrap();
        assert_eq!(x.state, SubscriptionState::Active);
        assert_eq!(x.usd_month, Some(30.0));
    }

    #[test]
    fn nothing_to_say_without_config_and_quota() {
        assert_eq!(r("claude", None, &[]), None);
        assert_eq!(r("claude", Some(json!({})), &[]), None);
    }

    #[test]
    fn entry_lookup_follows_quota_provider_order() {
        let providers = vec!["codex".to_string(), "zai".to_string()];
        let entries = vec![json!({"plan": "free"}), json!({"plan": "pro"})];
        assert_eq!(entry_plan(entry_for("zai", &providers, &entries).unwrap()), Some("pro"));
        assert!(entry_for("xai", &providers, &entries).is_none());
    }

    #[test]
    fn blank_plan_is_absent() {
        assert_eq!(entry_plan(&json!({"plan": " "})), None);
        assert_eq!(entry_plan(&json!({"plan": 12})), None);
    }
}
