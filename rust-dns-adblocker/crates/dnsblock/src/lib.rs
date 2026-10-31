//! `dnsblock` – DNS-Sinkhole-Daemon des Rust-DNS-Adblockers.
//!
//! Läuft als eigenständiger DNS-Server im Netzwerk, prüft jede Anfrage
//! gegen die Filter-Engine und leitet Erlaubtes an den Upstream
//! (Standard: lokales Unbound, rekursiv + DNSSEC; optional DoH) weiter.
//! REST-API und Web-UI laufen im selben Prozess.

pub mod config;
pub mod dns;
pub mod net;
pub mod resolver;
pub mod server;
pub mod state;
pub mod updater;
pub mod uplink;
pub mod web;

pub use filter_engine::FilterEngine;

use std::sync::Arc;

/// Geladene Engine inkl. Ladestatistik (lokale Dateien, Legacy-API).
pub struct LoadedEngine {
    pub engine: Arc<FilterEngine>,
    pub stats: filter_engine::LoadStats,
}

/// Lädt die Filter-Engine aus allen in der Konfiguration gelisteten Dateien.
/// (Für Produktionsszenarien mit Remote-Listen: `updater::rebuild`.)
pub fn load_engine(cfg: &config::ResolvedConfig) -> std::io::Result<LoadedEngine> {
    let mut engine = FilterEngine::new();
    let mut stats = filter_engine::LoadStats::default();
    for path in &cfg.blocklists {
        let content = std::fs::read_to_string(path)?;
        let s = engine.load_str(&content);
        tracing::info!(list = %path.display(), blocked = s.blocked_added, skipped = s.skipped, "Blockliste geladen");
        stats.blocked_added += s.blocked_added;
        stats.skipped += s.skipped;
    }
    for path in &cfg.allowlists {
        let content = std::fs::read_to_string(path)?;
        let s = engine.load_allowlist_str(&content);
        tracing::info!(list = %path.display(), allowed = s.allowed_added, "Allowliste geladen");
        stats.allowed_added += s.allowed_added;
    }
    Ok(LoadedEngine {
        engine: Arc::new(engine),
        stats,
    })
}
