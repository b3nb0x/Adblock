//! Listen-Update: HTTPS-Fetch (reqwest + rustls), FNV-Caching,
//! Engine-Neuaufbau und atomarer Hot-Reload (Anforderung 1 + Phase 2).
//!
//! Doppel-Strategie wie bei Pi-hole/AdGuard Home:
//!
//! - **Lokale Listen** (Repo/`config/`) werden sofort geladen.
//! - **Remote-Listen** werden beim Start und alle `remote_refresh_hours`
//!   Stunden per HTTPS gezogen und **atomar** in
//!   `data_dir/remote/<hash>.txt` geschrieben (ein Crash mid-write
//!   zerstört den bestehenden Cache nicht). Danach wird die Engine
//!   komplett neu aufgebaut und gegen den laufenden Server getauscht.
//! - Bei Fetch-Fehlern bleibt der zuletzt geladene Cache aktiv (fail-closed).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use filter_engine::FilterEngine;
use tokio::sync::RwLock;

use crate::config::ResolvedConfig;
use crate::state::{ListInfo, SharedEngine};

/// Offizielle Quelle der Root-Hints (Unbound-Sidecar, Phase 3).
pub const ROOT_HINTS_URL: &str = "https://www.internic.net/domain/named.cache";

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = "dnsblock/0.2 (+https://github.com/b3nb0x/Adblock)";

/// FNV-1a 64-bit (kurze, deterministische Cache-File-Namen).
pub fn fnv64(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Cache-Datei-Pfad für eine Remote-URL.
pub fn cache_path(url: &str, data_dir: &Path) -> PathBuf {
    data_dir
        .join("remote")
        .join(format!("{:016x}.txt", fnv64(url)))
}

/// Schreibt Daten atomar nach `path` (temp + rename, gleiches Dateisystem).
/// Ein Absturz mid-write kann die bestehende Datei nicht zerstören.
pub fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(FETCH_TIMEOUT)
        .build()
        .expect("reqwest-Client mit rustls immer baubar")
}

/// Lädt eine Remote-Liste per HTTPS; gibt den Text oder Fehlergrund zurück.
async fn fetch_list(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.text().await.map_err(|e| e.to_string())
}

/// Pfad der vom Web-UI geschriebenen Auswahl (`remote_lists.json`).
/// Überschreibt – wenn vorhanden – `cfg.remote_blocklists`.
pub fn selection_path(data_dir: &Path) -> PathBuf {
    data_dir.join("remote_lists.json")
}

/// Format der Auswahl-Datei (Web-UI → Datei).
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListSelection {
    /// Aktivierte Remote-Listen-URLs (Reihenfolge = Lade-Reihenfolge).
    pub urls: Vec<String>,
}

/// Liest die Auswahl-Datei; fehlt/kaputt ⇒ `None` (Konfig-Default gewinnt).
pub fn read_selection(path: &Path) -> Option<ListSelection> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<ListSelection>(&text) {
        Ok(sel) => Some(sel),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "remote_lists.json unlesbar");
            None
        }
    }
}

/// Schreibt die Auswahl atomar (Web-UI POST → Daemon-Neuladen).
pub fn write_selection(path: &Path, urls: &[String]) -> io::Result<()> {
    let payload = serde_json::to_vec_pretty(&ListSelection {
        urls: urls.to_vec(),
    })?;
    atomic_write(path, &payload)
}

/// Aktive Remote-URLs: Auswahl-Datei > Konfig-Default.
pub fn active_selection(cfg: &ResolvedConfig) -> Vec<String> {
    read_selection(&selection_path(&cfg.data_dir))
        .map(|s| s.urls)
        .unwrap_or_else(|| cfg.remote_blocklists.clone())
}

/// Persistierte benutzerdefinierte Blocklisten (JSON-Array von URLs), Web-UI.
pub fn custom_lists_path(data_dir: &Path) -> PathBuf {
    data_dir.join("custom_blocklists.json")
}

/// Format der Custom-Listen-Datei (Web-UI → Daemon).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CustomListsFile {
    /// Benutzerdefinierte Remote-Blocklisten-URLs.
    pub urls: Vec<String>,
}

/// Liest die benutzerdefinierten Blocklisten; fehlt/kaputt ⇒ leer (Fail-safe).
pub fn read_custom_lists(data_dir: &Path) -> Vec<String> {
    let path = custom_lists_path(data_dir);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    match serde_json::from_str::<CustomListsFile>(&text) {
        Ok(f) => f.urls,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "custom_blocklists.json unlesbar");
            Vec::new()
        }
    }
}

/// Schreibt die benutzerdefinierten Blocklisten atomar (Web-UI → Daemon).
pub fn write_custom_lists(data_dir: &Path, urls: &[String]) -> io::Result<()> {
    let payload = serde_json::to_vec_pretty(&CustomListsFile {
        urls: urls.to_vec(),
    })?;
    atomic_write(&custom_lists_path(data_dir), &payload)
}

