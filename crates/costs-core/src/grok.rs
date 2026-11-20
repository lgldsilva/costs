//! Coletor do Grok Build.
//!
//! Soma os turnos devolvidos por `grok usage <sessao>` por (dia local, modelo) e
//! devolve o JSON no formato diário (compatível com ccusage).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, Local, NaiveDateTime, Offset, Utc};
use serde_json::{Value, json};

use crate::collect::{Runner, SystemRunner};
use crate::source::split_input;

/// `costUsdTicks` por dólar.
const TICKS_PER_USD: f64 = 1e10;
/// Workers paralelos de `grok usage`.
const WORKERS: usize = 4;
/// Limite por chamada de `grok usage`.
pub const USAGE_TIMEOUT: Duration = Duration::from_secs(120);
/// Modelo usado quando o turno não informa `primaryModelId`.
const UNKNOWN_MODEL: &str = "grok-unknown";

/// Falhas fatais do coletor (falhas por sessão são ignoradas com aviso).
#[derive(Debug, thiserror::Error)]
pub enum GrokError {
    #[error("falha ao ler sessoes em {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("falha ao serializar saida: {0}")]
    Json(#[from] serde_json::Error),
}

/// Falha de uma chamada `grok usage`.
#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    /// Saída não zero ou vazia: sessão sem uso registrado (silencioso).
    #[error("sessao sem uso registrado")]
    NoUsage,
    #[error("timeout")]
    Timeout,
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Lista as sessões (pastas que contêm `summary.json`).
pub trait SessionSource {
    /// Nomes das pastas-sessão, sem decodificar o caminho dos pais.
    fn session_ids(&self) -> Result<Vec<String>, GrokError>;
}

/// Executa `grok usage <sessao>` e devolve o stdout.
pub trait UsageRunner: Sync {
    fn usage(&self, session: &str) -> Result<String, UsageError>;
}

/// Sessões no disco: qualquer `summary.json` a profundidade >= 2 sob a raiz.
pub struct FsSessions {
    root: PathBuf,
}

impl FsSessions {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Raiz padrão: `~/.grok/sessions`.
    pub fn default_root() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|d| d.home_dir().join(".grok").join("sessions"))
    }
}

/// Profundidade mínima da pasta-sessão (raiz = 1): `<raiz>/<pai>/<sessao>`.
const MIN_SESSION_DEPTH: usize = 3;

fn walk(dir: &Path, depth: usize, out: &mut Vec<String>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            // Subpasta ilegível não derruba a coleta.
            let _ = walk(&path, depth + 1, out);
        } else if depth >= MIN_SESSION_DEPTH
            && entry.file_name() == "summary.json"
            && let Some(name) = dir.file_name().and_then(|n| n.to_str())
        {
            out.push(name.to_owned());
        }
    }
    Ok(())
}

impl SessionSource for FsSessions {
    fn session_ids(&self) -> Result<Vec<String>, GrokError> {
        let mut ids = Vec::new();
        match walk(&self.root, 1, &mut ids) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(GrokError::Io {
                    path: self.root.clone(),
                    source,
                });
            }
        }
        ids.sort();
        ids.dedup();
        Ok(ids)
    }
}

/// Escolhe o binário: `GROK_BIN`, `grok` no PATH, fallback existente, ou `grok`.
pub fn resolve_binary(env_bin: Option<OsString>, on_path: bool, fallback: Option<PathBuf>) -> PathBuf {
    if let Some(bin) = env_bin.filter(|b| !b.is_empty()) {
        return PathBuf::from(bin);
    }
    if on_path {
        return PathBuf::from("grok");
    }
    fallback.filter(|p| p.exists()).unwrap_or_else(|| PathBuf::from("grok"))
}

/// Binário real do ambiente (`which` respeita PATHEXT no Windows).
pub fn find_binary() -> PathBuf {
    let name = if cfg!(windows) { "grok.exe" } else { "grok" };
    let fallback = directories::BaseDirs::new().map(|d| d.home_dir().join(".grok").join("bin").join(name));
    resolve_binary(std::env::var_os("GROK_BIN"), which::which("grok").is_ok(), fallback)
}

