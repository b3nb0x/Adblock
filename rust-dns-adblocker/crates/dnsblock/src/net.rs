//! Netzwerk- und Router-Erkennung (Linux).
//!
//! Liest-Laufzeit-Infos aus `/proc/net/route` (Default-Gateway + Interface +
//! Maske) und `/proc/net/arp` (MAC des Gateways), ordnet die MAC über eine
//! kurze OUI-Tabelle einem Router-Hersteller zu und erzeugt daraus konkrete
//! Konfigurations-Vorschläge (Anforderung 5 + 6).
//!
//! Die lokale IP wird ohne Paketversand ermittelt: ein UDP-`connect` zu
//! einem Public-Endpoint entscheidet routeseitig; `local_addr()` gibt die
//! quellside IP zurück (Standard-Trick, portabel und ohne Root).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::OnceLock;

/// Erkanntes Netz. Alle Felder optional – unbekannt bleibt unbekannt
/// (keine erfundene Fallback-IP).
#[derive(Debug, Clone, Default)]
pub struct NetworkInfo {
    pub local_ip: Option<IpAddr>,
    pub netmask: Option<IpAddr>,
    pub iface: Option<String>,
    pub gateway: Option<IpAddr>,
    pub gateway_mac: Option<String>,
    pub vendor: Option<&'static str>,
}

/// Konkreter Konfigurations-Vorschlag für den erkanntenen Router.
#[derive(Debug, Clone)]
pub struct Suggestion {
    /// Anzeigename des Herstellers (oder "Router").
    pub vendor: &'static str,
    /// IP des Routers (Weboberfläche).
    pub router_ip: String,
    /// IP, die als DNS-Server im Router eingetragen werden soll (= dnsblock).
    pub dns_to_set: String,
    /// Menüpfad in der Router-Oberfläche.
    pub path: &'static str,
    /// Kurzer Hinweis (Kontext/Verify).
    pub note: &'static str,
}

impl NetworkInfo {
    /// Vorschlag aus erkannten Werten; ohne Netzwerk nur der generische Text.
    pub fn suggestion(&self) -> Option<Suggestion> {
        let dns = self.local_ip?.to_string();
        let router_ip = self
            .gateway
            .map(|g| g.to_string())
            .unwrap_or_else(|| "<Router-IP>".into());
        let (vendor, path, note) = match self.vendor {
            Some("AVM (FRITZ!Box)") => (
                "AVM (FRITZ!Box)",
                "Heimnetz > Netzwerk > Netzwerkeinstellungen > DNS-Server für das Heimnetz",
                "IPv4- statt IPv6-DNS verwenden; Geräte danach neu verbinden (WLAN aus/an) oder Router rebooten.",
            ),
            Some("Sagemcom (Speedport/Vodafone/Telekom)") => (
                "Sagemcom (Speedport/Vodafone/Telekom)",
                "System > Netzwerkeinstellungen > DNS-/DHCP-Server",
                "Speedport: DNS-Server-Override aktivieren und dnsblock-IP eintragen.",
            ),
            Some("Huawei") => (
                "Huawei",
                "Weboberfläche > LAN > DHCP-Server > DNS",
                "Primären DNS auf dnsblock-IP setzen.",
            ),
            Some("NETGEAR") => (
                "NETGEAR",
                "Advanced > Setup > LAN Setup > DNS Address",
                "Router-DNS auf dnsblock-IP setzen; einige Modelle müssen danach neu verbunden werden.",
            ),
            Some("TP-Link") => (
                "TP-Link",
                "Advanced > Network > DHCP Server > Primary DNS",
                "DNS 1 auf dnsblock-IP setzen.",
            ),
            Some("ASUS") => (
                "ASUS",
                "WAN > Internet connection > WAN DNS Setting",
                "DNS Server 1 auf dnsblock-IP setzen.",
            ),
            Some("MikroTik (RouterOS)") => (
                "MikroTik (RouterOS)",
                "WebFig/WinBox: IP > DHCP Server > <Server> > Network > DNS Servers",
                "DNS-Server auf dnsblock-IP setzen; Geräte mit hartem DNS per DNS-Weiterleitung rudern: /ip firewall nat → chain=dstnat, dst-port=53 (udp+tcp), action=redirect.",
            ),
            Some(v) => (
                v,
                "Weboberfläche > LAN/DHCP > DNS-Server",
                "DNS-Server auf dnsblock-IP setzen; Geräte danach neu verbinden.",
            ),
            None => (
                "Router",
                "Weboberfläche > LAN/DHCP > DNS-Server",
                "Primären DNS-Server auf dnsblock-IP setzen; Geräte danach neu verbinden.",
            ),
        };
        Some(Suggestion {
            vendor,
            router_ip,
            dns_to_set: dns,
            path,
            note,
        })
    }