/// Zieht die gegebenen URLs in den Cache (fail-closed, atomar).
/// Rückgabe: (erfolgreich, fehlgeschlagen) URLs.
pub async fn sync_remote_lists_for(cfg: &ResolvedConfig, urls: &[String]) -> (usize, usize) {
    // Bewusst KEIN fetch_remote-Gate: Diese Funktion wird explizit mit der
    // aktiven Auswahl aufgerufen (Web-UI-POST, Supervisor). Das Gate für den
    // periodischen Autopiloten liegt in `sync_remote_lists`.
    if urls.is_empty() {
        return (0, 0);
    }
    let client = http_client();
    let mut ok = 0usize;
    let mut bad = 0usize;
    for url in urls {
        match fetch_list(&client, url).await {
            Ok(text) => {
                let path = cache_path(url, &cfg.data_dir);
                match atomic_write(&path, text.as_bytes()) {
                    Ok(()) => {
                        tracing::info!(url, bytes = text.len(), "Remote-Liste aktualisiert");
                        ok += 1;
                    }
                    Err(e) => {
                        tracing::warn!(url, error = %e, "Cache-Schreiben fehlgeschlagen");
                        bad += 1;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(url, error = %e, "Remote-Liste unauffindbar (Cache bleibt)");
                bad += 1;
            }
        }
    }
    (ok, bad)
}

/// Zieht alle aktiven Remote-Listen in den Cache (Auswahl-Datei > Konfig).
pub async fn sync_remote_lists(cfg: &ResolvedConfig) -> (usize, usize) {
    if !cfg.fetch_remote {
        return (0, 0);
    }
    let urls = active_selection(cfg);
    sync_remote_lists_for(cfg, &urls).await
}

/// Baut eine frische Engine aus allen Quellen: lokale Blöcke/Allows +
/// Remote-Caches. Ein fehlender Cache wird übersprungen (fail-closed).
pub fn rebuild(cfg: &ResolvedConfig) -> io::Result<(FilterEngine, Vec<ListInfo>)> {
    let remote = if cfg.fetch_remote {
        active_selection(cfg)
    } else {
        Vec::new()
    };
    rebuild_with(cfg, &remote)
}

/// Wie `rebuild`, aber die Remote-Liste ist explizit (Web-UI-Auswahl).
pub fn rebuild_with(
    cfg: &ResolvedConfig,
    remote_urls: &[String],
) -> io::Result<(FilterEngine, Vec<ListInfo>)> {
    let mut engine = FilterEngine::new();
    let mut infos = Vec::new();

    for path in &cfg.blocklists {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(list = %path.display(), error = %e, "Liste unlesbar – übersprungen");
                continue;
            }
        };
        let s = engine.load_str(&text);
        infos.push(ListInfo {
            name: path.display().to_string(),
            kind: "lokal",
            domains: s.blocked_added,
        });
    }

    for url in remote_urls {
        let path = cache_path(url, &cfg.data_dir);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(url, error = %e, "Remote-Cache fehlt – übersprungen");
                continue;
            }
        };
        let s = engine.load_str(&text);
        infos.push(ListInfo {
            name: url.clone(),
            kind: "remote",
            domains: s.blocked_added,
        });
    }

    for path in &cfg.allowlists {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(list = %path.display(), error = %e, "Allowliste unlesbar – übersprungen");
                continue;
            }
        };
        let s = engine.load_allowlist_str(&text);
        infos.push(ListInfo {
            name: path.display().to_string(),
            kind: "allow",
            domains: s.allowed_added,
        });
    }

    Ok((engine, infos))
}

/// Ersetzt die aktive Engine (Hot-Reload) und aktualisiert die Listen-Infos.
pub async fn swap_engine(
    engine: Arc<RwLock<Arc<FilterEngine>>>,
    lists_registry: &Mutex<Vec<ListInfo>>,
    fresh: (FilterEngine, Vec<ListInfo>),
) {
    let domains = fresh.0.blocked_count();
    *engine.write().await = Arc::new(fresh.0);
    *lists_registry.lock().unwrap() = fresh.1;
    tracing::info!(domains, "Filter-Engine hot-reloaded");
}

