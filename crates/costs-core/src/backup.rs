//! Comando backup: cópia consistente do SQLite, retenção local e envio opcional por ssh.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};

use crate::collect::{Runner, sh_quote, ssh_args};

/// Quantas cópias manter por padrão.
pub const DEFAULT_KEEP: usize = 30;
const PAGES_PER_STEP: std::ffi::c_int = 100;
const SHORT_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const EXTENSION: &str = ".sqlite";

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("--keep precisa ser >= 1")]
    Keep,
    #[error("banco nao encontrado: {0}")]
    MissingDb(PathBuf),
    #[error("erro de E/S em {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("erro SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Remote(String),
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> BackupError + '_ {
    move |source| BackupError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn check_keep(keep: usize) -> Result<(), BackupError> {
    if keep < 1 { Err(BackupError::Keep) } else { Ok(()) }
}

fn db_stem(src: &Path) -> String {
    src.file_stem()
        .map_or_else(|| "store".to_string(), |s| s.to_string_lossy().into_owned())
}

/// Copia `src` com a API de backup do SQLite e poda as mais antigas, mantendo `keep`.
pub fn backup_db(src: &Path, dest_dir: &Path, keep: usize, now: DateTime<Utc>) -> Result<PathBuf, BackupError> {
    check_keep(keep)?;
    if !src.exists() {
        return Err(BackupError::MissingDb(src.to_path_buf()));
    }
    fs::create_dir_all(dest_dir).map_err(io_err(dest_dir))?;
    let stem = db_stem(src);
    let target = dest_dir.join(format!("{stem}-{}Z{EXTENSION}", now.format("%Y%m%dT%H%M%S%6f")));
    let source = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut dest = Connection::open(&target)?;
    Backup::new(&source, &mut dest)?.run_to_completion(PAGES_PER_STEP, Duration::ZERO, None)?;
    drop(dest);
    prune_local(dest_dir, &format!("{stem}-"), keep)?;
    Ok(target)
}

fn prune_local(dir: &Path, prefix: &str, keep: usize) -> Result<(), BackupError> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map_err(io_err(dir))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(prefix) && n.ends_with(EXTENSION))
        .collect();
    names.sort_by(|a, b| b.cmp(a));
    for old in names.into_iter().skip(keep) {
        let p = dir.join(old);
        fs::remove_file(&p).map_err(io_err(&p))?;
    }
    Ok(())
}

fn ssh_run(
    runner: &dyn Runner,
    remote: &str,
    command: &str,
    stdin: Option<&[u8]>,
    t: Duration,
) -> Result<(), BackupError> {
    let out = runner
        .run("ssh", &ssh_args(remote, command), stdin, t)
        .map_err(|e| BackupError::Remote(format!("ssh: {e}")))?;
    if out.code == Some(0) {
        Ok(())
    } else {
        let code = out.code.map_or_else(|| "?".to_string(), |c| c.to_string());
        Err(BackupError::Remote(format!("exit {code}")))
    }
}

/// O host responde? (`ssh ... exit 0`).
pub fn remote_reachable(runner: &dyn Runner, remote: &str) -> bool {
    ssh_run(runner, remote, "exit 0", None, SHORT_TIMEOUT).is_ok()
}

/// Script POSIX portátil: renomeia o `.part` e poda o destino por ordem de nome.
pub fn remote_finalize_script(remote_dir: &str, name: &str, prefix: &str, keep: usize) -> String {
    let dir = sh_quote(remote_dir.trim_end_matches('/'));
    let part = sh_quote(&format!("{name}.part"));
    let target = sh_quote(name);
    format!(
        "cd {dir} && mv {part} {target} && n=0 && for f in $(ls -1r {prefix}*{EXTENSION} 2>/dev/null); do \
n=$((n+1)); if [ \"$n\" -gt {keep} ]; then rm -f -- \"$f\"; fi; done\n",
        prefix = sh_quote(prefix).trim_matches('\''),
    )
}

