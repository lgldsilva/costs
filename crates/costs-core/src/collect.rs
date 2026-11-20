//! Coleta em hosts ssh: uma única chamada `ssh` por host roda o `costs collect` instalado
//! lá, com o contexto (regras e preços) por stdin, e devolve as linhas de cada agente.
//!
//! O formato do contexto e da resposta está em `remote`.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Host;
use crate::model::{Row, Rules};
use crate::remote::{AgentOutcome, adopt_rows, parse_response};

/// Tempo máximo de uma coleta. A mais pesada observada (7 agentes num host ssh)
/// leva ~90s; 180s dá folga sem pendurar o sync num host que aceita conexão e não
/// responde (notebook que dormiu no meio, rota preta via ProxyJump).
pub const TIMEOUT: Duration = Duration::from_secs(180);
/// Binário do `costs` no host remoto (expandido pelo shell remoto).
pub const REMOTE_COSTS_PATH: &str = "$HOME/.local/bin/costs";
const DETAIL_TAIL_CHARS: usize = 300;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Espera pelos pipes depois que o processo acabou (ou foi morto).
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// ssh não conectou.
pub const SSH_UNREACHABLE: i32 = 255;
/// Shell remoto não achou o comando.
const COMMAND_NOT_FOUND: i32 = 127;
/// Erro do clap para subcomando inexistente (`costs` anterior ao `collect`).
const UNKNOWN_SUBCOMMAND: &str = "unrecognized subcommand";
/// Opções do ssh: sem interatividade, limite na conexão TCP local e detecção de par
/// morto (`ServerAlive*`: host que dorme no meio da coleta, ou mux ControlMaster
/// parado, aborta em ~15s em vez de esperar o tempo do kernel — o `ConnectTimeout`
/// não cobre o segundo salto do ProxyJump nem a sessão já estabelecida).
const SSH_OPTIONS: [&str; 8] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=5",
    "-o",
    "ServerAliveCountMax=3",
];

/// Saída de um processo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Executa processos (injetável nos testes).
pub trait Runner: Sync {
    fn run(&self, program: &str, args: &[String], stdin: Option<&[u8]>, timeout: Duration) -> io::Result<CmdOutput>;
}

/// Runner real, baseado em `std::process`.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[String], stdin: Option<&[u8]>, timeout: Duration) -> io::Result<CmdOutput> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Grupo próprio: no timeout morre a árvore inteira, não só o filho direto.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd.spawn()?;
        if let (Some(mut h), Some(data)) = (child.stdin.take(), stdin) {
            let data = data.to_vec();
            // o filho pode fechar o stdin antes
            thread::spawn(move || drop(h.write_all(&data)));
        }
        let out = drain_in_background(child.stdout.take());
        let err = drain_in_background(child.stderr.take());
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(st) = child.try_wait()? {
                break Some(st);
            }
            if Instant::now() >= deadline {
                kill_tree(&mut child);
                break None;
            }
            thread::sleep(POLL_INTERVAL);
        };
        // Um neto que herdou os pipes (ControlMaster do ssh, `sh` sem exec) não pode prender o
        // retorno: a leitura é abandonada depois de um respiro.
        let stdout = out.recv_timeout(DRAIN_GRACE).unwrap_or_default();
        let stderr = err.recv_timeout(DRAIN_GRACE).unwrap_or_default();
        match status {
            Some(st) => Ok(CmdOutput {
                code: st.code(),
                stdout,
                stderr,
            }),
            None => Err(io::Error::new(io::ErrorKind::TimedOut, "timeout")),
        }
    }
}

/// Mata o grupo de processos do filho (unix) e o próprio filho.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{}", child.id())])
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Lê o pipe inteiro numa thread solta; o resultado chega pelo canal.
fn drain_in_background<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    rx
}

/// Falha de coleta de um agente.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CollectError {
    /// ssh não conectou (exit 255): o resto do host é pulado.
    #[error("{0}")]
    Unreachable(String),
    #[error("{0}")]
    Failed(String),
}

/// Aspas simples POSIX.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Argumentos do ssh para executar `remote_command` no host.
pub fn ssh_args(target: &str, remote_command: &str) -> Vec<String> {
    let mut v: Vec<String> = SSH_OPTIONS.iter().map(|s| (*s).to_string()).collect();
    v.extend(["--".to_string(), target.to_string(), remote_command.to_string()]);
    v
}

/// `path_prefix` numa linha própria: aceita `;` ou comentário no fim sem quebrar o comando.
fn with_prefix(host: &Host, body: &str) -> String {
    match host.path_prefix.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => format!("{p}\n{body}"),
        None => body.to_string(),
    }
}