/// Executa o binário real com timeout.
pub struct ProcessRunner {
    binary: PathBuf,
    timeout: Duration,
}

impl ProcessRunner {
    pub fn new(binary: PathBuf, timeout: Duration) -> Self {
        Self { binary, timeout }
    }
}

impl UsageRunner for ProcessRunner {
    /// Mesmo runner do `collect` (grupo de processos morto no timeout, pipes abandonáveis).
    fn usage(&self, session: &str) -> Result<String, UsageError> {
        let args = ["usage".to_string(), session.to_string()];
        let out = SystemRunner
            .run(&self.binary.to_string_lossy(), &args, None, self.timeout)
            .map_err(|e| match e.kind() {
                io::ErrorKind::TimedOut => UsageError::Timeout,
                _ => UsageError::Io(e),
            })?;
        if out.code != Some(0) || out.stdout.trim().is_empty() {
            return Err(UsageError::NoUsage);
        }
        Ok(out.stdout)
    }
}

/// Valida `8-4-4-4-12` hexadecimal (formato UUID) antes de usar como argumento.
pub fn is_uuid(s: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == GROUPS.len()
        && parts
            .iter()
            .zip(GROUPS)
            .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Turnos de uma sessão: uma falha isolada vira aviso e lista vazia.
fn fetch_turns(runner: &dyn UsageRunner, session: &str) -> Vec<Value> {
    let parsed = match runner.usage(session) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .map_err(|e| format!("JSONDecodeError: {e}"))
            .and_then(|v| match v {
                Value::Object(mut m) => Ok(match m.remove("turns") {
                    Some(Value::Array(a)) => a,
                    _ => Vec::new(),
                }),
                _ => Err("saida nao e objeto".to_owned()),
            }),
        Err(UsageError::NoUsage) => return Vec::new(),
        Err(e) => Err(e.to_string()),
    };
    parsed.unwrap_or_else(|why| {
        eprintln!("aviso: sessao {session} ignorada ({why})");
        Vec::new()
    })
}

/// Busca os turnos de todas as sessões com pool limitado; a ordem do resultado
/// acompanha a ordem de `sessions`.
pub fn fetch_all(runner: &dyn UsageRunner, sessions: &[String], workers: usize) -> Vec<Vec<Value>> {
    let slots: Vec<Mutex<Vec<Value>>> = sessions.iter().map(|_| Mutex::new(Vec::new())).collect();
    let next = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..workers.max(1).min(sessions.len()) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(session) = sessions.get(i) else { break };
                    let turns = fetch_turns(runner, session);
                    if let Ok(mut slot) = slots[i].lock() {
                        *slot = turns;
                    }
                }
            });
        }
    });
    slots.into_iter().map(|m| m.into_inner().unwrap_or_default()).collect()
}

/// Dia (`YYYY-MM-DD`) de um instante UTC no deslocamento dado.
pub fn bucket_day(stamp: DateTime<Utc>, offset: FixedOffset) -> String {
    stamp.with_timezone(&offset).date_naive().format("%Y-%m-%d").to_string()
}

/// Deslocamento do fuso local da máquina no instante dado.
pub fn local_offset_at(stamp: DateTime<Utc>) -> FixedOffset {
    stamp.with_timezone(&Local).offset().fix()
}

/// Dia local de um `endedAt` ISO. Sem fuso, a data já é local (como `astimezone()`).
fn day_of(iso: &str, offset_at: &dyn Fn(DateTime<Utc>) -> FixedOffset) -> Option<String> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        let utc = dt.with_timezone(&Utc);
        return Some(bucket_day(utc, offset_at(utc)));
    }
    NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|n| n.date().format("%Y-%m-%d").to_string())
}

#[derive(Default)]
struct Cell {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
    cost: f64,
}

/// Equivalente a `int(x or 0)`.
fn int_field(turn: &Value, key: &str) -> i64 {
    match turn.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        _ => 0,
    }
}

