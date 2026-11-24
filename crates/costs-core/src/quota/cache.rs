//! Cache em disco: leitura, escrita atômica, merge `has`-based e lock por diretório.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::entry::{Entry, Partial};
use super::env::Env;

const LOCK_FRESH_S: i64 = 60;
const SCHEMA_VERSION: u32 = 1;
const SOURCE_ACTIVE: &str = "active";

/// `$AI_USAGE_CACHE_DIR`, senão `$XDG_CACHE_HOME/ai-standards/usage`, senão `~/.cache`.
pub fn default_cache_dir(env: &dyn Env) -> PathBuf {
    if let Some(d) = env.var("AI_USAGE_CACHE_DIR") {
        return PathBuf::from(d);
    }
    let base = match env.var("XDG_CACHE_HOME") {
        Some(x) => PathBuf::from(x),
        None => os_cache_dir(env),
    };
    base.join("ai-standards").join("usage")
}

/// Convenção XDG (`~/.cache`), igual aos scripts bash do ai-standards: o cache é
/// compartilhado com o statusline e o `~/Library/Caches` do macOS dividia o diretório
/// em dois conforme o contexto (launchd/GUI sem `XDG_CACHE_HOME` vs shell com).
fn os_cache_dir(env: &dyn Env) -> PathBuf {
    env.home().unwrap_or_default().join(".cache")
}