/// Dauerlauf des Updaters: initialer Fetch + periodischer Refresh + Engine-Swap.
/// Wird vom Aufrufer gespawnt; Rückgabe erfolgt nie regulär.
pub async fn supervisor(
    cfg: ResolvedConfig,
    engine: SharedEngine,
    lists_registry: Arc<Mutex<Vec<ListInfo>>>,
) {
    loop {
        // 1. Aktive Auswahl ermitteln (Datei kann zwischen den Zyklen wechseln).
        let urls = active_selection(&cfg);

        // 2. Remote-Listen nachladen (fail-closed: Cache bleibt bei Fehler).
        sync_remote_lists_for(&cfg, &urls).await;

        // 3. Engine frisch aufbauen und atomar tauschen.
        match rebuild_with(&cfg, &urls) {
            Ok(fresh) => swap_engine(engine.clone(), &lists_registry, fresh).await,
            Err(e) => {
                tracing::warn!(error = %e, "Engine-Rebuild fehlgeschlagen (alte Engine bleibt)")
            }
        }

        // 3. Schlafen bis zum nächsten Intervall (standard 12 h).
        let pause = Duration::from_secs(u64::from(cfg.remote_refresh_hours.max(1)) * 3600);
        tokio::time::sleep(pause).await;
    }
}

// Legacy-Helper aus Phase 1 (Unit-Tests + Unbound-Sidecar-Fokus).

/// Intervall des Root-Hints-Updates (Standard: 4 Tage).
pub fn root_hints_interval(days: u32) -> Duration {
    Duration::from_secs(u64::from(days) * 24 * 60 * 60)
}

/// Zeitpunkt des nächsten geplanten Updates.
pub fn next_deadline(last_update: SystemTime, days: u32) -> SystemTime {
    last_update + root_hints_interval(days)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join("dnsblock-updater-test");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("named.cache");
        std::fs::write(&target, b"alt").unwrap();
        atomic_write(&target, b"neu").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"neu");
        assert!(!dir.join("named.new").exists());
    }

    #[test]
    fn interval_is_four_days() {
        assert_eq!(
            root_hints_interval(4),
            Duration::from_secs(4 * 24 * 60 * 60)
        );
    }

    #[test]
    fn fnv64_is_deterministic_and_covers_input() {
        let a = fnv64("https://example.com/hosts.txt");
        let b = fnv64("https://example.com/hosts.txt");
        let c = fnv64("https://example.com/hosts2.txt");
        assert_eq!(a, b);
        assert_ne!(a, c);
        // Cache-Pfad leitet sich aus fnv64 ab
        let p = cache_path("https://example.com/hosts.txt", Path::new("data"));
        assert!(p.starts_with("data/remote"));
        assert_eq!(p.extension().and_then(|e| e.to_str()), Some("txt"));
    }

    #[test]
    fn rebuild_loads_local_lists_and_reports_infos() {
        let dir = std::env::temp_dir().join("dnsblock-rebuild-test");
        std::fs::create_dir_all(&dir).unwrap();
        let list = dir.join("block.txt");
        std::fs::write(&list, "||ads.example.com^\n").unwrap();
        let allow = dir.join("allow.txt");
        std::fs::write(&allow, "ok.ads.example.com\n").unwrap();

        let mut cfg = ResolvedConfig::minimal(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        );
        cfg.blocklists = vec![list.clone()];
        cfg.allowlists = vec![allow.clone()];
        cfg.remote_blocklists = vec!["http://localhost/nonexistent-url".into()];
        // fetch_remote false => Remote-Sektion übersprungen

        let (engine, infos) = rebuild(&cfg).unwrap();
        assert!(engine.is_blocked("ads.example.com"));
        // Allowlist gewinnt gegen Block-Suffix:
        assert!(!engine.is_blocked("ok.ads.example.com"));
        assert!(infos.len() == 2, "lokal + allow erwartet: {infos:?}");
        assert_eq!(infos[0].kind, "lokal");
        assert_eq!(infos[1].kind, "allow");
    }

    #[test]
    fn selection_roundtrip_and_override() {
        let dir = std::env::temp_dir().join("dnsblock-selection-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("remote_lists.json");

        // Erst keine Datei → None
        assert!(read_selection(&path).is_none());

        // Schreiben + Lesen
        let urls = vec![
            "https://example.com/a.txt".into(),
            "https://example.com/b.txt".into(),
        ];
        write_selection(&path, &urls).unwrap();
        let sel = read_selection(&path).unwrap();
        assert_eq!(sel.urls, urls);

        // Kaputte Datei → None (Konfig-Default gewinnt)
        std::fs::write(&path, "kein json").unwrap();
        assert!(read_selection(&path).is_none());

        // active_selection: Datei schlägt Konfig
        let mut cfg = ResolvedConfig::minimal(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        );
        cfg.data_dir = dir.clone();
        cfg.remote_blocklists = vec!["https://default.example/list.txt".into()];
        write_selection(&path, &urls).unwrap();
        assert_eq!(active_selection(&cfg), urls);

        std::fs::write(&path, "immer noch kaputt: {").unwrap();
        assert_eq!(active_selection(&cfg), cfg.remote_blocklists);
    }
}
