//! Web-UI + REST-API (Anforderung 2, 5-8): Dashboard, Werbetestseite,
//! Netzwerk-/Router-Infos, Uplink-Test und Listen-Übersicht – im Stil von
//! Pi-hole (Live-Zähler + Query-Log) und AdGuard Home (Listen-Verwaltung,
//! Upstream-Diagnostik). Bündelung ohne externe Assets (kein CDN-Leak).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Query as AxQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
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
    /// Marker-Datei (in `data_dir`): existiert = Setup einmal abgeschlossen.
    pub done_path: PathBuf,
    /// Runtime-Fenster des Flags (Marker wird beim Start eingelesen).
    pub setup_done: Arc<AtomicBool>,
}

/// (Flag, Marker-Pfad) für „Setup einmal abgeschlossen" aus dem `data_dir`.
///
/// Erststart-Erlebnis: solange das Flag `false` ist, landet `/` direkt auf
/// `/setup`; nach dem ersten erfolgreichen Setup-Submit bleibt Dashboard.
pub fn setup_done_paths(data_dir: &Path) -> (Arc<AtomicBool>, PathBuf) {
    let done = data_dir.join("setup_done.flag");
    let done_exists = done.is_file();
    (
        Arc::new(AtomicBool::new(done_exists)),
        done,
    )
}

/// Nach erfolgreichem Setup-Submit aufrufen: Flag setzen + Marker legen
/// (Best-Effort; ohne Persist bleibt das Flag nur für diese Laufzeit).
pub fn mark_setup_done(handles: &SetupHandles) {
    handles.setup_done.store(true, Ordering::Relaxed);
    if let Err(e) = std::fs::write(&handles.done_path, b"") {
        tracing::warn!(
            error = %e,
            path = %handles.done_path.display(),
            "Setup-Marker konnte nicht geschrieben werden"
        );
    } else {
        tracing::info!(path = %handles.done_path.display(), "Setup abgeschlossen – Dashboard unter /");
    }
}

/// Eintrag im System-Log (Startup/Reload/Aktionen) – RAM-only, wie das Query-Log.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SystemLogEntry {
    /// Unix-Timestamp in Millisekunden.
    pub ts: u64,
    /// Level: "info" | "warn" | "error".
    pub level: &'static str,
    /// Loginhalt.
    pub msg: String,
}

