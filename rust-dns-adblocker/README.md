# dnsblock – Rust-DNS-Adblocker

Ein Self-Hosted-Werbe- und Tracker-Blocker, **komplett in Rust**, der die
Stärken von **Pi-hole** und **AdGuard Home** kombiniert – als Firewall-tauglicher
DNS-Sinkhole-Daemon mit Web-UI, Netzwerk-/Router-Erkennung, DNS-Uplink-Test,
DoH-Upstream, Remote-Listen mit Hot-Reload und einem YouTube-Begleitmodul.

**Status: Phase 1 + 2 + Kern von Phase 3/4 fertig** – alle acht Projektziele sind
implementiert (Tabelle unten).

## Features

| # | Anforderung | Umsetzung |
|---|---|---|
| 1 | DNS-Filter | UDP/TCP-Sinkhole auf Port 53 (konfigurierbar), hosts- + Domänen- + AdGuard-Syntax, drei Block-Modi (`zero`/`nxdomain`/`refused`) |
| 2 | Pi-hole × AdGuard-Aufbau | Web-Dashboard (Live-Zähler, Query-Log, Listen-Übersicht), Remote-Listen mit Atomarem Austausch, Allow/Blocklisten, DoH-Upstream |
| 3 | Komplett in Rust | Rust-Workspace (`filter-engine` + `dnsblock`), tokio + hickory-proto + axum + reqwest(rustls); statisches musl-Binary möglich |
| 4 | YouTube-Werbefilter | `blocklists/youtube.txt` (DNS-seitige YT-Ad-/Tracker-Endpunkte) **+** Begleit-Userscript `/userscript.js` (Skip-/Overlay-Logik im Browser) |
| 5 | Netzwerk-Einstellungen automatisch | `net.rs`: lokale IP, Interface, Maske werden beim Start erkannt und im Dashboard + Startup-Log angezeigt |
| 6 | Router-Erkennung + Vorschläge | Default-Gateway aus `/proc/net/route`, MAC aus `/proc/net/arp`, Herstellererkennung über OUI-Tabelle → konkrete Menü-Pfade pro Router (Fritz!Box, Speedport, ASUS, …) |
| 7 | DNS-Uplink-Test | Beim Start + im Dashboard: RTT/Rcode gegen eigenen Upstream + Public-Resolver, konkurrierend |
| 8 | Werbetestseite | `/test`: Direkt-Engine-Probe (~14 Werbe-Domains + Negativprobe) und echter Browser-Ad-Request-Test |

### Architektur

```
Clients (LAN) ──DNS UDP/TCP──> dnsblock (Rust) ──DoH (optional)──> Cloudflare & Co.
                                    │  └──UDP/TCP──> Unbound (127.0.0.1:5353) ──rekursiv+DNSSEC──> Root-/TLD-Server
                                    │
                                    └──geblockt──> 0.0.0.0 / NXDOMAIN / REFUSED

Bedient wird's über das Web-UI unter `http://<ip>:8080/`.
```

Workspace:

| Crate     | Zweck                                                              |
|-----------|--------------------------------------------------------------------|
| `filter-engine` | Listen-Parser + Matcher (hosts, Domänen, AdGuard-Subset), Suffix-Match, Allow-Vorrang |
| `dnsblock`      | Daemon: UDP/TCP-Server (per-Datagramm-Spawns + Semaphor-Bound), Resolver (DoH→UDP/TCP-Failover), Netzwerk-/Router-Erkennung, Uplink-Test, HTTPS-Listen-Updater mit Hot-Reload, REST + Web-UI |

## Installation (Installer-Script)

Ein Befehl, interaktiv (Server / Raspberry Pi / Docker):

```sh
curl -sSL https://raw.githubusercontent.com/b3nb0x/Adblock/main/install.sh | bash
```

Der Installer ist **pipe-safe** (curl|bash), fragt den Modus interaktiv über
`/dev/tty` ab (falls vorhanden) und unterstützt Flags für Automatisierung:

```sh
curl -sSL .../install.sh | bash -s -- --mode docker            # oder server|pi
# Update/deinstall:
./install.sh --update        # Binary/Container neu bauen, Konfig bleibt
./install.sh --uninstall
```

| Modus | Ablauf |
|---|---|
| `server` | Native Install: `/usr/local/bin/dnsblock`, Konfig `/etc/dnsblock/config.toml`, Daten `/var/lib/dnsblock`, systemd-Dienst (DynamicUser + `CAP_NET_BIND_SERVICE`) |
| `pi` | Identisch zu `server` – zusätzlich wird rustup/cargo nachinstalliert, Build dauert auf ARM länger |
| `docker` | Quellcode nach `/etc/dnsblock/docker/src`, Multi-Stage-Container-Build (`Dockerfile` + `compose.yaml`), Ports 53/udp, 53/tcp, 8080 |
| (ohne Root) | Fallback: `~/.local/bin`, `~/.config/dnsblock`, Port `5300`, Start per PID-Datei |

**Port-53-Konflikt:** erkennt systemd-resolved-Stub (127.0.0.53) und bietet an,
ihn via `/etc/systemd/resolved.conf.d/dnsblock-stub.conf` zu deaktivieren
(Überspringen mit `ADBLOCK_NO_RESOLVED_FIX=1`). Überschreibbar via Env-Vars:
`ADBLOCK_PORT`, `ADBLOCK_ADMIN_PORT`, `ADBLOCK_MODE`.

**Selbsttest** nach dem Start: geblockte Werbe-Domain (doubleclick.net →
0.0.0.0/NXDOMAIN), Kontrolle (example.com → echte IP), Dashboard-API
(`/api/stats`) und Prozess-/Container-Status.

**Auto-Update (alle 2 Tage):**

```text
dnsblock-update.timer  → weckt täglich
dnsblock-update        → debounct auf 48 h (Stempel /var/lib/dnsblock/.last_update_check)
```

- Aus-Schalter: `/etc/dnsblock/update.conf` → `AUTO_UPDATE=no`
- Manuelles Erzwingen: `dnsblock-update --force`
- Der Installer wird zur Wiederherstellung nach `/etc/dnsblock/install.sh` gecached;
  Update-Timer reinstalliert Binary bzw. Container-Image, Konfig bleibt erhalten.

**Dashboard-Init** nach der Installation öffnen: `http://<host>:8080/setup` –
dort prüft das Web-UI Netzwerk/Router (Anforderung 5+6) und bietet die
Filterlisten-Auswahl als Katalog mit 6 kuratierten Listen + eigene URLs;
Speichern lädt und aktiviert sie direkt in die laufende Engine (Hot-Reload).

