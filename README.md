# Adblock – Rust-DNS-Adblocker (`dnsblock`)

Self-hosted Werbe- und Tracker-Blocker komplett in Rust (Pi-hole × AdGuard Home:
DNS-Sinkhole + Web-Dashboard + Netzwerk-/Router-Erkennung + Uplink-Test +
YouTube-Modul). Details: [rust-dns-adblocker/README.md](rust-dns-adblocker/README.md)

## Installation (ein Befehl)

```sh
curl -sSL https://raw.githubusercontent.com/b3nb0x/Adblock/main/install.sh | bash
```

Interaktiv: **Server**, **Raspberry Pi** oder **Docker**. Danach Ersteinrichtung
im Web-Dashboard unter `http://<host>:8080/setup` (Netzwerk, Router,
Filterlisten-Auswahl). Auto-Update alle 2 Tage (Timer, abschaltbar).

Lizenz: MIT – [LICENSE](LICENSE)
