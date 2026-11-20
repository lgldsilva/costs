//! Comando doctor: checa o `costs` dos hosts ssh, o grok e o frescor dos preços.

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};

use crate::collect::{CmdOutput, CollectError, REMOTE_COSTS_PATH, Runner, remote_stdout, ssh_args};
use crate::config::{Host, HostKind, expand_tilde};
use crate::store::Store;

/// Idade máxima dos preços antes do aviso.
pub const PRICES_MAX_AGE_DAYS: f64 = 30.0;
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
const SECS_PER_DAY: f64 = 86400.0;
const GROK_CHECK: &str = "command -v grok || test -x \"$HOME/.grok/bin/grok\"";

/// Versão na saída de `costs --version` (`costs 0.1.0`): último token da última linha.
pub fn reported_version(text: &str) -> Option<&str> {
    text.trim().lines().last()?.split_whitespace().last()
}

/// Idade em dias pelo `fetched_at`; `None` se faltar ou for inválido.
pub fn prices_age_days(fetched_at: &str, now: DateTime<Utc>) -> Option<f64> {
    let ts = DateTime::parse_from_rfc3339(fetched_at)
        .map(|d| d.with_timezone(&Utc))
        .or_else(|_| NaiveDateTime::parse_from_str(fetched_at, "%Y-%m-%dT%H:%M:%S%.f").map(|n| n.and_utc()))
        .ok()?;
    Some(((now - ts).num_milliseconds() as f64 / 1000.0 / SECS_PER_DAY).max(0.0))
}

fn prices_fetched_at(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("fetched_at")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn check_prices(prices: &Path, now: DateTime<Utc>, out: &mut dyn Write) -> io::Result<u32> {
    let name = prices
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    if !prices.exists() {
        writeln!(out, "FALHA local      {name} ausente (rode `costs prices`)")?;
        return Ok(1);
    }
    let Some(age) = prices_fetched_at(prices).and_then(|f| prices_age_days(&f, now)) else {
        writeln!(out, "FALHA local      {name} sem fetched_at legivel")?;
        return Ok(1);
    };
    let stale = age > PRICES_MAX_AGE_DAYS;
    let (tag, hint) = if stale {
        ("AVISO", " (rode `costs prices`)")
    } else {
        ("ok  ", "")
    };
    writeln!(out, "{tag} local      {name} com {age:.0} dias{hint}")?;
    Ok(0)
}

fn check_grok(runner: &dyn Runner, host: &Host) -> bool {
    if host.kind == HostKind::Local {
        let home_bin = expand_tilde("~/.grok/bin/grok");
        return which::which("grok").is_ok() || home_bin.is_file();
    }
    remote_ok(runner, host, GROK_CHECK)
}

fn remote_ok(runner: &dyn Runner, host: &Host, body: &str) -> bool {
    run_remote(runner, host, body).is_ok_and(|o| o.code == Some(0))
}

fn run_remote(runner: &dyn Runner, host: &Host, body: &str) -> io::Result<CmdOutput> {
    let script = match host.path_prefix.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => format!("{p}\n{body}\n"),
        None => format!("{body}\n"),
    };
    let target = host.target.as_deref().unwrap_or_default();
    runner.run(
        "ssh",
        &ssh_args(target, "sh -s"),
        Some(script.as_bytes()),
        CHECK_TIMEOUT,
    )
}

/// `costs --version` no host ssh: FALHA se ausente/inacessível, AVISO se a versão
/// difere da local (o protocolo de `costs collect` pode ter mudado).
fn check_remote_costs(runner: &dyn Runner, host: &Host, local: &str, out: &mut dyn Write) -> io::Result<u32> {
    let name = &host.name;
    let output = match run_remote(runner, host, &format!("\"{REMOTE_COSTS_PATH}\" --version")) {
        Ok(o) => o,
        Err(e) if e.kind() == io::ErrorKind::TimedOut => {
            writeln!(out, "FALHA {name:<12} timeout")?;
            return Ok(1);
        }
        Err(e) => {
            writeln!(out, "FALHA {name:<12} ssh: {e}")?;
            return Ok(1);
        }
    };
    let version = match remote_stdout(&output, name) {
        Ok(stdout) => reported_version(stdout),
        Err(CollectError::Unreachable(m)) => {
            writeln!(out, "FALHA {name:<12} inacessivel: {m}")?;
            return Ok(1);
        }
        Err(CollectError::Failed(m)) => {
            writeln!(out, "FALHA {name:<12} {m}")?;
            return Ok(1);
        }
    };
    match version {
        Some(v) if v == local => writeln!(out, "ok   {name:<12} costs {v}")?,
        Some(v) => writeln!(
            out,
            "AVISO {name:<12} costs {v} (local {local}; atualize o ~/.local/bin/costs do host)"
        )?,
        None => {
            writeln!(out, "FALHA {name:<12} costs sem versao na saida")?;
            return Ok(1);
        }
    }
    Ok(0)
}