## Schnellstart (Entwicklung)

```sh
cargo run -- config/default.toml
# → Web-UI: http://127.0.0.1:8080/   Dashboard + Testseite + APIs
```

### Konfigurationsprofile

| Datei | Sinn |
|---|---|
| `config/default.toml` | **Pi-hole/Unbound-Fokus:** Upstream = lokales Unbound (`127.0.0.1:5353`, rekursiv + DNSSEC), Web-UI auf localhost |
| `config/public.toml`  | **AdGuard-Standalone:** DoH bevorzugt (`https://dns.cloudflare.com/dns-query`), UDP-Failover, Port 53, Web-UI im LAN |
| `config/docker.toml`  | **Container-Profil** (vom Image eingebaut): DoH, Listen unter `/opt/dnsblock/blocklists`, Daten-Volume `/var/lib/dnsblock` |

Wichtige Parameter (alles optional, Defaults im Code):

```toml
admin_listen        = "127.0.0.1:8080"      # ""/ohne = Web-UI aus
upstream_doh        = ""                    # RFC 8484-URL, bevorzugt gegenüber upstream
fetch_remote        = true                  # HTTPS-Fetch der Remote-Listen
remote_blocklists   = [ "https://adguardteam.github.io/HostlistsRegistry/assets/filter_1.txt", ... ]
remote_refresh_hours = 24                   # Refresh-Zyklus (Cache: data/remote/)
uplink_test_servers = ["1.1.1.1:53", "8.8.8.8:53", "9.9.9.9:53"]
block_mode          = "zero"                # zero | nxdomain | refused
```

### Produktion: Port 53

```sh
cargo build --release
sudo setcap 'cap_net_bind_service=+ep' target/release/dnsblock
./target/release/dnsblock config/default.toml   # Ports in der TOML auf :53
```

Statisches Binary (Router/NAS/Pi):

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

### systemd

Der Installer nutzt die fertige Unit [`packaging/dnsblock.service`](packaging/dnsblock.service)
(DynamicUser, `CAP_NET_BIND_SERVICE`, Härtung via ProtectSystem). Manuell entspricht das:

```ini
[Service]
DynamicUser=yes
StateDirectory=dnsblock
ExecStart=/usr/local/bin/dnsblock /etc/dnsblock/config.toml
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
KillSignal=SIGINT
Restart=on-failure
```

### Router-DNS umstellen

Beim Start erkennt der Daemon Gateway + Hersteller und loggt den Weg
(und zeigt ihn im Dashboard). Für FRITZ!Box z. B.:
*Heimnetz → Netzwerk → Netzwerkeinstellungen → DNS-Server für das Heimnetz.*
Die dnsblock-Maschine hat dann als DNS die erkannte lokale IP (z. B. `192.168.1.10`).

## Web-UI

| Route | Zweck |
|---|---|
| `/` | Dashboard: Anfragen/geblockt/Quote/Regeln, Uplink-Test live, Netzwerkkarte mit Router-Vorschlag, Listen-Übersicht, Query-Log (RAM-only) |
| `/setup` | **Ersteinrichtung:** Netzwerk-/Router-Erkennung (Anforderung 5+6) + Filterlisten-Auswahl aus dem Katalog (Installer-Punkt 5) |
| `/test` | Werbetestseite (Anforderung 8): Filter-Engine-Direktprobe + echter Browser-Ad-Request |
| `/userscript.js` | Begleit-Userscript für YouTube (in Tampermonkey/Violentmonkey laden) — Client-Anteil des YT-Filters |
| `/api/stats` `/api/lists` `/api/network` `/api/uplink` `/api/uplink/last` `/api/probe?d=…` `/api/setup` | REST-API für eigene Clients (Setup: GET Katalog, POST Auswahl) |

