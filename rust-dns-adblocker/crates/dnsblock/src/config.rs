//! Konfiguration (TOML) des Daemons.

use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Konfigurationsdatei konnte nicht gelesen werden: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML konnte nicht geparst werden: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("Ungültige Adresse '{addr}': {source}")]
    Addr {
        addr: String,
        source: std::net::AddrParseError,
    },
}

/// Antwort-Verhalten für blockierte Anfragen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockMode {
    /// A-Record 0.0.0.0 (Pi-hole-Verhalten), andere Typs: NODATA.
    #[default]
    Zero,
    /// NXDOMAIN (AdGuard-Home-Verhalten).
    NxDomain,
    /// REFUSED zurückmelden.
    Refused,
}

/// Standards für den DNS-Uplink-Test (Antwortfähigkeit + RTT).
fn default_uplink_test_servers() -> Vec<String> {
    vec![
        "1.1.1.1:53".into(),
        "8.8.8.8:53".into(),
        "9.9.9.9:53".into(),
    ]
}

fn default_data_dir() -> String {
    "data".into()
}

fn default_remote_refresh_hours() -> u32 {
    12
}

/// Rohe Konfiguration aus der TOML-Datei.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// UDP-Listen-Adresse, z. B. "0.0.0.0:53".
    pub listen_udp: String,
    /// TCP-Listen-Adresse, z. B. "0.0.0.0:53".
    pub listen_tcp: String,
    /// Upstream-Resolver (UDP/TCP), Standard: lokales Unbound "127.0.0.1:5353".
    pub upstream: String,
    /// Antwort-Verhalten für Block-Treffer.
    #[serde(default)]
    pub block_mode: BlockMode,
    /// Blocklisten-Dateien (hosts-, Domänen- oder AdGuard-Format).
    #[serde(default)]
    pub blocklists: Vec<PathBuf>,
    /// Allowlisten-Dateien.
    #[serde(default)]
    pub allowlists: Vec<PathBuf>,
    /// Root-Hints-Auto-Update-Intervall in Tagen (0 = aus). Standard: 4.
    #[serde(default = "default_root_hints_days")]
    pub root_hints_update_days: u32,

    /// Web-UI/REST (Dashboard, Testseite, APIs). Leer/ohne = aus.
    #[serde(default)]
    pub admin_listen: Option<String>,
    /// DoH-Upstream (URL wie https://dns.cloudflare.com/dns-query), "" = aus.
    /// Bevorzugt gegenüber `upstream`; UDP fungiert als Failover.
    #[serde(default)]
    pub upstream_doh: String,
    /// Verzeichnis für Remote-Listen-Cache.
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    /// Remote-Listen per HTTPS aktualisieren (Cache im `data_dir`).
    #[serde(default)]
    pub fetch_remote: bool,
    /// Remote-Blocklisten-URLs (hosts-, Domänen- oder AdGuard-Format).
    #[serde(default)]
    pub remote_blocklists: Vec<String>,
    /// Refresh-Intervall der Remote-Listen in Stunden.
    #[serde(default = "default_remote_refresh_hours")]
    pub remote_refresh_hours: u32,
    /// Server für den DNS-Uplink-Test (Antwortfähigkeit + RTT).
    #[serde(default = "default_uplink_test_servers")]
    pub uplink_test_servers: Vec<String>,
}

fn default_root_hints_days() -> u32 {
    4
}

impl Config {
    /// Liest die Konfiguration aus einer Datei.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path)?;
        Self::parse_str(&raw)
    }

    /// Parst die Konfiguration aus einem String.
    /// (Bewusst nicht `FromStr`-Trait: TOML-Input, keine automatische `parse`-Semantik.)
    pub fn parse_str(raw: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(raw)?)
    }

    /// Löst die Adressen auf und erzeugt die Laufzeit-Konfiguration.
    pub fn resolve(&self) -> Result<ResolvedConfig, ConfigError> {
        let parse = |s: &str| -> Result<SocketAddr, ConfigError> {
            s.parse().map_err(|source| ConfigError::Addr {
                addr: s.to_string(),
                source,
            })
        };
        let admin_listen = match self.admin_listen.as_deref() {
            None | Some("") => None,
            Some(s) => Some(parse(s)?),
        };
        let upstream_doh = match self.upstream_doh.trim() {
            "" => None,
            url => Some(url.to_string()),
        };
        let mut uplink_test_servers = Vec::with_capacity(self.uplink_test_servers.len());
        for s in &self.uplink_test_servers {
            uplink_test_servers.push(parse(s)?);
        }
        Ok(ResolvedConfig {
            listen_udp: parse(&self.listen_udp)?,
            listen_tcp: parse(&self.listen_tcp)?,
            upstream: parse(&self.upstream)?,
            block_mode: self.block_mode,
            blocklists: self.blocklists.clone(),
            allowlists: self.allowlists.clone(),
            root_hints_update_days: self.root_hints_update_days,
            admin_listen,
            upstream_doh,
            data_dir: PathBuf::from(&self.data_dir),
            fetch_remote: self.fetch_remote,
            remote_blocklists: self.remote_blocklists.clone(),
            remote_refresh_hours: self.remote_refresh_hours,
            uplink_test_servers,
        })
    }
}

