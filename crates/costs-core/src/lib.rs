pub mod backup;
pub mod collect;
pub mod config;
pub mod doctor;
pub mod fmt;
pub mod grok;
pub mod history;
pub mod model;
pub mod native;
pub mod pace;
pub mod prices;
pub mod pricing;
pub mod priority;
pub mod quota;
pub mod remote;
pub mod report;
pub mod repricing;
pub mod runs;
pub mod service;
pub mod source;
pub mod store;
pub mod sync;

/// Versão do crate, definida no `Cargo.toml`.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!version().is_empty());
    }
}