/// Envia por ssh de forma atômica (`.part` + rename) e aplica a retenção no destino.
pub fn upload(
    runner: &dyn Runner,
    path: &Path,
    remote: &str,
    remote_dir: &str,
    keep: usize,
) -> Result<(), BackupError> {
    check_keep(keep)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = name.split('-').next().map_or_else(String::new, |s| format!("{s}-"));
    let dir = remote_dir.trim_end_matches('/');
    let data = fs::read(path).map_err(io_err(path))?;
    ssh_run(
        runner,
        remote,
        &format!("mkdir -p {}", sh_quote(dir)),
        None,
        SHORT_TIMEOUT,
    )?;
    let part = format!("{dir}/{name}.part");
    ssh_run(
        runner,
        remote,
        &format!("cat > {}", sh_quote(&part)),
        Some(&data),
        UPLOAD_TIMEOUT,
    )?;
    let script = remote_finalize_script(dir, &name, &prefix, keep);
    let out = runner
        .run(
            "ssh",
            &ssh_args(remote, "sh -s"),
            Some(script.as_bytes()),
            SHORT_TIMEOUT,
        )
        .map_err(|e| BackupError::Remote(format!("ssh: {e}")))?;
    if out.code == Some(0) {
        Ok(())
    } else {
        Err(BackupError::Remote(format!("exit {:?}", out.code)))
    }
}

/// Destino remoto opcional: `(host ssh, diretório)`.
pub struct RemoteTarget<'a> {
    pub remote: &'a str,
    pub dir: &'a str,
}

/// Parâmetros do backup completo.
pub struct BackupPlan<'a> {
    pub db: &'a Path,
    pub dest_dir: &'a Path,
    pub keep: usize,
    pub remote: Option<RemoteTarget<'a>>,
}