impl AppState {
    /// Schreibt einen System-Log-Eintrag (RAM-only, begrenzt, wie Stats::log).
    pub fn sys_log(&self, level: &'static str, msg: impl Into<String>) {
        let mut log = self.system_log.lock().unwrap();
        if log.len() >= 200 {
            log.remove(0);
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        log.push(SystemLogEntry {
            ts,
            level,
            msg: msg.into(),
        });
    }

    /// Snapshot des System-Logs (älteste zuerst).
    pub fn sys_log_snapshot(&self) -> Vec<SystemLogEntry> {
        self.system_log.lock().unwrap().clone()
    }
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
    pub system_log: Arc<Mutex<Vec<SystemLogEntry>>>,
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
        .route("/blocklists", get(blocklists_page))
        .route("/yt-addon", get(yt_addon_page))
        .route("/systemlog", get(systemlog_page))
        .route("/blocklog", get(blocklog_page))
        .route("/api/stats", get(api_stats))
        .route("/api/lists", get(api_lists))
        .route("/api/network", get(api_network))
        .route("/api/uplink", get(api_uplink_run))
        .route("/api/uplink/last", get(api_uplink_last))
        .route("/api/probe", get(api_probe))
        .route("/api/setup", get(api_setup_get).post(api_setup_post))
        .route("/api/blocklists", get(api_blocklists))
        .route("/api/blocklists/add", post(api_blocklists_add))
        .route("/api/blocklists/remove", post(api_blocklists_remove))
        .route("/api/blocklists/reload", post(api_blocklists_reload))
        .route("/api/blocklog", get(api_blocklog))
        .route("/api/logs", get(api_logs))
        .with_state(state)
}

/// Läuft die Web-UI bis zum Prozessende; Fehler (Port belegt) werden geloggt.
pub async fn serve(state: AppState, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(url = %format!("http://{addr}/"), "Web-UI bereit");
    axum::serve(listener, router(state)).await
}

// ---------- HTML-Seiten ----------

async fn dashboard(State(state): State<AppState>) -> Response {
    // Erststart-Erlebnis: Setup nie abgeschlossen → direkt zum Setup-Portal.
    if !state.setup.setup_done.load(Ordering::Relaxed) {
        tracing::info!("Erster Start (Setup offen) – / verweist auf /setup");
        return Redirect::temporary("/setup").into_response();
    }
    Html(include_str!("../assets/dashboard.html")).into_response()
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

async fn blocklists_page() -> Response {
    Html(include_str!("../assets/blocklists.html")).into_response()
}

async fn yt_addon_page() -> Response {
    Html(include_str!("../assets/yt_addon.html")).into_response()
}

async fn systemlog_page() -> Response {
    Html(include_str!("../assets/systemlog.html")).into_response()
}

async fn blocklog_page() -> Response {
    Html(include_str!("../assets/blocklog.html")).into_response()
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
    let log: Vec<serde_json::Value> = query_log_json(&state.stats);
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

/// Query-Log → JSON-Array (für /api/stats und /api/blocklog identisch).
fn query_log_json(stats: &Stats) -> Vec<serde_json::Value> {
    stats
        .log_snapshot()
        .into_iter()
        .map(|e| serde_json::json!({ "ts": e.ts, "qname": e.qname, "action": e.action.as_str() }))
        .collect()
}

// ---------- Blocklisten: Übersicht + Custom-Add/Remove/Reload ----------

/// Merge: Setup-Auswahl (remote_lists.json bzw. Konfig-Default) + Custom-Listen.
fn merged_remote_urls(state: &AppState) -> Vec<String> {
    let mut urls = updater::active_selection(&state.setup.base_cfg);
    for u in updater::read_custom_lists(&state.setup.base_cfg.data_dir) {
        if !urls.iter().any(|x| x == &u) {
            urls.push(u);
        }
    }
    urls
}

async fn api_blocklists(State(state): State<AppState>) -> Response {
    let lists = state.lists.lock().unwrap().clone();
    let selection = updater::active_selection(&state.setup.base_cfg);
    let catalog: Vec<CatalogJson> = CATALOG
        .iter()
        .map(|c| CatalogJson {
            name: c.name.to_string(),
            desc: c.desc.to_string(),
            url: c.url.to_string(),
            selected: selection.iter().any(|u| u == c.url),
        })
        .collect();
    let custom = updater::read_custom_lists(&state.setup.base_cfg.data_dir);
    axum::Json(serde_json::json!({
        "lists": lists.iter().map(|l| serde_json::json!({
            "name": l.name, "kind": l.kind, "domains": l.domains,
        })).collect::<Vec<_>>(),
        "catalog": catalog,
        "custom": custom,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct BlocklistUrlPayload {
    url: String,
}

fn valid_remote_url(u: &str) -> bool {
    let u = u.trim();
    (u.starts_with("https://") || u.starts_with("http://"))
        && u.len() <= 400
        && !u.contains(char::is_whitespace)
}

/// Führt Sync + Engine-Reload für die aktiven Listen aus (Custom-Änderungen).
async fn reload_engine_state(state: &AppState) -> (usize, usize, String) {
    let urls = merged_remote_urls(state);
    let (ok, bad) = updater::sync_remote_lists_for(&state.setup.base_cfg, &urls).await;
    let mut last_err = String::new();
    match updater::rebuild_with(&state.setup.base_cfg, &urls) {
        Ok(fresh) => {
            updater::swap_engine(state.engine.clone(), &state.lists, fresh).await;
        }
        Err(e) => {
            last_err = e.to_string();
        }
    }
    (ok, bad, last_err)
}

async fn api_blocklists_add(
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<BlocklistUrlPayload>,
) -> Response {
    let url = payload.url.trim().to_string();
    if !valid_remote_url(&url) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": "unzulässige URL" })),
        )
            .into_response();
    }
    let mut custom = updater::read_custom_lists(&state.setup.base_cfg.data_dir);
    if !custom.iter().any(|u| u == &url) {
        custom.push(url.clone());
        if let Err(e) = updater::write_custom_lists(&state.setup.base_cfg.data_dir, &custom) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({ "error": format!("Persist fehlgeschlagen: {e}") })),
            )
                .into_response();
        }
    }
    state.sys_log("info", format!("Blockliste hinzugefügt: {url}"));
    let (ok, bad, err) = reload_engine_state(&state).await;
    if !err.is_empty() {
        state.sys_log("warn", format!("Engine-Reload fehlgeschlagen: {err}"));
    }
    axum::Json(serde_json::json!({
        "ok": err.is_empty(),
        "added": url,
        "synced": ok,
        "failed": bad,
    }))
    .into_response()
}

async fn api_blocklists_remove(
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<BlocklistUrlPayload>,
) -> Response {
    let url = payload.url.trim().to_string();
    let mut custom = updater::read_custom_lists(&state.setup.base_cfg.data_dir);
    let before = custom.len();
    custom.retain(|u| u != &url);
    if custom.len() == before {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({ "error": "Blockliste nicht gefunden" })),
        )
            .into_response();
    }
    if let Err(e) = updater::write_custom_lists(&state.setup.base_cfg.data_dir, &custom) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({ "error": format!("Persist fehlgeschlagen: {e}") })),
        )
            .into_response();
    }
    state.sys_log("info", format!("Blockliste entfernt: {url}"));
    let (ok, bad, err) = reload_engine_state(&state).await;
    if !err.is_empty() {
        state.sys_log("warn", format!("Engine-Reload fehlgeschlagen: {err}"));
    }
    axum::Json(serde_json::json!({ "ok": true, "removed": url, "synced": ok, "failed": bad }))
        .into_response()
}

