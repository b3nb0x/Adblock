//! Web-UI + REST-API (Anforderung 2, 5-8): Dashboard, Werbetestseite,
//! Netzwerk-/Router-Infos, Uplink-Test und Listen-Übersicht – im Stil von
//! Pi-hole (Live-Zähler + Query-Log) und AdGuard Home (Listen-Verwaltung,
//! Upstream-Diagnostik). Bündelung ohne externe Assets (kein CDN-Leak).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{Query as AxQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::config::{BlockMode, ResolvedConfig};
use crate::net::NetworkInfo;
use crate::state::{ListInfo, SharedEngine, Stats, UplinkTestResult};
use crate::updater;
use crate::uplink;

/// Kuratierter Katalog bekannt guter DNS-Blocklisten (Panel-Auswahl).
/// Nur handgeprüfte, freie Quellen; alle im hosts/Domänen/AdGuard-Format,
/// das die Engine parst. Erweiterbar ohne Daemon-Update (eigene URLs).
pub struct CatalogEntry {
    pub name: &'static str,
    pub desc: &'static str,
    pub url: &'static str,
}

pub static CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        name: "AdGuard DNS Filter",
        desc: "Baseliste: Ads + Tracking, https-Quelle der HostlistsRegistry (~70k Regeln)",
        url: "https://adguardteam.github.io/HostlistsRegistry/assets/filter_1.txt",
    },
    CatalogEntry {
        name: "AdAway Default Blocklist",
        desc: "Mobile-/hosts-orientierte Baseliste der HostlistsRegistry (~40k)",
        url: "https://adguardteam.github.io/HostlistsRegistry/assets/filter_2.txt",
    },
    CatalogEntry {
        name: "Peter Lowe Blocklist",
        desc: "Kompakte, altehrwürdige Ad-/Tracker-Liste (HostlistsRegistry)",
        url: "https://adguardteam.github.io/HostlistsRegistry/assets/filter_3.txt",
    },
    CatalogEntry {
        name: "Dan Pollock Hosts",
        desc: "Breite hosts-Liste (HostlistsRegistry)",
        url: "https://adguardteam.github.io/HostlistsRegistry/assets/filter_4.txt",
    },
    CatalogEntry {
        name: "StevenBlack Unified Hosts",
        desc: "Klassiker: Ads + Malware, Unified-Format (~90k)",
        url: "https://raw.githubusercontent.com/StevenBlack/hosts/master/hosts",
    },
    CatalogEntry {
        name: "StevenBlack + Fakenews",
        desc: "StevenBlack mit Fakenews-Erweiterung",
        url: "https://raw.githubusercontent.com/StevenBlack/hosts/master/alternates/fakenews/hosts",
    },
];

/// Vom Web-UI getrieben hardwire Setup: Auswahl-Datei + Rebuild-Quellen.
#[derive(Debug, Clone)]
pub struct SetupHandles {
    pub engine: SharedEngine,
    pub lists: Arc<Mutex<Vec<ListInfo>>>,
    pub base_cfg: ResolvedConfig,
    pub selection_path: PathBuf,
}

/// Der geteilte Zustand der Web-UI (parallel zum DNS-Server).
#[derive(Debug, Clone)]
pub struct AppState {
    pub engine: SharedEngine,
    pub stats: Arc<Stats>,
    pub lists: Arc<Mutex<Vec<ListInfo>>>,
    pub net: Arc<NetworkInfo>,
    pub uplink_last: Arc<RwLock<Vec<UplinkTestResult>>>,
    pub uplink_servers: Arc<Vec<SocketAddr>>,
    pub block_mode: &'static str,
    pub upstream_desc: String,
    pub setup: Arc<SetupHandles>,
}

pub fn block_mode_name(mode: BlockMode) -> &'static str {
    match mode {
        BlockMode::Zero => "zero (0.0.0.0, Pi-hole-artig)",
        BlockMode::NxDomain => "nxdomain (AdGuard-artig)",
        BlockMode::Refused => "refused",
    }
}