/// Backup completo; devolve o código de saída.
pub fn run_backup(
    runner: &dyn Runner,
    plan: BackupPlan,
    now: DateTime<Utc>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, BackupError> {
    let BackupPlan {
        db,
        dest_dir,
        keep,
        remote,
    } = plan;
    let path = backup_db(db, dest_dir, keep, now)?;
    let size = fs::metadata(&path).map_err(io_err(&path))?.len();
    writeln!(out, "ok: {} ({:.0} KB)", path.display(), size as f64 / 1024.0).ok();
    let Some(target) = remote else { return Ok(0) };
    if !remote_reachable(runner, target.remote) {
        writeln!(err, "ERRO: {} inacessivel; copia local mantida", target.remote).ok();
        return Ok(1);
    }
    match upload(runner, &path, target.remote, target.dir, keep) {
        Ok(()) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            writeln!(
                out,
                "ok: enviado para {}:{}/{name}",
                target.remote,
                target.dir.trim_end_matches('/')
            )
            .ok();
            Ok(0)
        }
        Err(e) => {
            writeln!(
                err,
                "ERRO: envio para {} falhou ({e}); copia local mantida",
                target.remote
            )
            .ok();
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::fake::{FakeRunner, exit, ok};
    use chrono::TimeZone;

    fn make_db(dir: &Path) -> PathBuf {
        let p = dir.join("store.sqlite");
        let c = Connection::open(&p).unwrap();
        c.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (42);")
            .unwrap();
        p
    }

    fn at(sec: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 1, 10, 0, sec).unwrap()
    }

    #[test]
    fn backup_copies_data_names_by_utc_and_applies_retention() {
        let d = tempfile::tempdir().unwrap();
        let db = make_db(d.path());
        let dest = d.path().join("bk");
        let mut last = PathBuf::new();
        for s in 0..4 {
            last = backup_db(&db, &dest, 2, at(s)).unwrap();
        }
        let mut names: Vec<_> = fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "store-20260501T100002000000Z.sqlite",
                "store-20260501T100003000000Z.sqlite"
            ]
        );
        let v: i64 = Connection::open(&last)
            .unwrap()
            .query_row("SELECT x FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 42);
        fs::write(dest.join("other.sqlite"), b"x").unwrap();
        backup_db(&db, &dest, 1, at(9)).unwrap();
        assert!(dest.join("other.sqlite").exists());
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 2);
    }

    #[test]
    fn backup_rejects_bad_keep_and_missing_db() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(
            backup_db(&d.path().join("x"), d.path(), 0, at(0)),
            Err(BackupError::Keep)
        ));
        assert!(matches!(
            backup_db(&d.path().join("x"), d.path(), 1, at(0)),
            Err(BackupError::MissingDb(_))
        ));
    }

    #[test]
    fn finalize_script_is_portable() {
        let s = remote_finalize_script("backups/costs/", "store-1.sqlite", "store-", 5);
        assert!(!s.contains("xargs"));
        assert!(s.contains("cd 'backups/costs'") && s.contains("mv 'store-1.sqlite.part' 'store-1.sqlite'"));
        assert!(s.contains("ls -1r store-*.sqlite") && s.contains("-gt 5"));
    }

    #[test]
    fn upload_sends_part_then_finalizes() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("store-1.sqlite");
        fs::write(&f, b"DATA").unwrap();
        let r = FakeRunner::new(Box::new(|_, _, _| ok("")));
        upload(&r, &f, "srv", "bk/costs", 3).unwrap();
        let calls = r.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].1.last().unwrap().starts_with("mkdir -p 'bk/costs'"));
        assert!(calls[1].1.last().unwrap().contains("bk/costs/store-1.sqlite.part"));
        assert_eq!(calls[1].2.as_deref(), Some("DATA"));
        assert!(calls[2].2.as_deref().unwrap().contains("mv "));
        assert!(calls.iter().all(|c| !c.1.join(" ").contains("Control")));
    }

    #[test]
    fn upload_failure_at_each_step() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("store-1.sqlite");
        fs::write(&f, b"D").unwrap();
        for fail_at in 0..3 {
            let n = std::sync::atomic::AtomicUsize::new(0);
            let r = FakeRunner::new(Box::new(move |_, _, _| {
                if n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == fail_at {
                    exit(1, "")
                } else {
                    ok("")
                }
            }));
            assert!(
                matches!(upload(&r, &f, "srv", "bk", 3), Err(BackupError::Remote(_))),
                "{fail_at}"
            );
        }
        let r = FakeRunner::new(Box::new(|_, _, _| Err(io::Error::other("spawn"))));
        assert!(upload(&r, &f, "srv", "bk", 3).is_err());
        assert!(matches!(upload(&r, &f, "srv", "bk", 0), Err(BackupError::Keep)));
    }

    #[test]
    fn run_backup_local_only_and_remote_paths() {
        let d = tempfile::tempdir().unwrap();
        let db = make_db(d.path());
        let dest = d.path().join("bk");
        let r = FakeRunner::new(Box::new(|_, _, _| ok("")));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            run_backup(
                &r,
                BackupPlan {
                    db: &db,
                    dest_dir: &dest,
                    keep: 2,
                    remote: None
                },
                at(1),
                &mut out,
                &mut err
            )
            .unwrap(),
            0
        );
        assert!(String::from_utf8(out).unwrap().starts_with("ok: "));
        assert!(r.calls.lock().unwrap().is_empty());

        let t = || {
            Some(RemoteTarget {
                remote: "srv",
                dir: "bk/",
            })
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            run_backup(
                &r,
                BackupPlan {
                    db: &db,
                    dest_dir: &dest,
                    keep: 2,
                    remote: t()
                },
                at(2),
                &mut out,
                &mut err
            )
            .unwrap(),
            0
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("ok: enviado para srv:bk/store-")
        );
        assert_eq!(r.calls.lock().unwrap()[0].1.last().unwrap(), "exit 0");

        let down = FakeRunner::new(Box::new(|_, _, _| exit(255, "")));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            run_backup(
                &down,
                BackupPlan {
                    db: &db,
                    dest_dir: &dest,
                    keep: 2,
                    remote: t()
                },
                at(3),
                &mut out,
                &mut err
            )
            .unwrap(),
            1
        );
        assert!(String::from_utf8(err).unwrap().contains("inacessivel"));

        let n = std::sync::atomic::AtomicUsize::new(0);
        let flaky = FakeRunner::new(Box::new(move |_, _, _| {
            if n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                ok("")
            } else {
                exit(1, "")
            }
        }));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            run_backup(
                &flaky,
                BackupPlan {
                    db: &db,
                    dest_dir: &dest,
                    keep: 2,
                    remote: t()
                },
                at(4),
                &mut out,
                &mut err
            )
            .unwrap(),
            1
        );
        assert!(String::from_utf8(err).unwrap().contains("envio para srv falhou"));
    }
}