async fn api_blocklists_reload(State(state): State<AppState>) -> Response {
    let (ok, bad, err) = reload_engine_state(&state).await;
    state.sys_log("info", format!("Engine-Reload ausgelöst (sync ok={ok} failed={bad})"));
    axum::Json(serde_json::json!({
        "ok": err.is_empty(),
        "synced": ok,
        "failed": bad,
        "error": err,
    }))
    .into_response()
}

async fn api_blocklog(State(state): State<AppState>) -> Response {
    axum::Json(serde_json::json!({ "log": query_log_json(&state.stats) })).into_response()
}

async fn api_logs(State(state): State<AppState>) -> Response {
    let entries = state.sys_log_snapshot();
    axum::Json(serde_json::json!({ "log": entries })).into_response()
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

/// Validiert eine Test-Domain (kleingeschrieben, ohne Trailing-Dot, max 253).
pub fn is_valid_domain(s: &str) -> bool {
    let domain = s.trim().trim_end_matches('.').to_ascii_lowercase();
    !domain.is_empty()
        && domain.len() <= 253
        && domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

/// Direkt-Abfrage der Filter-Engine für eine Domain (Testseite).
async fn api_probe(
    State(state): State<AppState>,
    AxQuery(params): AxQuery<ProbeParams>,
) -> Response {
    let domain = params.d.trim().trim_end_matches('.').to_ascii_lowercase();
    let valid = is_valid_domain(&params.d);
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
    // Setup gilt ab jetzt als abgeschlossen: `/` zeigt ab hier das Dashboard.
    mark_setup_done(&state.setup);
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

#[cfg(test)]
mod tests {
    use super::*;
    use filter_engine::FilterEngine;

    fn test_state(data_dir: &Path) -> (AppState, Arc<SetupHandles>) {
        let (setup_done, done_path) = setup_done_paths(data_dir);
        let handles = Arc::new(SetupHandles {
            engine: Arc::new(RwLock::new(Arc::new(FilterEngine::new()))),
            lists: Arc::new(Mutex::new(Vec::new())),
            base_cfg: crate::config::Config::parse_str(
                "listen_udp = \"127.0.0.1:0\"\nlisten_tcp = \"127.0.0.1:0\"\nupstream = \"127.0.0.1:53\"\n",
            )
            .unwrap()
            .resolve()
            .unwrap(),
            selection_path: data_dir.join("remote_lists.json"),
            done_path,
            setup_done,
        });
        let state = AppState {
            engine: handles.engine.clone(),
            stats: Arc::new(Stats::default()),
            lists: handles.lists.clone(),
            net: Arc::new(NetworkInfo::default()),
            uplink_last: Arc::new(RwLock::new(Vec::new())),
            uplink_servers: Arc::new(Vec::new()),
            block_mode: "zero",
            upstream_desc: "Test".into(),
            setup: handles.clone(),
            system_log: Arc::new(Mutex::new(Vec::new())),
        };
        (state, handles)
    }

    /// Erststart-Logik: solange kein Setup gespeichert wurde, verweist `/`
    /// auf `/setup`; nach `mark_setup_done` (und nach „Neustart" über den
    /// Marker) zeigt `/` das Dashboard.
    #[tokio::test]
    async fn dashboard_redirects_to_setup_until_saved() {
        let dir = std::env::temp_dir().join(format!(
            "dnsblock-webtest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let (state, handles) = test_state(&dir);
        let app = router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let base = format!("http://{addr}");

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        // 1) Erststart: / verweist auf /setup.
        let resp = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(resp.status(), 307, "erster Start: / sollte weiterleiten");
        assert_eq!(resp.headers().get("Location").unwrap(), "/setup");
        assert!(!resp.url().as_str().ends_with("/setup"));

        // /setup selbst bleibt normal erreichbar:
        let resp = client.get(format!("{base}/setup")).send().await.unwrap();
        assert_eq!(resp.status(), 200);

        // 2) Setup-Submit: Flag + Marker.
        assert!(!handles.setup_done.load(Ordering::Relaxed));
        mark_setup_done(&handles);
        assert!(handles.setup_done.load(Ordering::Relaxed));
        assert!(handles.done_path.is_file(), "Marker fehlt");

        // Ab jetzt zeigt / das Dashboard:
        let resp = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(resp.status(), 200, "nach Setup: / ist Dashboard");
        assert!(resp
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.starts_with("text/html"))
            .unwrap_or(false));

        // 3) „Neustart": Flag wird aus dem Marker wiederhergestellt.
        let (restarted, _) = setup_done_paths(&dir);
        assert!(restarted.load(Ordering::Relaxed), "Marker muss persistieren");

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_valid_domain_checks_format() {
        assert!(is_valid_domain("doubleclick.net"));
        assert!(is_valid_domain("  ADS.Example.COM. "));
        assert!(!is_valid_domain(""));
        assert!(!is_valid_domain(".."));
        // '-'/'_' sind wie bisher erlaubt (Chars alnum, '.', '-', '_').
        assert!(is_valid_domain("-bad-.example"));
        assert!(!is_valid_domain("http://evil.example.com"));
        assert!(!is_valid_domain(&"a".repeat(254)));
    }
}