fn file_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// Entrada em cache; ausente ou ilegível → `None`.
pub fn read(dir: &Path, id: &str) -> Option<Entry> {
    let raw = fs::read_to_string(file_path(dir, id)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Tag de conta válida: `[a-z0-9_-]+`, exceto `default`.
fn is_account_tag(tag: &str) -> bool {
    tag != "default"
        && !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Leitura tolerante a conta: `<id>.json` vence; senão `<id>@<tag>.json`. Havendo vários
/// sufixos, escolhe o de `fetched_at` mais recente (empate: nome menor, determinístico).
pub fn read_account(dir: &Path, id: &str) -> Option<Entry> {
    if let Some(e) = read(dir, id) {
        return Some(e);
    }
    let prefix = format!("{id}@");
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|f| {
            let name = f.file_name().into_string().ok()?;
            let tag = name.strip_prefix(&prefix)?.strip_suffix(".json")?;
            if !is_account_tag(tag) {
                return None;
            }
            let entry: Entry = serde_json::from_str(&fs::read_to_string(f.path()).ok()?).ok()?;
            Some((name, entry))
        })
        .max_by(|(an, a), (bn, b)| a.fetched_at.cmp(&b.fetched_at).then_with(|| bn.cmp(an)))
        .map(|(_, e)| e)
}

fn ensure_dir(dir: &Path) -> io::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
}

fn create_private(path: &Path) -> io::Result<fs::File> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

/// Escrita atômica: arquivo temporário no mesmo diretório e `rename`.
pub fn write(dir: &Path, entry: &Entry) -> io::Result<()> {
    ensure_dir(dir)?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let tmp = dir.join(format!(".{}.{}.{nanos}.tmp", entry.provider, std::process::id()));
    let res = create_private(&tmp).and_then(|mut f| {
        f.write_all(serde_json::to_string(entry)?.as_bytes())?;
        f.sync_all()
    });
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, file_path(dir, &entry.provider)).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Sucesso: chave presente no parcial substitui (mesmo null/[]); ausente mantém.
pub fn apply_partial(prev: Option<&Entry>, id: &str, p: Partial, now: i64) -> Entry {
    let has_data = p.windows.as_ref().is_some_and(|w| !w.is_empty()) || p.balance.as_ref().is_some_and(Option::is_some);
    let data_at = if let Some(at) = p.data_at.filter(|_| has_data) {
        at
    } else if has_data {
        now
    } else {
        prev.and_then(|e| e.data_at.or(Some(e.fetched_at))).unwrap_or(now)
    };
    Entry {
        v: SCHEMA_VERSION,
        provider: id.into(),
        source: SOURCE_ACTIVE.into(),
        fetched_at: now,
        ok: true,
        error: None,
        data_at: Some(data_at),
        plan: p.plan.unwrap_or_else(|| prev.and_then(|e| e.plan.clone())),
        windows: p
            .windows
            .unwrap_or_else(|| prev.map(|e| e.windows.clone()).unwrap_or_default()),
        balance: p.balance.unwrap_or_else(|| prev.and_then(|e| e.balance.clone())),
        reset_credits: p
            .reset_credits
            .unwrap_or_else(|| prev.and_then(|e| e.reset_credits.clone())),
        week_reset_credits: p
            .week_reset_credits
            .unwrap_or_else(|| prev.and_then(|e| e.week_reset_credits.clone())),
    }
}

/// Erro: `ok=false`, preserva plano/janelas/saldo e o `data_at` do último dado bom.
pub fn apply_error(prev: Option<&Entry>, id: &str, code: &str, now: i64) -> Entry {
    Entry {
        v: SCHEMA_VERSION,
        provider: id.into(),
        source: prev.map_or_else(|| SOURCE_ACTIVE.into(), |e| e.source.clone()),
        fetched_at: now,
        ok: false,
        error: Some(code.into()),
        data_at: prev.and_then(|e| e.data_at.or(e.ok.then_some(e.fetched_at))),
        plan: prev.and_then(|e| e.plan.clone()),
        windows: prev.map(|e| e.windows.clone()).unwrap_or_default(),
        balance: prev.and_then(|e| e.balance.clone()),
        reset_credits: prev.and_then(|e| e.reset_credits.clone()),
        week_reset_credits: prev.and_then(|e| e.week_reset_credits.clone()),
    }
}

/// Posse do lock `<id>.lock`; liberado no `Drop`.
#[derive(Debug)]
pub struct LockGuard {
    dir: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stamp(lock: &Path, now: i64) -> io::Result<()> {
    fs::write(lock.join("epoch"), now.to_string())
}

fn lock_fresh(lock: &Path, now: i64) -> bool {
    if !lock.is_dir() {
        return false;
    }
    if let Some(e) = fs::read_to_string(lock.join("epoch"))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
    {
        return now - e <= LOCK_FRESH_S;
    }
    // sem epoch ainda (dono gravando): vale o mtime
    fs::metadata(lock)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|d| d.as_secs() as i64 <= LOCK_FRESH_S)
}

fn try_mkdir(lock: &Path, now: i64) -> Option<LockGuard> {
    fs::create_dir(lock).ok()?;
    let guard = LockGuard {
        dir: lock.to_path_buf(),
    };
    stamp(lock, now).ok()?;
    Some(guard)
}

/// Adquire o lock; `None` se há um lock fresco. Lock velho é tomado sob mutex `.take`.
pub fn lock_acquire(dir: &Path, id: &str, now: i64) -> Option<LockGuard> {
    ensure_dir(dir).ok()?;
    let lock = dir.join(format!("{id}.lock"));
    if let Some(g) = try_mkdir(&lock, now) {
        return Some(g);
    }
    if lock_fresh(&lock, now) {
        return None;
    }
    let take = dir.join(format!("{id}.lock.take"));
    if fs::create_dir(&take).is_err() {
        if !lock_fresh(&take, now) {
            let _ = fs::remove_dir(&take);
        }
        return None;
    }
    let guard = if lock_fresh(&lock, now) {
        None
    } else {
        let _ = fs::remove_dir_all(&lock);
        try_mkdir(&lock, now)
    };
    let _ = fs::remove_dir(&take);
    guard
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::entry::{Balance, ResetCredits, Window};

    fn win() -> Window {
        Window {
            label: "5h".into(),
            remaining_pct: Some(80),
            resets_at: Some(9),
            window_s: Some(18000),
            window_kind: None,
        }
    }

    fn good(now: i64) -> Entry {
        apply_partial(
            None,
            "x",
            Partial {
                plan: Some(Some("pro".into())),
                windows: Some(vec![win()]),
                balance: Some(Some(Balance {
                    amount: 1.5,
                    currency: "USD".into(),
                })),
                reset_credits: Some(Some(ResetCredits {
                    available: 1,
                    expires: vec![500],
                    expiry_groups: None,
                })),
                week_reset_credits: Some(Some(ResetCredits {
                    available: 1,
                    expires: vec![900],
                    expiry_groups: None,
                })),
                data_at: None,
            },
            now,
        )
    }

    #[test]
    fn round_trip_and_key_order() {
        let d = tempfile::tempdir().unwrap();
        let e = good(100);
        write(d.path(), &e).unwrap();
        assert_eq!(read(d.path(), "x"), Some(e.clone()));
        let raw = fs::read_to_string(d.path().join("x.json")).unwrap();
        let keys = [
            "\"v\"",
            "\"provider\"",
            "\"source\"",
            "\"fetched_at\"",
            "\"ok\"",
            "\"error\"",
            "\"data_at\"",
            "\"plan\"",
            "\"windows\"",
            "\"balance\"",
        ];
        let pos: Vec<usize> = keys.iter().map(|k| raw.find(k).unwrap()).collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]), "{raw}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(d.path().join("x.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1, "sem temporários");
    }

    #[test]
    fn has_based_replacement() {
        let prev = good(100);
        let keep = apply_partial(Some(&prev), "x", Partial::default(), 200);
        assert_eq!(keep.plan, prev.plan);
        assert_eq!(keep.windows, prev.windows);
        assert_eq!(keep.balance, prev.balance);
        assert_eq!(keep.reset_credits, prev.reset_credits);
        assert_eq!(keep.week_reset_credits, prev.week_reset_credits);
        assert_eq!(keep.data_at, Some(100), "sem dados novos não avança");
        assert_eq!(keep.fetched_at, 200);

        let clear = apply_partial(
            Some(&prev),
            "x",
            Partial {
                plan: Some(None),
                windows: Some(vec![]),
                balance: Some(None),
                reset_credits: Some(None),
                week_reset_credits: Some(None),
                data_at: None,
            },
            300,
        );
        assert_eq!((clear.plan, clear.windows, clear.balance), (None, vec![], None));
        assert_eq!(clear.reset_credits, None);
        assert_eq!(clear.week_reset_credits, None);
        assert_eq!(clear.data_at, Some(100));

        let fresh = apply_partial(
            Some(&prev),
            "x",
            Partial {
                windows: Some(vec![win()]),
                ..Partial::default()
            },
            400,
        );
        assert_eq!(fresh.data_at, Some(400));
        assert_eq!(fresh.plan, prev.plan);
    }

    #[test]
    fn error_keeps_last_good_data() {
        let prev = good(100);
        let err = apply_error(Some(&prev), "x", "http", 500);
        assert_eq!(err.reset_credits, prev.reset_credits, "erro preserva os créditos");
        assert_eq!(err.week_reset_credits, prev.week_reset_credits);
        assert!(!err.ok);
        assert_eq!(err.error.as_deref(), Some("http"));
        assert_eq!(err.data_at, Some(100));
        assert_eq!(err.windows, prev.windows);
        assert_eq!(err.balance, prev.balance);
        assert_eq!(err.plan, prev.plan);
        let none = apply_error(None, "x", "nocred", 5);
        assert_eq!((none.data_at, none.windows.len()), (None, 0));
    }

    #[test]
    fn lock_fresh_blocks_and_stale_is_taken_over() {
        let d = tempfile::tempdir().unwrap();
        let g = lock_acquire(d.path(), "x", 1000).expect("livre");
        assert!(lock_acquire(d.path(), "x", 1030).is_none(), "fresco bloqueia");
        drop(g);
        assert!(!d.path().join("x.lock").exists());

        let old = d.path().join("x.lock");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("epoch"), "1000").unwrap();
        let g = lock_acquire(d.path(), "x", 1100).expect("takeover do lock velho");
        assert_eq!(fs::read_to_string(old.join("epoch")).unwrap(), "1100");
        assert!(!d.path().join("x.lock.take").exists());
        drop(g);
    }

    #[test]
    fn v1_file_without_week_credits_deserializes() {
        let d = tempfile::tempdir().unwrap();
        let e = good(100);
        let mut v1 = serde_json::to_value(&e).unwrap();
        v1.as_object_mut().unwrap().remove("week_reset_credits");
        fs::write(d.path().join("x.json"), v1.to_string()).unwrap();
        let old: Entry = serde_json::from_str(&fs::read_to_string(d.path().join("x.json")).unwrap()).unwrap();
        assert_eq!(old.week_reset_credits, None);
        assert_eq!(old.reset_credits, e.reset_credits);
    }

    #[test]
    fn read_ignores_corrupt_file() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("x.json"), "{nao json").unwrap();
        assert_eq!(read(d.path(), "x"), None);
        assert_eq!(read(d.path(), "ausente"), None);
    }

    fn put(d: &Path, file: &str, fetched_at: i64) {
        let mut e = good(fetched_at);
        e.fetched_at = fetched_at;
        fs::write(d.join(file), serde_json::to_string(&e).unwrap()).unwrap();
    }

    #[test]
    fn account_suffix_only_is_read() {
        let d = tempfile::tempdir().unwrap();
        put(d.path(), "x@pi.json", 7);
        assert_eq!(read_account(d.path(), "x").unwrap().fetched_at, 7);
        assert!(read_account(d.path(), "y").is_none());
    }

    #[test]
    fn account_default_file_wins() {
        let d = tempfile::tempdir().unwrap();
        put(d.path(), "x@pi.json", 900);
        put(d.path(), "x.json", 1);
        assert_eq!(read_account(d.path(), "x").unwrap().fetched_at, 1);
    }

    #[test]
    fn account_ambiguous_picks_newest_and_skips_invalid_tags() {
        let d = tempfile::tempdir().unwrap();
        put(d.path(), "x@a.json", 10);
        put(d.path(), "x@b_2-c.json", 30);
        put(d.path(), "x@default.json", 99);
        put(d.path(), "x@Bad.json", 98);
        put(d.path(), "x@.json", 97);
        fs::write(d.path().join("x@corrupt.json"), "{").unwrap();
        assert_eq!(read_account(d.path(), "x").unwrap().fetched_at, 30);
    }
}
