//! `dnsblock` – Startpunkt des DNS-Sinkhole-Daemons.
//!
//! Ablauf (Anforderungen 1–8): Netzerk-/Router-Erkennung (5, 6) →
//! Engine initial laden (1, 4) → Uplink-Test (7) → DNS-Server (1) →
//! Update-Supervisor mit Hot-Reload (1, 2) → Web-UI/REST (2, 5–8).

use std::sync::Arc;
use std::time::Duration;

use dnsblock::config::Config;
use dnsblock::net;
use dnsblock::state::{shared_engine, ListInfo};
use dnsblock::{server, updater, uplink, web};
use tokio::sync::RwLock;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/default.toml".to_string());

    let raw = match Config::from_path(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(config = %cfg_path, error = %e, "Konfiguration ungültig");
            std::process::exit(1);
        }
    };
    let cfg = match raw.resolve() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "Adressen in der Konfiguration ungültig");
            std::process::exit(1);
        }
    };

    // ---------- Netzwerk- und Router-Erkennung (Anforderung 5 + 6) ----------
    let net = net::detect();
    if let Some(s) = net.suggestion() {
        tracing::info!(
            router = s.router_ip,
            dns_target = s.dns_to_set,
            menu = s.path,
            "Router-Vorschlag: '{}' → im Router-Menü '{}' den DNS auf {} setzen ({}).",
            s.vendor,
            s.path,
            s.dns_to_set,
            s.note
        );
    }

    // ---------- Filter-Engine initial (Anforderung 1 + 4) ----------
    let (engine, lists) = match updater::rebuild(&cfg) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "Blocklisten konnten nicht geladen werden");
            std::process::exit(1);
        }
    };
    tracing::info!(domains = engine.blocked_count(), "Filter-Engine bereit");
    let engine = shared_engine(Arc::new(engine));
    let lists_registry = Arc::new(std::sync::Mutex::new(lists));

    // ---------- DNS-Uplink-Test (Anforderung 7) ----------
    // Eigener Upstream vorne dran, sonst die konfigurierten Test-Server.
    let mut test_servers = Vec::with_capacity(cfg.uplink_test_servers.len() + 1);
    test_servers.push(cfg.upstream);
    for s in &cfg.uplink_test_servers {
        if *s != cfg.upstream {
            test_servers.push(*s);
        }
    }
    let uplink_results = uplink::test_many(&test_servers).await;
    for r in &uplink_results {
        tracing::info!("{}", uplink::format_result(r));
    }
    let uplink_state = Arc::new(RwLock::new(uplink_results));

    // ---------- DNS-Server (UDP/TCP, Hot-Reload-fähig) ----------
    let handle = match server::start(engine.clone(), cfg.clone()).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "Server konnte nicht starten (Port belegt?)");
            std::process::exit(1);
        }
    };
    tracing::info!(udp = %handle.udp, tcp = %handle.tcp, "dnsblock lauscht");

    // ---------- Update-Supervisor (Remote-Listen + Hot-Reload) ----------
    if cfg.fetch_remote && !cfg.remote_blocklists.is_empty() {
        tokio::spawn(updater::supervisor(
            cfg.clone(),
            engine.clone(),
            Arc::clone(&lists_registry),
        ));
        tracing::info!(
            lists = cfg.remote_blocklists.len(),
            hours = cfg.remote_refresh_hours,
            "Remote-Listen-Update aktiv (Start + Zyklus)"
        );
    } else {
        tracing::info!("Remote-Listen-Update aus (fetch_remote/remove_blocklists leer)");
    }

    if cfg.root_hints_update_days > 0 {
        tracing::info!(
            days = cfg.root_hints_update_days,
            url = updater::ROOT_HINTS_URL,
            "Root-Hints-Zyklus konfiguriert (Fetch folgt mit Unbound-Sidecar)"
        );
    }

    // ---------- Web-UI / REST (Anforderung 2, 4–8) ----------
    let upstream_desc = match &cfg.upstream_doh {
        Some(url) => format!("{} + DoH {}", cfg.upstream, url),
        None => format!("{} (UDP/TCP)", cfg.upstream),
    };
    // Setup-Marker: erster Start → `/` verweist auf `/setup`,
    // nach dem ersten gespeicherten Setup zeigt `/` das Dashboard.
    let (setup_done, setup_done_path) = web::setup_done_paths(&cfg.data_dir);

    let app_state = web::AppState {
        engine: engine.clone(),
        stats: handle.stats.clone(),
        lists: lists_registry.clone(),
        net: Arc::new(net),
        uplink_last: Arc::clone(&uplink_state),
        uplink_servers: Arc::new(test_servers),
        block_mode: web::block_mode_name(cfg.block_mode),
        upstream_desc,
        setup: Arc::new(web::SetupHandles {
            engine: engine.clone(),
            lists: lists_registry.clone(),
            base_cfg: cfg.clone(),
            selection_path: updater::selection_path(&cfg.data_dir),
            done_path: setup_done_path,
            setup_done,
        }),
        system_log: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    app_state.sys_log("info", "Web-UI bereitgestellt");
    match cfg.admin_listen {
        Some(addr) => {
            let state_clone = app_state.clone();
            tokio::spawn(async move {
                if let Err(e) = web::serve(state_clone, addr).await {
                    tracing::error!(addr = %addr, error = %e, "Web-UI konnte nicht starten");
                }
            });
        }
        None => {
            tracing::info!("Web-UI deaktiviert (admin_listen gesetzt? Standard: 127.0.0.1:8080)")
        }
    }

    tracing::info!("Beenden mit Strg+C");
    tokio::signal::ctrl_c().await.expect("Signal-Handler");
    tracing::info!(stats = handle.stats.summary(), "Shutdown");
    handle.shutdown();
    if let Some(addr) = cfg.admin_listen {
        let _ = addr; // Web-UI lebt im Task, Prozessende beendet alles
    }
    // kurze Gnadenfrist für ausstehende Antworten
    tokio::time::sleep(Duration::from_millis(100)).await;
}

// Listen-Zähltyp wird nur für AppState gebraucht (gleicher Typ wie state.rs).
#[allow(dead_code)]
fn _type_anchor(_l: &ListInfo) {}