/// Avisa quando o banco tem uso sem preço (sintoma de catálogo incompleto).
fn check_unpriced(db: &Path, out: &mut dyn Write) -> io::Result<()> {
    let Ok(store) = Store::open_readonly(db) else {
        return Ok(());
    };
    let Ok((rows, models, tokens)) = store.unpriced() else {
        return Ok(());
    };
    if rows > 0 {
        writeln!(
            out,
            "AVISO local      {rows} linhas sem preço em {models} modelos ({tokens} tokens; rode `costs repricing`)"
        )?;
    }
    Ok(())
}

/// Roda as checagens e devolve o código de saída (1 se alguma FALHA).
pub fn run_doctor(
    runner: &dyn Runner,
    hosts: &[&Host],
    prices: &Path,
    db: &Path,
    now: DateTime<Utc>,
    out: &mut dyn Write,
) -> io::Result<i32> {
    let mut bad = check_prices(prices, now, out)?;
    check_unpriced(db, out)?;
    for host in hosts {
        if host.agents.iter().any(|a| a == "grok") {
            let ok = check_grok(runner, host);
            let word = if ok { "encontrado" } else { "nao encontrado" };
            writeln!(
                out,
                "{} {:<12} grok {word}",
                if ok { "ok  " } else { "FALHA" },
                host.name
            )?;
            bad += u32::from(!ok);
        }
        if !host.is_native() && !host.agents.is_empty() {
            bad += check_remote_costs(runner, host, crate::version(), out)?;
        }
    }
    Ok(i32::from(bad > 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::fake::{FakeRunner, exit, ok};
    use crate::config::HostSource;
    use chrono::TimeZone;

    fn host(kind: HostKind, agents: &[&str]) -> Host {
        Host {
            name: "srv".into(),
            kind,
            target: Some("srv".into()),
            enabled: true,
            agents: agents.iter().map(|s| (*s).to_string()).collect(),
            path_prefix: Some("export X=1".into()),
            source: if kind == HostKind::Local {
                HostSource::Native
            } else {
                HostSource::Remote
            },
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 10, 0, 0, 0).unwrap()
    }

    fn prices_file(dir: &Path, body: &str) -> std::path::PathBuf {
        let p = dir.join("prices.json");
        std::fs::write(&p, body).unwrap();
        p
    }

    fn fresh(dir: &Path) -> std::path::PathBuf {
        prices_file(dir, r#"{"fetched_at": "2026-05-09T00:00:00+00:00", "models": {}}"#)
    }

    fn run(r: &FakeRunner, hosts: &[&Host], prices: &Path) -> (i32, String) {
        let mut out = Vec::new();
        let rc = run_doctor(r, hosts, prices, Path::new("nao-existe.sqlite"), now(), &mut out).unwrap();
        (rc, String::from_utf8(out).unwrap())
    }

    #[test]
    fn reported_version_is_last_token_of_last_line() {
        assert_eq!(reported_version("costs 0.1.0\n"), Some("0.1.0"));
        assert_eq!(reported_version("aviso\ncosts 1.2.3"), Some("1.2.3"));
        assert_eq!(reported_version("  \n"), None);
    }

    #[test]
    fn age_handles_formats_and_invalid() {
        let age = prices_age_days("2026-05-08T00:00:00+00:00", now()).unwrap();
        assert!((age - 2.0).abs() < 1e-9);
        assert!(prices_age_days("2026-05-08T00:00:00", now()).is_some());
        assert_eq!(prices_age_days("2026-06-01T00:00:00Z", now()), Some(0.0));
        assert_eq!(prices_age_days("lixo", now()), None);
    }

    #[test]
    fn fresh_prices_and_same_remote_version_pass() {
        let d = tempfile::tempdir().unwrap();
        let p = fresh(d.path());
        let version = format!("costs {}\n", crate::version());
        let r = FakeRunner::new(Box::new(move |_, _, _| ok(&version)));
        let h = host(HostKind::Ssh, &["claude"]);
        let (rc, out) = run(&r, &[&h], &p);
        assert_eq!(rc, 0);
        assert_eq!(
            out,
            format!(
                "ok   local      prices.json com 1 dias\nok   srv          costs {}\n",
                crate::version()
            )
        );
        let calls = r.calls.lock().unwrap();
        let script = calls[0].2.as_deref().unwrap();
        assert!(script.starts_with("export X=1\n"));
        assert!(script.contains("\"$HOME/.local/bin/costs\" --version"));
    }

    #[test]
    fn remote_version_mismatch_is_a_warning_not_a_failure() {
        let d = tempfile::tempdir().unwrap();
        let r = FakeRunner::new(Box::new(|_, _, _| ok("costs 0.0.0-velho\n")));
        let h = host(HostKind::Ssh, &["claude"]);
        let (rc, out) = run(&r, &[&h], &fresh(d.path()));
        assert_eq!(rc, 0);
        assert!(
            out.contains(&format!(
                "AVISO srv          costs 0.0.0-velho (local {}; atualize",
                crate::version()
            )),
            "{out}"
        );
    }

    #[test]
    fn remote_costs_missing_unreachable_empty_and_errors_fail() {
        let d = tempfile::tempdir().unwrap();
        let p = fresh(d.path());
        let h = host(HostKind::Ssh, &["claude"]);
        type Reply = fn() -> io::Result<CmdOutput>;
        let cases: [(Reply, &str); 5] = [
            (
                || exit(127, "sh: costs: not found"),
                "costs ausente em srv: instale em ~/.local/bin/costs",
            ),
            (|| exit(255, "refused"), "inacessivel: refused"),
            (|| ok(""), "costs sem versao na saida"),
            (
                || Err(io::Error::new(io::ErrorKind::TimedOut, "t")),
                "FALHA srv          timeout",
            ),
            (|| Err(io::Error::new(io::ErrorKind::NotFound, "nf")), "ssh: nf"),
        ];
        for (reply, want) in cases {
            let r = FakeRunner::new(Box::new(move |_, _, _| reply()));
            let (rc, out) = run(&r, &[&h], &p);
            assert_eq!(rc, 1, "{want}");
            assert!(out.contains(want), "{want}: {out}");
        }
    }

    #[test]
    fn local_host_needs_no_remote_costs() {
        let d = tempfile::tempdir().unwrap();
        let r = FakeRunner::new(Box::new(|_, _, _| panic!("sem ssh para host local")));
        let h = host(HostKind::Local, &["claude"]);
        let (rc, out) = run(&r, &[&h], &fresh(d.path()));
        assert_eq!(rc, 0);
        assert!(!out.contains("srv"));
    }

    #[test]
    fn stale_missing_and_unreadable_prices() {
        let d = tempfile::tempdir().unwrap();
        let r = FakeRunner::new(Box::new(|_, _, _| ok("")));
        let stale = prices_file(d.path(), r#"{"fetched_at": "2026-01-01T00:00:00+00:00"}"#);
        let (rc, out) = run(&r, &[], &stale);
        assert_eq!(rc, 0);
        assert!(out.starts_with("AVISO local") && out.contains("(rode `costs prices`)"));
        let (rc, out) = run(&r, &[], &d.path().join("nope.json"));
        assert_eq!(rc, 1);
        assert!(out.contains("nope.json ausente"));
        for body in ["{}", "not json", r#"{"fetched_at": "x"}"#] {
            let p = prices_file(d.path(), body);
            let (rc, out) = run(&r, &[], &p);
            assert_eq!((rc, out.contains("sem fetched_at legivel")), (1, true), "{body}");
        }
    }

    #[test]
    fn unpriced_rows_in_the_db_are_a_warning() {
        use crate::model::Row;
        use crate::store::Store;

        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("store.sqlite");
        let store = Store::open(&db, "", now()).unwrap();
        store
            .upsert_rows(
                &[Row {
                    host: "mac".into(),
                    agent: "codex".into(),
                    date: "2026-10-01".into(),
                    model: "gpt-5.6-luna".into(),
                    model_raw: "gpt-5.6-luna".into(),
                    provider: "openai".into(),
                    input: 1_000_000,
                    output: 100,
                    cache_read: 0,
                    cache_write: 0,
                    cost_usd: None,
                    cost_basis: "estimated".into(),
                }],
                "t",
            )
            .unwrap();
        let p = fresh(d.path());
        let r = FakeRunner::new(Box::new(|_, _, _| ok("")));
        let mut out = Vec::new();
        run_doctor(&r, &[], &p, &db, now(), &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("AVISO local      1 linhas sem preço em 1 modelos"),
            "{out}"
        );
        assert!(out.contains("rode `costs repricing`"), "{out}");
    }

    #[test]
    fn remote_grok_missing_fails_and_found_passes() {
        let d = tempfile::tempdir().unwrap();
        let p = fresh(d.path());
        let version = format!("costs {}\n", crate::version());
        let r = FakeRunner::new(Box::new(move |_, _, stdin| {
            let s = String::from_utf8_lossy(stdin.unwrap()).into_owned();
            if s.contains("command -v grok") {
                exit(1, "")
            } else {
                ok(&version)
            }
        }));
        let h = host(HostKind::Ssh, &["claude", "grok"]);
        let (rc, out) = run(&r, &[&h], &p);
        assert_eq!(rc, 1);
        assert!(out.contains("FALHA srv          grok nao encontrado"));
        assert!(out.contains("ok   srv          costs"));

        let r = FakeRunner::new(Box::new(|_, _, _| ok("costs 0.1.0")));
        let h = host(HostKind::Ssh, &["grok"]);
        let (_, out) = run(&r, &[&h], &p);
        assert!(out.contains("ok   srv          grok encontrado"));
    }
}