/// Router mit allen Routen + State.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/test", get(test_page))
        .route("/setup", get(setup_page))
        .route("/userscript.js", get(userscript))
        .route("/api/stats", get(api_stats))
        .route("/api/lists", get(api_lists))
        .route("/api/network", get(api_network))
        .route("/api/uplink", get(api_uplink_run))
        .route("/api/uplink/last", get(api_uplink_last))
        .route("/api/probe", get(api_probe))
        .route("/api/setup", get(api_setup_get).post(api_setup_post))
        .with_state(state)
}

/// Läuft die Web-UI bis zum Prozessende; Fehler (Port belegt) werden geloggt.
pub async fn serve(state: AppState, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(url = %format!("http://{addr}/"), "Web-UI bereit");
    axum::serve(listener, router(state)).await
}

// ---------- HTML-Seiten ----------

async fn dashboard() -> Html<&'static str> {
    Html(include_str!("../assets/dashboard.html"))
}

async fn test_page() -> Html<&'static str> {
    Html(include_str!("../assets/test.html"))
}

async fn userscript() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../assets/yt-adskip.user.js"),
    )
        .into_response()
}

// ---------- API ----------

async fn api_stats(State(state): State<AppState>) -> Response {
    let (total, blocked, forwarded, failed) = (
        state.stats.total.load(std::sync::atomic::Ordering::Relaxed),
        state
            .stats
            .blocked
            .load(std::sync::atomic::Ordering::Relaxed),
        state
            .stats
            .forwarded
            .load(std::sync::atomic::Ordering::Relaxed),
        state
            .stats
            .failed
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    let engine = state.engine.read().await.clone();
    let log: Vec<serde_json::Value> = state
        .stats
        .log_snapshot()
        .into_iter()
        .map(|e| serde_json::json!({ "ts": e.ts, "qname": e.qname, "action": e.action.as_str() }))
        .collect();
    axum::Json(serde_json::json!({
        "total": total,
        "blocked": blocked,
        "forwarded": forwarded,
        "failed": failed,
        "blocked_percent": if total > 0 { blocked as f64 / total as f64 * 100.0 } else { 0.0 },
        "engine_domains": engine.blocked_count(),
        "block_mode": state.block_mode,
        "upstream": state.upstream_desc,
        "log": log,
    }))
    .into_response()
}

async fn api_lists(State(state): State<AppState>) -> Response {
    let lists = state.lists.lock().unwrap().clone();
    axum::Json(serde_json::json!({
        "lists": lists.iter().map(|l| serde_json::json!({
            "name": l.name, "kind": l.kind, "domains": l.domains,
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

async fn api_network(State(state): State<AppState>) -> Response {
    let net = state.net.clone();
    let suggestion = net.suggestion();
    axum::Json(serde_json::json!({
        "local_ip": net.local_ip.map(|i| i.to_string()),
        "netmask": net.netmask.map(|i| i.to_string()),
        "iface": net.iface,
        "gateway": net.gateway.map(|i| i.to_string()),
        "gateway_mac": net.gateway_mac,
        "vendor": net.vendor,
        "suggestion": suggestion.map(|s| serde_json::json!({
            "vendor": s.vendor,
            "router_ip": s.router_ip,
            "dns_to_set": s.dns_to_set,
            "path": s.path,
            "note": s.note,
        })),
    }))
    .into_response()
}

/// Startet einen frischen Uplink-Test (Konkurrierend, max ~3 s).
async fn api_uplink_run(State(state): State<AppState>) -> Response {
    let results = uplink::test_many(&state.uplink_servers).await;
    *state.uplink_last.write().await = results.clone();
    axum::Json(serde_json::json!({ "results": results })).into_response()
}

async fn api_uplink_last(State(state): State<AppState>) -> Response {
    let results = state.uplink_last.read().await.clone();
    axum::Json(serde_json::json!({ "results": results })).into_response()
}

#[derive(Debug, Deserialize)]
struct ProbeParams {
    d: String,
}

/// Direkt-Abfrage der Filter-Engine für eine Domain (Testseite).
async fn api_probe(
    State(state): State<AppState>,
    AxQuery(params): AxQuery<ProbeParams>,
) -> Response {
    let domain = params.d.trim().trim_end_matches('.').to_ascii_lowercase();
    let valid = !domain.is_empty()
        && domain.len() <= 253
        && domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_');
    if !valid {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": "unzulässige Domain" })),
        )
            .into_response();
    }
    let engine = state.engine.read().await.clone();
    let blocked = engine.is_blocked(&domain);
    axum::Json(serde_json::json!({ "domain": domain, "blocked": blocked })).into_response()
}

// ---------- Setup: Filterlisten-Auswahl (Dashboard-Init) ----------

async fn setup_page() -> Html<&'static str> {
    Html(include_str!("../assets/setup.html"))
}

#[derive(Debug, Deserialize)]
struct SetupPayload {
    urls: Vec<String>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct CatalogJson {
    name: String,
    desc: String,
    url: String,
    selected: bool,
}

fn current_selection(state: &AppState) -> Vec<String> {
    updater::read_selection(&state.setup.selection_path)
        .map(|s| s.urls)
        .unwrap_or_else(|| state.setup.base_cfg.remote_blocklists.clone())
}

async fn api_setup_get(State(state): State<AppState>) -> Response {
    let selected = current_selection(&state);
    let catalog: Vec<CatalogJson> = CATALOG
        .iter()
        .map(|c| CatalogJson {
            name: c.name.to_string(),
            desc: c.desc.to_string(),
            url: c.url.to_string(),
            selected: selected.iter().any(|u| u == c.url),
        })
        .collect();
    let engine = state.engine.read().await.clone();
    axum::Json(serde_json::json!({
        "selected": selected,
        "catalog": catalog,
        "engine_domains": engine.blocked_count(),
        "fetch_remote": state.setup.base_cfg.fetch_remote,
    }))
    .into_response()
}

/// Validiert URLs hart: HTTPS/HTTP, Länge, Anzahl.
fn validate_urls(urls: &[String]) -> Result<(), &'static str> {
    if urls.len() > 20 {
        return Err("maximal 20 Listen");
    }
    for u in urls {
        let ok_prefix = u.starts_with("https://") || u.starts_with("http://");
        if !ok_prefix || u.len() > 400 || u.contains(char::is_whitespace) {
            return Err("unzulässige URL");
        }
    }
    Ok(())
}

async fn api_setup_post(
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<SetupPayload>,
) -> Response {
    if let Err(msg) = validate_urls(&payload.urls) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": msg })),
        )
            .into_response();
    }
    if let Err(e) = updater::write_selection(&state.setup.selection_path, &payload.urls) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({ "error": format!("Persist fehlgeschlagen: {e}") })),
        )
            .into_response();
    }
    // Sofort anwenden (Fetch + Rebuild + Hot-Swap) im Hintergrund; der
    // periodische Supervisor übernimmt die Auswahl spätestens am Zyklus.
    let handles = state.setup.clone();
    let urls = payload.urls.clone();
    tokio::spawn(async move {
        updater::sync_remote_lists_for(&handles.base_cfg, &urls).await;
        match updater::rebuild_with(&handles.base_cfg, &urls) {
            Ok(fresh) => updater::swap_engine(handles.engine.clone(), &handles.lists, fresh).await,
            Err(e) => tracing::warn!(error = %e, "Setup-Rebuild fehlgeschlagen"),
        }
    });
    axum::Json(serde_json::json!({
        "ok": true,
        "selected": payload.urls,
        "hint": "Anwendung läuft im Hintergrund; /api/stats (engine_domains) zeigt die neue Anzahl."
    }))
    .into_response()
}
