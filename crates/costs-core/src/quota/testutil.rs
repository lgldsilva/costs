//! Fakes e fixture de testes (sem rede).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use super::entry::Entry;
use super::env::{Env, HttpClient, HttpError};

/// HTTP falso: rotas por prefixo de URL; registra os headers recebidos.
#[derive(Default)]
pub struct FakeHttp {
    routes: Vec<(String, u16, String)>,
    seen: RefCell<Vec<(String, String)>>,
    urls: RefCell<Vec<String>>,
}

impl FakeHttp {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn route(mut self, prefix: &str, status: u16, body: &str) -> Self {
        self.routes.push((prefix.into(), status, body.into()));
        self
    }

    pub fn header_seen(&self, k: &str, v: &str) -> bool {
        self.seen.borrow().iter().any(|(a, b)| a == k && b == v)
    }

    /// Quantas requisições foram para URLs com esse prefixo.
    pub fn calls_to(&self, prefix: &str) -> usize {
        self.urls.borrow().iter().filter(|u| u.starts_with(prefix)).count()
    }
}

impl HttpClient for FakeHttp {
    fn post(&self, url: &str, headers: &[(String, String)], _body: &str, _t: u64) -> Result<(u16, String), HttpError> {
        self.seen.borrow_mut().extend_from_slice(headers);
        self.urls.borrow_mut().push(url.into());
        self.routes
            .iter()
            .find(|r| url.starts_with(&r.0))
            .map(|r| (r.1, r.2.clone()))
            .ok_or_else(|| HttpError::Transport(format!("sem rota: {url}")))
    }

    fn get(&self, url: &str, headers: &[(String, String)], _t: u64) -> Result<(u16, String), HttpError> {
        self.seen.borrow_mut().extend_from_slice(headers);
        self.urls.borrow_mut().push(url.into());
        self.routes
            .iter()
            .find(|r| url.starts_with(&r.0))
            .map(|r| (r.1, r.2.clone()))
            .ok_or_else(|| HttpError::Transport(format!("sem rota: {url}")))
    }
}

/// Env falso com home em diretório temporário.
pub struct Fixture {
    pub now: i64,
    home: tempfile::TempDir,
    cache: tempfile::TempDir,
    vars: RefCell<HashMap<String, String>>,
    keychain_claude: RefCell<Option<String>>,
}

impl Env for Fixture {
    fn var(&self, key: &str) -> Option<String> {
        self.vars.borrow().get(key).cloned()
    }

    fn home(&self) -> Option<PathBuf> {
        Some(self.home.path().to_path_buf())
    }

    fn keychain_claude(&self) -> Option<String> {
        self.keychain_claude.borrow().clone()
    }
}

impl Fixture {
    pub fn new() -> Self {
        Self {
            now: 1_791_600_000,
            home: tempfile::tempdir().unwrap(),
            cache: tempfile::tempdir().unwrap(),
            vars: RefCell::default(),
            keychain_claude: RefCell::default(),
        }
    }

    /// Simula o item "Claude Code-credentials" do Keychain (mesmo JSON do arquivo).
    pub fn with_keychain_claude(&self, json: &str) -> &Self {
        *self.keychain_claude.borrow_mut() = Some(json.to_owned());
        self
    }

    pub fn with_env(&self, k: &str, v: &str) -> &Self {
        self.vars.borrow_mut().insert(k.into(), v.into());
        self
    }

    pub fn write_home(&self, rel: &str, content: &str) {
        let p = self.home.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    pub fn home_path(&self, rel: &str) -> PathBuf {
        self.home.path().join(rel)
    }

    pub fn run(&self, provider: &str, http: &FakeHttp) -> Entry {
        super::fetch(provider, self.now, http, self, true, self.cache.path())
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.cache.path().to_path_buf()
    }

    /// O segredo não pode aparecer na entrada nem no arquivo de cache.
    pub fn assert_no_secret(&self, e: &Entry, secret: &str) {
        assert!(!serde_json::to_string(e).unwrap().contains(secret));
        let raw = std::fs::read_to_string(self.cache.path().join(format!("{}.json", e.provider))).unwrap();
        assert!(!raw.contains(secret));
    }
}
