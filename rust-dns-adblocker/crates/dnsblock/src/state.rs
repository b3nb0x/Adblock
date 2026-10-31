//! Laufzeit-Zustand, der zwischen Server, Updater und Web-UI geteilt wird.
//!
//! Die Filter-Engine liegt hinter `RwLock<Arc<FilterEngine>>`: Reader klauen
//! pro Anfrage nur einen `Arc`-Klon aus dem Lock, der Updater kann die
//! Engine nach einem Listen-Refresh atomar austauschen (Hot-Reload).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::RwLock;

use filter_engine::FilterEngine;

/// Geteilter Zugang zur aktiven Engine (Hot-Reload-fähig).
pub type SharedEngine = Arc<RwLock<Arc<FilterEngine>>>;

pub fn shared_engine(engine: Arc<FilterEngine>) -> SharedEngine {
    Arc::new(RwLock::new(engine))
}

/// Aktion des Daemons für einen Query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Sinkhole-Antwort für blockierte Domain.
    Blocked,
    /// Unverändert an den Upstream weitergeleitet.
    Forwarded,
    /// Upstream nicht erreichbar -> SERVFAIL.
    Failed,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Blocked => "blocked",
            Action::Forwarded => "forwarded",
            Action::Failed => "failed",
        }
    }
}

/// Ein Query-Log-Eintrag (RAM-only, wird nicht persistiert).
#[derive(Debug, Clone)]
pub struct QueryLogEntry {
    /// Unix-Timestamp in Millisekunden.
    pub ts: u64,
    /// Angefragte Domäne (kleingeschrieben, ohne trailing dot).
    pub qname: String,
    pub action: Action,
}

/// Beobachtbarer Zustand: Zähler + begrenzter Query-Log.
///
/// Das `std::sync::Mutex` schützt nur kurze, nicht-awaitende Abschnitte
/// (VecDeque push/pop) und ist damit im Tokio-Kontext ungefährlich.
#[derive(Debug, Default)]
pub struct Stats {
    pub total: AtomicU64,
    pub blocked: AtomicU64,
    pub forwarded: AtomicU64,
    pub failed: AtomicU64,
    log: Mutex<VecDeque<QueryLogEntry>>,
}

impl Stats {
    /// Maximale Anzahl behaltener Log-Einträge (auch DNS-Antwort-TTL: Debug).
    pub const LOG_CAP: usize = 500;

    pub fn log_push(&self, entry: QueryLogEntry) {
        let mut log = self.log.lock().unwrap();
        if log.len() >= Self::LOG_CAP {
            log.pop_front();
        }
        log.push_back(entry);
    }

    /// Snapshot des Logs (ältester zuerst).
    pub fn log_snapshot(&self) -> Vec<QueryLogEntry> {
        self.log.lock().unwrap().iter().cloned().collect()
    }

    /// Kurz-Zusammenfassung (Start-/Shutdown-Log).
    pub fn summary(&self) -> String {
        format!("{}", self)
    }
}

impl std::fmt::Display for Stats {
    /// Kurz-Zusammenfassung (Shutdown-Log und Startup).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (total, blocked, forwarded, failed) = (
            self.total.load(Ordering::Relaxed),
            self.blocked.load(Ordering::Relaxed),
            self.forwarded.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        );
        write!(
            f,
            "Anfragen: {total}, geblockt: {blocked}, weitergeleitet: {forwarded}, fehlgeschlagen: {failed}"
        )
    }
}

/// Info über eine geladene Liste (Dashboard /api/lists).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListInfo {
    /// Anzeigename (Datei/URL).
    pub name: String,
    /// "lokal" | "remote" | "allow".
    pub kind: &'static str,
    /// Anzahl geladener Domänen (oder Allow-Einträge).
    pub domains: usize,
}

/// Ergebnis eines DNS-Uplink-Tests.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UplinkTestResult {
    /// Getesteter Server ("1.1.1.1:53").
    pub server: String,
    /// RTT in ms; `-1` = keine Antwort.
    pub ms: i64,
    /// DNS-Statuscode des Replys ("NOERROR", "NXDOMAIN", "SERVFAIL", ...).
    pub rcode: String,
    pub ok: bool,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_keeps_newest_and_caps() {
        let stats = Stats::default();
        for i in 0..(Stats::LOG_CAP + 10) {
            stats.log_push(QueryLogEntry {
                ts: i as u64,
                qname: format!("q{i}.example.com"),
                action: Action::Blocked,
            });
        }
        let log = stats.log_snapshot();
        assert_eq!(log.len(), Stats::LOG_CAP);
        assert_eq!(log.first().unwrap().ts, 10); // älteste fielen heraus
        assert_eq!(
            log.last().unwrap().qname,
            format!("q{}.example.com", Stats::LOG_CAP + 9)
        );
    }

    #[test]
    fn summary_contains_all_counters() {
        let stats = Stats::default();
        stats.total.fetch_add(3, Ordering::Relaxed);
        let s = stats.summary();
        assert!(s.contains("Anfragen: 3"), "{s}");
    }

    #[test]
    fn action_strings() {
        assert_eq!(Action::Blocked.as_str(), "blocked");
        assert_eq!(Action::Forwarded.as_str(), "forwarded");
        assert_eq!(Action::Failed.as_str(), "failed");
    }
}