/// Prioridade baixa no host remoto: a coleta lê histórico de vários agentes e não deve
/// disputar disco e CPU com quem está usando a máquina (a machine with HDD-backed home).
/// `ionice` só existe no Linux; sem ele a substituição fica vazia.
const REMOTE_LOW_PRIORITY: &str = "nice -n 10 $(command -v ionice >/dev/null 2>&1 && echo ionice -c3)";

/// `<path_prefix>` + nova linha + `nice ... "$HOME/.local/bin/costs" collect --host H --agent A ... --context -`.
pub fn remote_command(host: &Host, agents: &[String]) -> String {
    let mut parts = vec![
        REMOTE_LOW_PRIORITY.to_string(),
        format!("\"{REMOTE_COSTS_PATH}\""),
        "collect".to_string(),
        "--host".to_string(),
        sh_quote(&host.name),
    ];
    for agent in agents {
        parts.push("--agent".to_string());
        parts.push(sh_quote(agent));
    }
    parts.extend(["--context".to_string(), "-".to_string()]);
    with_prefix(host, &parts.join(" "))
}

/// Mensagem quando o host não tem o `costs`.
pub fn missing_costs_msg(host: &str) -> String {
    format!("costs ausente em {host}: instale em ~/.local/bin/costs")
}

/// Mensagem quando o `costs` do host é anterior ao subcomando `collect`.
pub fn outdated_costs_msg(host: &str) -> String {
    format!("costs em {host} sem o subcomando collect: atualize ~/.local/bin/costs")
}

/// Trecho do caminho do binário remoto citado pelo shell quando ele não existe.
const REMOTE_COSTS_TAIL: &str = ".local/bin/costs";

/// Shells que não devolvem 127 ainda citam o caminho do binário ausente.
fn mentions_missing_binary(stderr: &str) -> bool {
    stderr.contains(REMOTE_COSTS_TAIL) && (stderr.contains("not found") || stderr.contains("No such file"))
}

fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

/// Últimos 300 caracteres de stderr (ou stdout), ou `exit N`.
pub fn failure_detail(out: &CmdOutput) -> String {
    let text = if out.stderr.trim().is_empty() {
        &out.stdout
    } else {
        &out.stderr
    };
    let tail = tail_chars(text.trim(), DETAIL_TAIL_CHARS);
    if tail.is_empty() {
        format!("exit {}", out.code.map_or_else(|| "?".to_string(), |c| c.to_string()))
    } else {
        tail
    }
}

/// Classifica a saída de um comando remoto: stdout se ok; 255 = inacessível;
/// 127 (ou o shell citando o binário ausente) = `costs` ausente no host; subcomando
/// desconhecido do clap = `costs` desatualizado.
pub fn remote_stdout<'o>(out: &'o CmdOutput, host: &str) -> Result<&'o str, CollectError> {
    match out.code {
        Some(0) => Ok(&out.stdout),
        Some(SSH_UNREACHABLE) => Err(CollectError::Unreachable(failure_detail(out))),
        Some(COMMAND_NOT_FOUND) => Err(CollectError::Failed(missing_costs_msg(host))),
        _ if mentions_missing_binary(&out.stderr) => Err(CollectError::Failed(missing_costs_msg(host))),
        _ if out.stderr.contains(UNKNOWN_SUBCOMMAND) => Err(CollectError::Failed(outdated_costs_msg(host))),
        _ => Err(CollectError::Failed(failure_detail(out))),
    }
}

/// Resultado por agente, na ordem pedida.
pub type Collected = Vec<(String, Result<Vec<Row>, CollectError>)>;

/// Coletor de hosts ssh.
pub struct Collector<'a> {
    pub runner: &'a dyn Runner,
    /// Contexto JSON (`remote::context_json`) enviado por stdin.
    pub context: &'a str,
    /// Regras locais: o provedor das linhas recebidas é recalculado com elas.
    pub rules: &'a Rules,
}

