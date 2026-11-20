//! Ambiente injetável (variáveis, home, keychain) e cliente HTTP.

use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

/// Variáveis de ambiente e home, injetáveis em testes.
pub trait Env {
    /// Valor da variável; vazia conta como ausente.
    fn var(&self, key: &str) -> Option<String>;
    fn home(&self) -> Option<PathBuf>;
    /// JSON de credencial do Claude Code no Keychain (só macOS, quando o arquivo não existe).
    fn keychain_claude(&self) -> Option<String> {
        None
    }
}

/// Ambiente real do processo.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealEnv;

impl Env for RealEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }

    fn home(&self) -> Option<PathBuf> {
        self.var("HOME")
            .map(PathBuf::from)
            .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()))
    }

    fn keychain_claude(&self) -> Option<String> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let out = std::process::Command::new("security")
            .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Falha de transporte (sem resposta HTTP).
#[derive(Debug, Error)]
pub enum HttpError {
    #[error("falha de transporte: {0}")]
    Transport(String),
}

/// GET injetável: devolve `(status, corpo)`.
pub trait HttpClient {
    fn get(&self, url: &str, headers: &[(String, String)], timeout_s: u64) -> Result<(u16, String), HttpError>;
    /// POST com corpo; implementações mínimas podem recusar com erro de transporte.
    fn post(
        &self,
        _url: &str,
        _headers: &[(String, String)],
        _body: &str,
        _timeout_s: u64,
    ) -> Result<(u16, String), HttpError> {
        Err(HttpError::Transport("post não suportado".into()))
    }
}

/// Implementação com `ureq`.
#[derive(Debug, Default, Clone, Copy)]
pub struct UreqClient;

impl HttpClient for UreqClient {
    fn get(&self, url: &str, headers: &[(String, String)], timeout_s: u64) -> Result<(u16, String), HttpError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(timeout_s)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut req = agent.get(url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        // a mensagem do ureq não inclui valores de header
        let mut resp = req.call().map_err(|e| HttpError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        Ok((status, body))
    }

    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
        timeout_s: u64,
    ) -> Result<(u16, String), HttpError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(timeout_s)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut req = agent.post(url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let mut resp = req.send(body).map_err(|e| HttpError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        Ok((status, text))
    }
}