Privacy-Note: Das Query-Log lebt **nur im RAM** (letzte 500 Queries) und wird
nicht persistiert.

## YouTube-Modul (ehrliche Einschränkung)

YouTube liefert Ads über **dieselbe Infrastruktur** (`googlevideo.com`) wie die
Videos – reines DNS kann die Anzeigen nicht trennen, ohne das Streaming kaputt
zu machen. Deshalb arbeitet das Modul zweistufig:

1. **DNS-seitig** (`blocklists/youtube.txt`): alle YT-Anzeige-/Tracker-Endpunkte,
   die sicher vom Videostream getrennt sind (doubleclick, googlesyndication,
   adservice, …). `googlevideo.com`, `youtube.com`, `ytimg.com` werden
   **bewusst nicht blockiert**.
2. **Client-seitig** (`/userscript.js`): klickt Skip-Buttons, schließt
   Overlay-Banner, entfernt Feed-/Masthead-Ads. Firefox/Chrome via
   Tampermonkey installieren: URL `http://<dnsblock>:8080/userscript.js`
   direkt als Userscript anlegen (Header ist enthalten).

## Unbound als Upstream (default-Profil)

`/etc/unbound/unbound.conf.d/dnsblock-upstream.conf`:

```yaml
server:
    interface: 127.0.0.1
    port: 5353
    root-hints: "/var/lib/unbound/root.hints"
    auto-trust-anchor-file: "/var/lib/unbound/root.key"
    val-permissive-mode: no
```

So bleibt alles rekursiv und DNSSEC-validierend – keine Public-Resolver im
default-Profil. Root-Hints-/`root.key`-Zyklus wird in Phase 3 vom Daemon
selbst gefahren (`updater.rs`-Grundgerüst vorhanden).

## Container (docker compose)

```sh
cd rust-dns-adblocker
docker compose up -d --build    # Build im Container, kein lokales Rust nötig
```

- Ports: 53/udp + 53/tcp + 8080 → Host (also auch hier resolved-Stub-Fix falls nötig)
- Daten: Named Volume `dnsblock_data` (/var/lib/dnsblock – Remote-Listen-Cache)
- Runtime als eigener Container-Benutzer `dnsblock`, Port 53 via `setcap`/`cap_add`
- Update: `bash install.sh --update --mode docker` (Quellcode neu ziehen → rebuild → up -d)

## Tests

```sh
cargo test
```

- 23 Unit-Tests: Filter-Engine (Parser, Suffix-Match, Allow-Vorrang), DNS-Antworten,
  Config inkl. neuer Felder, Netz-/Router-Erkennung (hex→IP, OUI, Vorschläge),
  Uplink-Formatierung, Updater (atomar, FNV, Rebuild+Infos, Auswahl-Auf-/Überschreiben), Stats/Log-Cap
- 7 Integrationstests: echter UDP/TCP-Server mit gemocktem Upstream
  (Block, Subdomänen, Allow, Forward, SERVFAIL bei tottem Upstream, TCP, Statistik)
- 8 Filter-Engine-Tests im eigenen Crate
- Clippy: 0 Warnungen (`cargo clippy --all-targets`)

## Roadmap

- **Phase 1 (fertig):** DNS-Daemon-Kern, Filter-Engine, UDP/TCP, Statistik
- **Phase 2 (fertig):** Remote-Listen-Fetch (HTTPS), Hot-Reload, DoH-Upstream,
  Netz-/Router-Erkennung, Uplink-Test, Web-UI, YouTube-Modul (Anforderungen 1–8)
- **Phase 3:** Query-Log auf `redb`/SQLite (opt-in), Per-Client-Regeln (Quell-IP),
  DoH/DoT-Server-Endpoint, Root-Hints-Fetch im Daemon
- **Phase 4+:** Supervise das Unbound-Sidecar selbst, DNS-Kompression/EDNS
  für synthetische Antworten, IPv6-RA-Konfiguration-Hint

## Hinweise

- Lizenz: MIT (siehe [LICENSE](../LICENSE))
- Robustheit: UDP-Handler sind pro Datagramm gespawnt (max. 256 parallel) –
  ein hängender Upstream blockiert die LAN-DNS nicht mehr.
- 64-KiB-UDP-Empfangspuffer: EDNS-Antworten werden nicht beschnitten.
- Ein hängender Listen-Fetch oder die Übersetzung einer einzelnen
  fehlerhaften Liste saturiert nie die Query-Pipeline (Atomic Writes,
  Hot-Reload, fail-closed-Cache).