/// Aufgelöste (validierte) Konfiguration für den Serverbetrieb.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub listen_udp: SocketAddr,
    pub listen_tcp: SocketAddr,
    pub upstream: SocketAddr,
    pub block_mode: BlockMode,
    pub blocklists: Vec<PathBuf>,
    pub allowlists: Vec<PathBuf>,
    pub root_hints_update_days: u32,

    pub admin_listen: Option<SocketAddr>,
    pub upstream_doh: Option<String>,
    pub data_dir: PathBuf,
    pub fetch_remote: bool,
    pub remote_blocklists: Vec<String>,
    pub remote_refresh_hours: u32,
    pub uplink_test_servers: Vec<SocketAddr>,
}

impl ResolvedConfig {
    /// Minimale Laufzeit-Konfiguration (Tests, Smoke-Läufe).
    pub fn minimal(listen: SocketAddr, upstream: SocketAddr) -> Self {
        Self {
            listen_udp: listen,
            listen_tcp: listen,
            upstream,
            block_mode: BlockMode::Zero,
            blocklists: vec![],
            allowlists: vec![],
            root_hints_update_days: 0,
            admin_listen: None,
            upstream_doh: None,
            data_dir: PathBuf::from("data"),
            fetch_remote: false,
            remote_blocklists: vec![],
            remote_refresh_hours: 12,
            uplink_test_servers: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_config() {
        let raw = r#"
listen_udp = "127.0.0.1:5300"
listen_tcp = "127.0.0.1:5300"
upstream = "127.0.0.1:5353"
block_mode = "nxdomain"
blocklists = ["a.txt"]
"#;
        let cfg = Config::parse_str(raw).unwrap();
        let res = cfg.resolve().unwrap();
        assert_eq!(
            res.listen_udp,
            "127.0.0.1:5300".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(res.block_mode, BlockMode::NxDomain);
        assert_eq!(res.root_hints_update_days, 4);
        assert!(res.allowlists.is_empty());
        assert!(res.admin_listen.is_none());
        assert!(res.upstream_doh.is_none());
        assert!(
            res.uplink_test_servers.len() == 3,
            "Standard-Uplink-Test-Server"
        );
        assert_eq!(res.data_dir, PathBuf::from("data"));
        assert!(!res.fetch_remote);
        assert_eq!(res.remote_refresh_hours, 12);
    }

    #[test]
    fn parses_new_fields() {
        let raw = r#"
listen_udp = "127.0.0.1:5300"
listen_tcp = "127.0.0.1:5300"
upstream = "127.0.0.1:5353"
admin_listen = "127.0.0.1:8080"
upstream_doh = "https://dns.cloudflare.com/dns-query"
data_dir = "/var/lib/dnsblock"
fetch_remote = true
remote_blocklists = ["https://example.com/hosts.txt"]
remote_refresh_hours = 24
uplink_test_servers = ["9.9.9.9:53"]
"#;
        let res = Config::parse_str(raw).unwrap().resolve().unwrap();
        assert_eq!(res.admin_listen, Some("127.0.0.1:8080".parse().unwrap()));
        assert_eq!(
            res.upstream_doh.as_deref(),
            Some("https://dns.cloudflare.com/dns-query")
        );
        assert_eq!(res.data_dir, PathBuf::from("/var/lib/dnsblock"));
        assert!(res.fetch_remote);
        assert_eq!(res.remote_blocklists.len(), 1);
        assert_eq!(res.remote_refresh_hours, 24);
        assert_eq!(res.uplink_test_servers.len(), 1);
    }

    #[test]
    fn defaults_block_mode_zero() {
        let raw = r#"
listen_udp = "0.0.0.0:53"
listen_tcp = "0.0.0.0:53"
upstream = "127.0.0.1:5353"
"#;
        let cfg = Config::parse_str(raw).unwrap();
        assert_eq!(cfg.block_mode, BlockMode::Zero);
    }

    #[test]
    fn invalid_address_is_rejected() {
        let raw = r#"
listen_udp = "keine-adresse"
listen_tcp = "0.0.0.0:53"
upstream = "127.0.0.1:5353"
"#;
        let cfg = Config::parse_str(raw).unwrap();
        assert!(cfg.resolve().is_err());
    }

    #[test]
    fn invalid_uplink_test_server_is_rejected() {
        let raw = r#"
listen_udp = "127.0.0.1:5300"
listen_tcp = "127.0.0.1:5300"
upstream = "127.0.0.1:5353"
uplink_test_servers = ["nüscht"]
"#;
        assert!(Config::parse_str(raw).unwrap().resolve().is_err());
    }
}