impl Collector<'_> {
    /// Coleta os agentes do host numa única chamada. Falha de um agente não afeta os
    /// outros; ssh inacessível encerra o host (só o primeiro agente é registrado).
    pub fn collect_remote(&self, host: &Host, agents: &[String]) -> Collected {
        let Some(first) = agents.first() else {
            return Vec::new();
        };
        let map = match self.fetch(host, agents) {
            Ok(map) => map,
            Err(e @ CollectError::Unreachable(_)) => return vec![(first.clone(), Err(e))],
            Err(e) => return agents.iter().map(|a| (a.clone(), Err(e.clone()))).collect(),
        };
        agents
            .iter()
            .map(|agent| {
                let result = match map.get(agent) {
                    Some(AgentOutcome::Rows(rows)) => Ok(adopt_rows(rows.clone(), &host.name, agent, self.rules)),
                    Some(AgentOutcome::Error(msg)) => Err(CollectError::Failed(msg.clone())),
                    None => Err(CollectError::Failed("agente ausente na saida remota".to_string())),
                };
                (agent.clone(), result)
            })
            .collect()
    }

    fn fetch(&self, host: &Host, agents: &[String]) -> Result<BTreeMap<String, AgentOutcome>, CollectError> {
        let target = host
            .target
            .as_deref()
            .ok_or_else(|| CollectError::Failed(format!("host {} sem target", host.name)))?;
        let args = ssh_args(target, &remote_command(host, agents));
        let out = match self.runner.run("ssh", &args, Some(self.context.as_bytes()), TIMEOUT) {
            Ok(o) => o,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => return Err(CollectError::Failed("timeout".to_string())),
            Err(e) => return Err(CollectError::Failed(format!("ssh: {e}"))),
        };
        let stdout = remote_stdout(&out, &host.name)?;
        parse_response(stdout, &host.name).map_err(CollectError::Failed)
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::Mutex;

    pub type Handler = Box<dyn Fn(&str, &[String], Option<&[u8]>) -> io::Result<CmdOutput> + Send + Sync>;

    pub type Call = (String, Vec<String>, Option<String>);

    /// Runner de teste: registra chamadas e responde via closure.
    pub struct FakeRunner {
        pub calls: Mutex<Vec<Call>>,
        handler: Handler,
    }

    impl FakeRunner {
        pub fn new(handler: Handler) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                handler,
            }
        }
    }

    pub fn ok(stdout: &str) -> io::Result<CmdOutput> {
        Ok(CmdOutput {
            code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }

    pub fn exit(code: i32, stderr: &str) -> io::Result<CmdOutput> {
        Ok(CmdOutput {
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.to_string(),
        })
    }

    impl Runner for FakeRunner {
        fn run(&self, program: &str, args: &[String], stdin: Option<&[u8]>, _t: Duration) -> io::Result<CmdOutput> {
            let text = stdin.map(|b| String::from_utf8_lossy(b).into_owned());
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec(), text));
            (self.handler)(program, args, stdin)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeRunner, exit, ok};
    use super::*;
    use crate::config::{HostKind, HostSource};
    use crate::model::load_rules;
    use crate::remote::{COLLECT_PROTOCOL_VERSION, collect_agents};

    fn host() -> Host {
        Host {
            name: "h".into(),
            kind: HostKind::Ssh,
            target: Some("srv".into()),
            enabled: true,
            agents: vec![],
            path_prefix: Some("export PATH=\"$HOME/.local/bin:$PATH\"".into()),
            source: HostSource::Remote,
        }
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn row(host: &str, agent: &str, model: &str) -> Row {
        Row {
            host: host.into(),
            agent: agent.into(),
            date: "2026-05-01".into(),
            model: model.into(),
            model_raw: model.into(),
            provider: "remoto".into(),
            input: 1,
            output: 2,
            cache_read: 0,
            cache_write: 0,
            cost_usd: Some(0.1),
            cost_basis: "estimated".into(),
        }
    }

    /// Resposta de `costs collect`: `claude` com uma linha, os demais com erro.
    fn response(agents: &[&str]) -> String {
        let agents = strs(agents);
        collect_agents("nome-remoto", &agents, &|agent, host| match agent {
            "claude" => Ok(vec![row(host, agent, "claude-x")]),
            _ => Err(format!("{agent} falhou")),
        })
        .to_json()
        .unwrap()
    }

    fn rules() -> Rules {
        load_rules("[[rules]]\nmatch = \"^claude\"\nprovider = \"anthropic\"\n").unwrap()
    }

    fn run_collect(r: &FakeRunner, h: &Host, agents: &[&str]) -> Collected {
        let rules = rules();
        let c = Collector {
            runner: r,
            context: "{\"ctx\":1}",
            rules: &rules,
        };
        c.collect_remote(h, &strs(agents))
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn remote_command_quotes_host_and_agents() {
        let mut h = host();
        h.name = "o'malley host".into();
        let cmd = remote_command(&h, &strs(&["claude", "we'ird; rm"]));
        assert_eq!(
            cmd,
            "export PATH=\"$HOME/.local/bin:$PATH\"\nnice -n 10 $(command -v ionice >/dev/null 2>&1 && echo ionice -c3) \
             \"$HOME/.local/bin/costs\" collect --host 'o'\\''malley host' --agent 'claude' --agent 'we'\\''ird; rm' --context -"
        );
        h.path_prefix = Some(String::new());
        assert!(remote_command(&h, &[]).starts_with(&format!(
            "{REMOTE_LOW_PRIORITY} \"$HOME/.local/bin/costs\" collect --host"
        )));
        let args = ssh_args("srv", &cmd);
        assert_eq!(
            args[..9],
            strs(&[
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=3",
                "--",
            ])[..]
        );
        assert_eq!(args[9..], [String::from("srv"), cmd][..]);
        assert!(!args.join(" ").contains("Control"));
    }

    #[test]
    fn one_ssh_call_sends_context_and_relabels_rows() {
        let payload = response(&["claude", "pi"]);
        let r = FakeRunner::new(Box::new(move |_, _, _| ok(&payload)));
        let res = run_collect(&r, &host(), &["claude", "pi"]);
        assert_eq!(res.len(), 2);
        let rows = res[0].1.as_ref().unwrap();
        assert_eq!(rows[0].host, "h");
        assert_eq!(rows[0].provider, "anthropic");
        assert_eq!(res[1].1, Err(CollectError::Failed("pi falhou".into())));
        let calls = r.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "ssh");
        assert_eq!(calls[0].2.as_deref(), Some("{\"ctx\":1}"));
        let cmd = calls[0].1.last().unwrap();
        assert!(cmd.contains("--agent 'claude' --agent 'pi' --context -"), "{cmd}");
    }

    #[test]
    fn missing_agent_in_response_fails_only_that_agent() {
        let payload = response(&["claude"]);
        let r = FakeRunner::new(Box::new(move |_, _, _| ok(&payload)));
        let res = run_collect(&r, &host(), &["claude", "kimi"]);
        assert!(res[0].1.is_ok());
        assert_eq!(
            res[1].1,
            Err(CollectError::Failed("agente ausente na saida remota".into()))
        );
    }

    #[test]
    fn version_mismatch_and_malformed_fail_every_agent() {
        let other = format!(r#"{{"version":{},"agents":{{}}}}"#, COLLECT_PROTOCOL_VERSION + 1);
        let r = FakeRunner::new(Box::new(move |_, _, _| ok(&other)));
        let res = run_collect(&r, &host(), &["claude", "pi"]);
        assert_eq!(res.len(), 2);
        assert!(
            res.iter()
                .all(|(_, x)| matches!(x, Err(CollectError::Failed(m)) if m.contains("atualize")))
        );
        let r = FakeRunner::new(Box::new(|_, _, _| ok("garbage")));
        let res = run_collect(&r, &host(), &["claude", "pi"]);
        assert!(
            res.iter()
                .all(|(_, x)| matches!(x, Err(CollectError::Failed(m)) if m.contains("invalida")))
        );
    }

    #[test]
    fn exit_255_is_unreachable_and_stops_host() {
        let r = FakeRunner::new(Box::new(|_, _, _| exit(255, "Connection refused")));
        let res = run_collect(&r, &host(), &["claude", "codex", "grok"]);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].1, Err(CollectError::Unreachable("Connection refused".into())));
    }

    #[test]
    fn missing_costs_binary_has_install_hint() {
        let want = Err(CollectError::Failed(
            "costs ausente em h: instale em ~/.local/bin/costs".into(),
        ));
        let r = FakeRunner::new(Box::new(|_, _, _| exit(127, "")));
        let res = run_collect(&r, &host(), &["claude", "pi"]);
        assert!(res.iter().all(|(_, x)| *x == want));
        for stderr in [
            "/home/u/.local/bin/costs: Command not found.",
            "fish: /home/u/.local/bin/costs: No such file or directory",
        ] {
            let r = FakeRunner::new(Box::new(move |_, _, _| exit(1, stderr)));
            assert_eq!(run_collect(&r, &host(), &["claude"])[0].1, want, "{stderr}");
        }
    }

    #[test]
    fn unrelated_not_found_keeps_the_real_error() {
        let r = FakeRunner::new(Box::new(|_, _, _| exit(1, "sh: .cargo/env: not found\nerro: x")));
        assert_eq!(
            run_collect(&r, &host(), &["claude"])[0].1,
            Err(CollectError::Failed("sh: .cargo/env: not found\nerro: x".into()))
        );
    }

    #[test]
    fn costs_without_collect_subcommand_asks_for_update() {
        let stderr = "error: unrecognized subcommand 'collect'\n\nUsage: costs <COMMAND>\n";
        let r = FakeRunner::new(Box::new(move |_, _, _| exit(2, stderr)));
        let res = run_collect(&r, &host(), &["claude", "pi"]);
        let want = Err(CollectError::Failed(
            "costs em h sem o subcomando collect: atualize ~/.local/bin/costs".into(),
        ));
        assert!(res.iter().all(|(_, x)| *x == want), "{res:?}");
    }

    #[cfg(unix)]
    #[test]
    fn path_prefix_ending_in_semicolon_or_comment_still_runs_the_command() {
        let mut h = host();
        for prefix in ["export A=1;", "export A=1 # comentario"] {
            h.path_prefix = Some(prefix.into());
            let cmd = remote_command(&h, &strs(&["claude"])).replace(&format!("\"{REMOTE_COSTS_PATH}\""), "echo");
            let o = SystemRunner
                .run("sh", &strs(&["-c", &cmd]), None, Duration::from_secs(10))
                .unwrap();
            assert_eq!(o.code, Some(0), "{prefix}: {}", o.stderr);
            assert!(
                o.stdout.starts_with("collect --host h --agent claude"),
                "{prefix}: {}",
                o.stdout
            );
        }
    }

    #[test]
    fn other_exit_codes_and_runner_errors_fail_every_agent() {
        let r = FakeRunner::new(Box::new(|_, _, _| exit(2, "falhou feio")));
        let res = run_collect(&r, &host(), &["claude", "pi"]);
        assert!(
            res.iter()
                .all(|(_, x)| *x == Err(CollectError::Failed("falhou feio".into())))
        );
        let r = FakeRunner::new(Box::new(|_, _, _| Err(io::Error::new(io::ErrorKind::TimedOut, "t"))));
        assert_eq!(
            run_collect(&r, &host(), &["claude"])[0].1,
            Err(CollectError::Failed("timeout".into()))
        );
        let r = FakeRunner::new(Box::new(|_, _, _| Err(io::Error::new(io::ErrorKind::NotFound, "nf"))));
        assert!(matches!(&run_collect(&r, &host(), &["claude"])[0].1, Err(CollectError::Failed(m)) if m == "ssh: nf"));
    }

    #[test]
    fn no_target_or_no_agents() {
        let mut h = host();
        h.target = None;
        let r = FakeRunner::new(Box::new(|_, _, _| panic!("sem chamada")));
        let res = run_collect(&r, &h, &["claude", "grok"]);
        assert!(res.iter().all(|(_, x)| x.is_err()));
        assert!(run_collect(&r, &host(), &[]).is_empty());
        assert!(r.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn failure_detail_prefers_stderr_and_falls_back_to_exit() {
        assert_eq!(failure_detail(&exit(3, "  bad  ").unwrap()), "bad");
        assert_eq!(failure_detail(&exit(3, "").unwrap()), "exit 3");
        let long = "x".repeat(400);
        assert_eq!(failure_detail(&exit(1, &long).unwrap()).len(), 300);
        let none = CmdOutput {
            code: None,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert_eq!(failure_detail(&none), "exit ?");
    }

    #[cfg(unix)]
    #[test]
    fn system_runner_runs_pipes_stdin_and_times_out() {
        let o = SystemRunner
            .run(
                "sh",
                &strs(&["-c", "cat; echo err >&2; exit 3"]),
                Some(b"hi"),
                Duration::from_secs(10),
            )
            .unwrap();
        assert_eq!((o.code, o.stdout.as_str(), o.stderr.trim()), (Some(3), "hi", "err"));
        let e = SystemRunner
            .run("sh", &strs(&["-c", "sleep 5"]), None, Duration::from_millis(100))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        // `sh` sem exec: o `sleep` neto herda os pipes e sobrevive ao kill do filho direto
        // (como um ControlMaster do ssh). O timeout tem de valer mesmo assim.
        let started = Instant::now();
        let e = SystemRunner
            .run("sh", &strs(&["-c", "sleep 5; true"]), None, Duration::from_millis(100))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        // Invariante: retornou ANTES do `sleep 5` do neto completar (o timeout
        // de 100ms valer para processos que herdam os pipes). O bound absoluto
        // depende do agendador — em runner carregado o retorno pode levar
        // segundos; 5s é o teto do próprio sleep do filho.
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert!(
            SystemRunner
                .run("/nonexistent/x", &[], None, Duration::from_secs(1))
                .is_err()
        );
    }
}