    /// Verzeichnis der Daten für das Web-UI (`/api/network`).
    pub fn summary(&self) -> String {
        format!(
            "IP={} / Maske={} / IF={} / GW={} / MAC={} / Hersteller={}",
            self.local_ip
                .map(|i| i.to_string())
                .unwrap_or("unbekannt".into()),
            self.netmask
                .map(|i| i.to_string())
                .unwrap_or("unbekannt".into()),
            self.iface.clone().unwrap_or_else(|| "unbekannt".into()),
            self.gateway
                .map(|i| i.to_string())
                .unwrap_or("unbekannt".into()),
            self.gateway_mac
                .clone()
                .unwrap_or_else(|| "unbekannt".into()),
            self.vendor.unwrap_or("unbekannt")
        )
    }
}

/// Sammelt alle Laufzeit-Info; fehlschlagende Teile werden `None` geloggt.
pub fn detect() -> NetworkInfo {
    let local_ip = local_ip_linux();
    let (iface, gateway, netmask) = route_info_linux();
    let gateway_mac = gateway.and_then(|gw| arp_mac_linux(&gw.to_string()));

    // OUI-Vendor-Lookup nur wenn MAC aus der ARP-Tabelle kommt.
    let vendor = gateway_mac.as_deref().and_then(mac_vendor);

    let info = NetworkInfo {
        local_ip,
        netmask,
        iface,
        gateway,
        gateway_mac,
        vendor,
    };
    tracing::info!("{}", info.summary());
    info
}

/// Lokale (primäre) IP ohne Paketversand.
pub fn local_ip_linux() -> Option<IpAddr> {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    // UDP connect sendet nichts – nur Routen-Auswahl.
    sock.connect("8.8.8.8:53").ok()?;
    Some(sock.local_addr().ok()?.ip())
}

/// (iface, gateway, mask) des Default-Route aus `/proc/net/route`.
pub fn route_info_linux() -> (Option<String>, Option<IpAddr>, Option<IpAddr>) {
    default_route_entry().map_or((None, None, None), |e| {
        (
            e.iface,
            e.gateway.as_deref().and_then(hex_to_ip),
            e.mask.as_deref().and_then(hex_to_ip),
        )
    })
}

struct RouteEntry {
    iface: Option<String>,
    gateway: Option<String>,
    mask: Option<String>,
}

/// Zeile mit Destination = 0.0.0.0 holen (Default-Route).
fn default_route_entry() -> Option<RouteEntry> {
    let raw = std::fs::read_to_string("/proc/net/route").ok()?;
    for line in raw.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 8 {
            continue;
        }
        if cols[1] != "00000000" {
            continue; // nicht die Default-Route
        }
        return Some(RouteEntry {
            iface: Some(cols[0].to_string()),
            gateway: Some(cols[2].to_string()),
            mask: Some(cols[7].to_string()),
        });
    }
    None
}

/// Kernel-typische Hex-Darstellung ("0100000A") in IP umwandeln.
///
/// `/proc/net/route` druckt die Felder mit `%08X` als **numerischen** u32 aus:
/// die Oktette des `__be32` liegen auf Little-Endian-Hosts also gedreht im Wort
/// (Gateway 10.0.0.1 → "0100000A"). `to_le_bytes` setzt die echte
/// Oktett-Reihenfolge wieder zusammen.
pub fn hex_to_ip(hex: &str) -> Option<IpAddr> {
    let word = u32::from_str_radix(hex, 16).ok()?;
    let [a, b, c, d] = word.to_le_bytes();
    Some(IpAddr::V4(Ipv4Addr::new(a, b, c, d)))
}

/// MAC des Gateways aus `/proc/net/arp` holen.
pub fn arp_mac_linux(gateway_ip: &str) -> Option<String> {
    let raw = std::fs::read_to_string("/proc/net/arp").ok()?;
    for line in raw.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        if cols[0] == gateway_ip {
            let mac = cols[3].trim().to_string();
            if mac.contains(':') && mac.len() == 17 {
                return Some(mac);
            }
            return None;
        }
    }
    None
}

/// Kurze OUI-Tabelle (nur weit verbreitete Router-Hersteller).
static VENDOR_TABLE: OnceLock<HashMap<[u8; 3], &'static str>> = OnceLock::new();

fn vendor_table() -> &'static HashMap<[u8; 3], &'static str> {
    VENDOR_TABLE.get_or_init(|| {
        let mut m = HashMap::new();
        for (oui, name) in VENDOR_TABLE_ENTRIES {
            m.insert(*oui, *name);
        }
        m
    })
}