/// Soma os turnos por (dia, modelo) e monta o JSON diário.
pub fn aggregate(turns: &[Value], offset_at: &dyn Fn(DateTime<Utc>) -> FixedOffset) -> Value {
    let mut acc: BTreeMap<(String, String), Cell> = BTreeMap::new();
    for t in turns {
        let Some(ended) = t.get("endedAt").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some(day) = day_of(ended, offset_at) else {
            eprintln!("aviso: turno com endedAt invalido ignorado ({ended})");
            continue;
        };
        let model = t
            .get("primaryModelId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(UNKNOWN_MODEL)
            .to_owned();
        // `inputTokens` do xAI inclui o cache: separa para não cobrar o cache duas vezes.
        let (input, cache_read, cache_creation) = split_input(
            int_field(t, "inputTokens").max(0),
            int_field(t, "cachedReadTokens").max(0),
            int_field(t, "cacheCreationTokens").max(0),
        );
        let cell = acc.entry((day, model)).or_default();
        cell.input += input;
        cell.output += int_field(t, "outputTokens");
        cell.cache_read += cache_read;
        cell.cache_creation += cache_creation;
        cell.cost += int_field(t, "costUsdTicks") as f64 / TICKS_PER_USD;
    }
    let mut by_day: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for ((day, model), c) in acc {
        by_day.entry(day).or_default().push(json!({
            "modelName": model,
            "inputTokens": c.input,
            "outputTokens": c.output,
            "cacheReadTokens": c.cache_read,
            "cacheCreationTokens": c.cache_creation,
            "cost": c.cost,
        }));
    }
    let daily: Vec<Value> = by_day
        .into_iter()
        .map(|(date, items)| json!({"date": date, "modelBreakdowns": items}))
        .collect();
    json!({ "daily": daily })
}

/// Fluxo completo: lista sessões, consulta o uso e devolve o JSON (texto).
pub fn run_collect(
    source: &dyn SessionSource,
    runner: &dyn UsageRunner,
    offset_at: &dyn Fn(DateTime<Utc>) -> FixedOffset,
) -> Result<String, GrokError> {
    let sessions: Vec<String> = source
        .session_ids()?
        .into_iter()
        .filter(|s| {
            let ok = is_uuid(s);
            if !ok {
                eprintln!("aviso: sessao {s} ignorada (id nao e UUID)");
            }
            ok
        })
        .collect();
    let turns: Vec<Value> = fetch_all(runner, &sessions, WORKERS).into_iter().flatten().collect();
    Ok(serde_json::to_string(&aggregate(&turns, offset_at))?)
}

/// Atalho para o ambiente real (usado por `native::grok_real`).
pub fn run_collect_real() -> Result<String, GrokError> {
    let root = FsSessions::default_root().unwrap_or_default();
    run_collect(
        &FsSessions::new(root),
        &ProcessRunner::new(find_binary(), USAGE_TIMEOUT),
        &local_offset_at,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const S1: &str = "11111111-1111-1111-1111-111111111111";
    const S2: &str = "22222222-2222-2222-2222-222222222222";
    const S3: &str = "33333333-3333-3333-3333-333333333333";

    type Canned = HashMap<String, Result<String, fn() -> UsageError>>;

    struct FakeSource(Vec<String>);
    impl SessionSource for FakeSource {
        fn session_ids(&self) -> Result<Vec<String>, GrokError> {
            Ok(self.0.clone())
        }
    }

    struct FakeRunner(Canned);
    impl UsageRunner for FakeRunner {
        fn usage(&self, session: &str) -> Result<String, UsageError> {
            match self.0.get(session) {
                Some(Ok(s)) => Ok(s.clone()),
                Some(Err(f)) => Err(f()),
                None => Err(UsageError::NoUsage),
            }
        }
    }

    fn utc_offset(_: DateTime<Utc>) -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    fn fake() -> FakeRunner {
        let mut m: Canned = HashMap::new();
        m.insert(
            S1.into(),
            Ok(r#"{"turns":[
              {"endedAt":"2026-10-01T10:00:00Z","inputTokens":100,"outputTokens":10,
               "cachedReadTokens":50,"cacheCreationTokens":5,"costUsdTicks":20000000000,
               "primaryModelId":"grok-4.7-build"},
              {"endedAt":"2026-10-01T11:00:00Z","inputTokens":200,"outputTokens":20,
               "costUsdTicks":10000000000,"primaryModelId":"grok-4.7-build"}]}"#
                .into()),
        );
        m.insert(
            S2.into(),
            Ok(r#"{"turns":[
              {"endedAt":"2026-10-02T09:00:00Z","inputTokens":7,"outputTokens":3,
               "costUsdTicks":5000000000,"primaryModelId":"grok-4.6-build"},
              {"endedAt":"","inputTokens":999,"costUsdTicks":999}]}"#
                .into()),
        );
        FakeRunner(m)
    }

    #[test]
    fn sums_turns_per_day_and_model_skipping_unended_and_no_usage() {
        let src = FakeSource(vec![S1.into(), S2.into(), S3.into()]);
        let out = run_collect(&src, &fake(), &utc_offset).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let daily = v["daily"].as_array().unwrap();
        assert_eq!(daily.len(), 2);
        let d1 = &daily[0]["modelBreakdowns"][0];
        assert_eq!(daily[0]["date"], "2026-10-01");
        assert_eq!(d1["modelName"], "grok-4.7-build");
        assert_eq!(d1["inputTokens"], 245); // (100 - 50 - 5) + 200: o cache sai da entrada
        assert_eq!(d1["outputTokens"], 30);
        assert_eq!(d1["cacheReadTokens"], 50);
        assert_eq!(d1["cacheCreationTokens"], 5);
        assert!((d1["cost"].as_f64().unwrap() - 3.0).abs() < 1e-9);
        assert_eq!(daily[1]["modelBreakdowns"][0]["inputTokens"], 7);
    }

    #[test]
    fn non_uuid_sessions_are_never_run() {
        let src = FakeSource(vec!["s1".into(), "--help".into()]);
        let out = run_collect(&src, &fake(), &utc_offset).unwrap();
        assert_eq!(out, r#"{"daily":[]}"#);
    }

    #[test]
    fn uuid_validation() {
        assert!(is_uuid(S1));
        assert!(is_uuid("ABCDEF01-2345-6789-abcd-ef0123456789"));
        assert!(!is_uuid("s1"));
        assert!(!is_uuid("11111111-1111-1111-1111-11111111111"));
        assert!(!is_uuid("1111111g-1111-1111-1111-111111111111"));
        assert!(!is_uuid("11111111111111111111111111111111"));
        assert!(!is_uuid(""));
    }

    #[test]
    fn bucketing_with_explicit_offsets() {
        let t = "2026-11-01T01:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let east = FixedOffset::east_opt(3 * 3600).unwrap();
        let west = FixedOffset::west_opt(3 * 3600).unwrap();
        assert_eq!(bucket_day(t, FixedOffset::east_opt(0).unwrap()), "2026-11-01");
        assert_eq!(bucket_day(t, east), "2026-11-01");
        assert_eq!(bucket_day(t, west), "2026-10-31");
        let late = "2026-10-31T22:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(bucket_day(late, east), "2026-11-01");
    }

    #[test]
    fn aggregate_uses_offset_and_default_model() {
        let west = |_: DateTime<Utc>| FixedOffset::west_opt(3 * 3600).unwrap();
        let turns = vec![json!({"endedAt":"2026-11-01T01:00:00Z","inputTokens":1.0})];
        let v = aggregate(&turns, &west);
        assert_eq!(v["daily"][0]["date"], "2026-10-31");
        assert_eq!(v["daily"][0]["modelBreakdowns"][0]["modelName"], "grok-unknown");
        assert_eq!(v["daily"][0]["modelBreakdowns"][0]["inputTokens"], 1);
    }

    #[test]
    fn naive_timestamp_keeps_its_date_and_garbage_is_skipped() {
        let turns = vec![json!({"endedAt":"2026-10-05T23:30:00"}), json!({"endedAt":"nonsense"})];
        let v = aggregate(&turns, &utc_offset);
        assert_eq!(v["daily"].as_array().unwrap().len(), 1);
        assert_eq!(v["daily"][0]["date"], "2026-10-05");
    }

    #[test]
    fn worker_pool_preserves_session_order() {
        struct Echo;
        impl UsageRunner for Echo {
            fn usage(&self, s: &str) -> Result<String, UsageError> {
                let n: u64 = s.parse().unwrap();
                // Sessões iniciais demoram mais: a ordem de término inverte.
                thread::sleep(Duration::from_millis(30 - n.min(29)));
                Ok(format!(r#"{{"turns":[{{"n":{n}}}]}}"#))
            }
        }
        let sessions: Vec<String> = (0..20).map(|n| n.to_string()).collect();
        let out = fetch_all(&Echo, &sessions, 4);
        let got: Vec<i64> = out.iter().map(|t| t[0]["n"].as_i64().unwrap()).collect();
        assert_eq!(got, (0..20).collect::<Vec<i64>>());
    }

    #[test]
    fn failures_are_skipped_per_session() {
        let mut m: Canned = HashMap::new();
        m.insert(S1.into(), Err(|| UsageError::Timeout));
        m.insert(S2.into(), Ok("not json".into()));
        m.insert(S3.into(), Ok("[1]".into()));
        let r = FakeRunner(m);
        let out = fetch_all(&r, &[S1.into(), S2.into(), S3.into()], 4);
        assert!(out.iter().all(Vec::is_empty));
        assert!(UsageError::Timeout.to_string().contains("timeout"));
    }

    #[test]
    fn missing_turns_key_is_empty() {
        let mut m: Canned = HashMap::new();
        m.insert(S1.into(), Ok("{}".into()));
        let out = fetch_all(&FakeRunner(m), &[S1.into()], 4);
        assert!(out[0].is_empty());
    }

    #[test]
    fn binary_resolution_order() {
        let tmp = tempfile::tempdir().unwrap();
        let fb = tmp.path().join("grok");
        std::fs::write(&fb, "").unwrap();
        assert_eq!(
            resolve_binary(Some("/x/grok".into()), true, Some(fb.clone())),
            PathBuf::from("/x/grok")
        );
        assert_eq!(resolve_binary(None, true, Some(fb.clone())), PathBuf::from("grok"));
        assert_eq!(resolve_binary(None, false, Some(fb.clone())), fb);
        assert_eq!(
            resolve_binary(Some("".into()), false, Some(tmp.path().join("nope"))),
            PathBuf::from("grok")
        );
        assert_eq!(resolve_binary(None, false, None), PathBuf::from("grok"));
    }

    #[test]
    fn fs_sessions_glob_depth_and_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sessions");
        for rel in ["%2Fproj/a", "x/y/b", "c"] {
            let d = root.join(rel);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("summary.json"), "{}").unwrap();
        }
        std::fs::create_dir_all(root.join("p/empty")).unwrap();
        let ids = FsSessions::new(root).session_ids().unwrap();
        assert_eq!(ids, vec!["a".to_owned(), "b".to_owned()]);
        let none = FsSessions::new(tmp.path().join("missing")).session_ids().unwrap();
        assert!(none.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn process_runner_maps_exit_output_and_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("fakegrok");
        std::fs::write(
            &script,
            "#!/bin/sh\ncase \"$2\" in ok) echo '{\"turns\":[]}';; slow) sleep 5;; empty) ;; *) exit 1;; esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let r = ProcessRunner::new(script.clone(), Duration::from_secs(30));
        assert!(r.usage("ok").unwrap().contains("turns"));
        assert!(matches!(r.usage("bad"), Err(UsageError::NoUsage)));
        assert!(matches!(r.usage("empty"), Err(UsageError::NoUsage)));
        let short = ProcessRunner::new(script, Duration::from_millis(400));
        assert!(matches!(short.usage("slow"), Err(UsageError::Timeout)));
        let missing = ProcessRunner::new(tmp.path().join("none"), Duration::from_secs(1));
        assert!(matches!(missing.usage("ok"), Err(UsageError::Io(_))));
    }

    #[test]
    fn local_offset_is_sane() {
        let off = local_offset_at(Utc::now());
        assert!(off.local_minus_utc().abs() <= 14 * 3600);
    }
}