const VENDOR_TABLE_ENTRIES: &[([u8; 3], &str)] = &[
    ([0x00, 0x1C, 0x4A], "AVM (FRITZ!Box)"),
    ([0x34, 0x31, 0xC4], "AVM (FRITZ!Box)"),
    ([0x7C, 0xFF, 0x4D], "AVM (FRITZ!Box)"),
    ([0xA8, 0xD3, 0xF7], "AVM (FRITZ!Box)"),
    ([0xE0, 0x28, 0x6D], "AVM (FRITZ!Box)"),
    ([0x00, 0x12, 0xBF], "Sagemcom (Speedport/Vodafone/Telekom)"),
    ([0x00, 0xE0, 0xFC], "Huawei"),
    ([0x9C, 0x3D, 0xCF], "NETGEAR"),
    ([0xA0, 0x40, 0xA0], "NETGEAR"),
    ([0xF4, 0xF2, 0x6D], "TP-Link"),
    ([0x0C, 0x27, 0x24], "TP-Link"),
    ([0x04, 0xD9, 0xF5], "ASUS"),
    ([0xAC, 0x9E, 0x17], "ASUS"),
    // MikroTik/Routerboard (via macvendors geprüft, 2026-11):
    ([0x78, 0x9A, 0x18], "MikroTik (RouterOS)"),
    ([0x48, 0x8F, 0x5A], "MikroTik (RouterOS)"),
    ([0x2C, 0xC8, 0x1B], "MikroTik (RouterOS)"),
    ([0x64, 0xD1, 0x54], "MikroTik (RouterOS)"),
    ([0xB8, 0x69, 0xF4], "MikroTik (RouterOS)"),
    ([0xCC, 0x2D, 0xE0], "MikroTik (RouterOS)"),
    ([0xD4, 0xCA, 0x6D], "MikroTik (RouterOS)"),
    ([0xF0, 0x9F, 0xC2], "Ubiquiti"),
    ([0x24, 0x5A, 0x4C], "Ubiquiti"),
];

/// MAC ("aa:bb:cc:dd:ee:ff") → Hersteller über die OUI-Tabelle.
pub fn mac_vendor(mac: &str) -> Option<&'static str> {
    let mut oui = [0u8; 3];
    let mut nibb = 0usize;
    for c in mac.bytes() {
        if nibb >= 6 {
            break;
        }
        let hex = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => continue,
        };
        nibb += 1;
        // Nach dem Inkrement: Index = (nibb-1)/2, High-Nibble auf ungeraden,
        // Low-Nibble auf geraden Positionen.
        let idx = (nibb - 1) / 2;
        if nibb % 2 == 1 {
            oui[idx] = hex << 4;
        } else {
            oui[idx] |= hex;
        }
    }
    if nibb < 6 {
        return None;
    }
    vendor_table().get(&oui).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_to_ip_parses_kernel_le_printed_words() {
        // Realer Druck von /proc/net/route auf Little-Endian-Hosts
        // (Gateway-Feld im Hinweis numerisch verdreht):
        assert_eq!(
            hex_to_ip("0100000A").unwrap(), // live auf dieser Maschine beobachtet
            "10.0.0.1".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            hex_to_ip("0101A8C0").unwrap(), // Gateway 192.168.1.1
            "192.168.1.1".parse::<IpAddr>().unwrap()
        );
        // Netzmaske 255.255.255.0 → FF FF FF 00 → Wort 00FFFFFF:
        assert_eq!(
            hex_to_ip("00FFFFFF").unwrap(),
            "255.255.255.0".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            hex_to_ip("00000000").unwrap(),
            "0.0.0.0".parse::<IpAddr>().unwrap()
        );
        assert!(hex_to_ip("xyz").is_none());
    }

    #[test]
    fn mac_vendor_matches_oui() {
        assert_eq!(mac_vendor("34:31:c4:aa:bb:cc"), Some("AVM (FRITZ!Box)"));
        assert_eq!(mac_vendor("34:31:C4:aa:bb:cc"), Some("AVM (FRITZ!Box)"));
        assert_eq!(mac_vendor("AC:9E:17:11:22:33"), Some("ASUS"));
        assert_eq!(
            mac_vendor("78:9A:18:de:ad:0f"),
            Some("MikroTik (RouterOS)")
        );
        assert_eq!(mac_vendor("48:8f:5a:00:11:22"), Some("MikroTik (RouterOS)"));
        assert_eq!(mac_vendor("00:11:22:33:44:55"), None);
        assert_eq!(mac_vendor("keine mac"), None);
    }

    #[test]
    fn suggestion_falls_back_to_generic() {
        let net = NetworkInfo {
            local_ip: Some("192.168.1.10".parse().unwrap()),
            gateway: Some("192.168.1.1".parse().unwrap()),
            ..Default::default()
        };
        let s = net.suggestion().unwrap();
        assert_eq!(s.vendor, "Router");
        assert_eq!(s.dns_to_set, "192.168.1.10");
        assert_eq!(s.router_ip, "192.168.1.1");

        // Ohne lokale IP reicht Vorschlag; Router-IP generisch.
        let net2 = NetworkInfo::default();
        assert!(net2.suggestion().is_none());
    }

    #[test]
    fn avm_suggestion_text() {
        let net = NetworkInfo {
            local_ip: Some("192.168.1.10".parse().unwrap()),
            gateway: Some("192.168.1.1".parse().unwrap()),
            gateway_mac: Some("34:31:c4:00:00:01".into()),
            vendor: Some("AVM (FRITZ!Box)"),
            ..Default::default()
        };
        let s = net.suggestion().unwrap();
        assert_eq!(s.vendor, "AVM (FRITZ!Box)");
        assert!(
            s.path.contains("FRITZ!Box") || s.path.contains("Heimnetz"),
            "{}",
            s.path
        );
    }

    #[test]
    fn route_info_never_panics() {
        let _ = route_info_linux();
        let _ = arp_mac_linux("192.0.2.99");
        let _ = local_ip_linux();
    }
}
