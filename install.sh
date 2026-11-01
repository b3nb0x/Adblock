#!/usr/bin/env bash
# ============================================================================
#  dnsblock – Installer für den Rust-DNS-Adblocker (Pi-hole × AdGuard Home)
#  https://github.com/b3nb0x/Adblock
#
#  Kurzmethode (Pipe-sicher, fragt interaktiv nach):
#      curl -sSL https://raw.githubusercontent.com/b3nb0x/Adblock/main/install.sh | bash
#
#  Modi:
#      server   Native Installation als systemd-Dienst (VM/NAS/Server)
#      pi       Native Installation für Raspberry Pi / ARM (identisch zu server)
#      docker   Quellcode-Build im Container + Compose (kein Host-Rust nötig)
#      user     automatischer Fallback ohne Root (kein systemd-Fokus, Port 5300)
#
#  Flags (alle optional):
#      --mode <server|pi|docker>   Modus vorgeben (überspringt Menü)
#      --uninstall                 Deinstallation
#      --update                    Update im bereits installierten Modus
#      --non-interactive / --yes   keine Nachfragen (Defaults / Umgebungsvariablen)
#      --skip-selftest             Selbsttest auslassen
#      --keep-data                 (bei --uninstall) Daten behalten
#      --repo-dir <Pfad>           Quelle = lokales Verzeichnis (Entwickler/Test)
#
#  Umgebungsvariablen (automatisierte Läufe):
#      ADBLOCK_MODE=server|pi|docker   Modus
#      ADBLOCK_PORT=53                 DNS-Port (server/pi, Standard 53)
#      ADBLOCK_ADMIN_PORT=8080         Dashboard-Port
#      ADBLOCK_NO_RESOLVED_FIX=1       systemd-resolved-Stub NICHT anfassen
#
#  Auto-Update: systemd weckt täglich (dnsblock-update.timer); dnsblock-update
#  debounct auf alle 2 Tage (Stempel: /var/lib/dnsblock/.last_update_check).
#  Aus: /etc/dnsblock/update.conf → AUTO_UPDATE=no.  Erzwingen: dnsblock-update --force
# ============================================================================

# ---- Truncation-Guard: 'curl -sSL … | bash' liest das Script aus der Pipe.
# ---- Interaktion/Hilfsprozesse dürfen den Stream nie direkt teilen, sonst
# ---- kann die Ausführung mid-write abreißen. Lösung: komplette Pipe in eine
# ---- Datei sammeln und aus dieser neu starten.
if [ -z "${_DNSBLOCK_SELF_EXEC:-}" ] && [ ! -t 0 ] && [ -p /dev/stdin ]; then
    _SELFTMP="$(mktemp "${TMPDIR:-/tmp}/dnsblock-install.XXXXXX" 2>/dev/null)" \
        || _SELFTMP="$(mktemp /tmp/dnsblock-install.XXXXXX)"
    cat > "$_SELFTMP"
    chmod +x "$_SELFTMP" 2>/dev/null || true
    export _DNSBLOCK_SELF_EXEC=1
    exec bash "$_SELFTMP" "$@"
fi

REPO="b3nb0x/Adblock"
URL_RAW="https://raw.githubusercontent.com/${REPO}/main"
URL_TARBALL="https://codeload.github.com/${REPO}/tar.gz/refs/heads/main"
URL_RELEASE_LATEST="https://github.com/${REPO}/releases/latest/download"
URL_RUSTUP="https://sh.rustup.rs"

MODE=""; UNINSTALL=0; UPDATE=0; NONINT=0; SKIP_SELFTEST=0; KEEP_DATA=0
REPO_DIR=""
PORT="${ADBLOCK_PORT:-53}"
ADMIN_PORT="${ADBLOCK_ADMIN_PORT:-8080}"
NO_RESOLVED_FIX="${ADBLOCK_NO_RESOLVED_FIX:-0}"
TEST_FAILED=0

# ---------------------------------------------------------------------------
# Ausgabe-Hilfsfunktionen
# ---------------------------------------------------------------------------
have() { command -v "$1" >/dev/null 2>&1; }

C_G=""; C_R=""; C_Y=""; C_B=""; C_0=""
if [ -t 1 ] && have tput; then
    C_G="$(tput setaf 2 2>/dev/null || true)"
    C_R="$(tput setaf 1 2>/dev/null || true)"
    C_Y="$(tput setaf 3 2>/dev/null || true)"
    C_B="$(tput setaf 4 2>/dev/null || true)"
    C_0="$(tput sgr0 2>/dev/null || true)"
fi
step() { printf '%s==>%s %s\n' "$C_B" "$C_0" "$*"; }
ok()   { printf '%s OK%s  %s\n' "$C_G" "$C_0" "$*"; }
warn() { printf '%sWARN%s %s\n' "$C_Y" "$C_0" "$*" >&2; }
die()  { printf '%sFEHLER%s %s\n' "$C_R" "$C_0" "$*" >&2; exit 1; }

fetch_to_file() { # $1=url $2=ziel
    if have curl; then curl -fsSL "$1" -o "$2"; return $?;
    elif have wget; then wget -qO "$2" "$1"; return $?;
    else return 127; fi
}

# Interaktive Nachfragen via /dev/tty (bei curl|bash ist stdin die Pipe).
ask() { # $1=default $2=prompt → Antwort auf stdout
    local def="$1" prompt="$2" ans=""
    if [ "$NONINT" = "1" ] || [ ! -r /dev/tty ]; then
        printf '%s\n' "$def"; return 0
    fi
    printf '%s [%s] ' "$prompt" "$def" > /dev/tty
    if IFS= read -r ans < /dev/tty; then :; else ans=""; fi
    if [ -n "$ans" ]; then printf '%s\n' "$ans"; else printf '%s\n' "$def"; fi
}
ask_yn() { # $1=default(y|n) $2=prompt → Exit-Status 0 wenn ja
    local a; a="$(ask "$1" "$2")"
    case "$(printf '%s' "$a" | tr '[:upper:]' '[:lower:]')" in
        y|j|yes|ja|1) return 0 ;;
        *) return 1 ;;
    esac
}

normalize_mode() {
    case "$1" in pi) printf 'pi';; docker) printf 'docker';; user) printf 'user';; openwrt) printf 'openwrt';; *) printf 'server';; esac
}

arch_name() {
    case "$(uname -m 2>/dev/null || echo x86_64)" in
        x86_64|amd64) printf 'x86_64'  ;;
        aarch64|arm64) printf 'aarch64' ;;
        armv7l|armv7) printf 'armv7'   ;;
        armv6l|armv6) printf 'armv6'   ;;
        *) printf '' ;;
    esac
}

cargo_version_ok() { # $1=cargo → 0 wenn >= 1.75 (axum 0.8 MSRV)
    local v; v="$("$1" --version 2>/dev/null | awk 'NR==1{print $2}')"
    [ -n "$v" ] || return 1
    awk -v v="$v" 'BEGIN{ split(v,p,"."); m=p[1]+0; n=p[2]+0; exit !(m>1 || (m==1 && n>=75)) }'
}

TMPROOT=""; RSRC=""; PREBUILT=""
cleanup() { [ -n "$TMPROOT" ] && [ -d "$TMPROOT" ] && rm -rf "$TMPROOT"; return 0; }
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Argumente
# ---------------------------------------------------------------------------
while [ $# -gt 0 ]; do
    case "$1" in
        --mode)             MODE="$(normalize_mode "${2:-}")"; shift 2 ;;
        --mode=*)           MODE="$(normalize_mode "${1#*=}")"; shift ;;
        --uninstall)        UNINSTALL=1; shift ;;
        --update)           UPDATE=1; shift ;;
        --non-interactive|--yes|-y)
                            NONINT=1; shift ;;
        --skip-selftest)    SKIP_SELFTEST=1; shift ;;
        --keep-data)        KEEP_DATA=1; shift ;;
        --repo-dir)         REPO_DIR="${2:-}"; shift 2 ;;
        --repo-dir=*)       REPO_DIR="${1#*=}"; shift ;;
        --port)             PORT="${2:-53}"; shift 2 ;;
        --admin-port)       ADMIN_PORT="${2:-8080}"; shift 2 ;;
        -h|--help)          sed -n '1,42p' "$0"; exit 0 ;;
        *)                  warn "Unbekanntes Argument: $1"; shift ;;
    esac
done
export REPO_DIR

# ---------------------------------------------------------------------------
# Pfade je nach Root-/Modus
# ---------------------------------------------------------------------------
IS_ROOT=0; HAVE_SD=0
BIN_DIR=""; CONF_DIR=""; DATA_DIR=""; UNIT_DIR=""; UPDATE_BIN=""; SD_UNIT=1
DOCKER_DIR="/etc/dnsblock/docker"
DOCKER_SRC="$DOCKER_DIR/src"
COMPOSE_FILE="$DOCKER_DIR/compose.yaml"

install_paths() {
    if [ "$(id -u)" = "0" ]; then IS_ROOT=1; else IS_ROOT=0; fi
    if [ -d /run/systemd/system ] && have systemctl; then HAVE_SD=1; else HAVE_SD=0; fi

    if [ "$MODE" = "openwrt" ] && [ "$IS_ROOT" = "1" ] && openwrt_detected; then
        # OpenWrt (busybox/procd, kein systemd): Daten auf /etc/dnsblock,
        # Port 5353 statt 53 (dhcnasq belegt :53).
        BIN_DIR="/usr/bin"
        CONF_DIR="/etc/dnsblock"
        DATA_DIR="$CONF_DIR/data"
        UNIT_DIR=""
        UPDATE_BIN="$BIN_DIR/dnsblock-update"
        SD_UNIT=0
        [ "$PORT" = "53" ] && PORT="5353"
        return 0
    fi
    if [ "$IS_ROOT" = "1" ]; then
        BIN_DIR="/usr/local/bin"
        CONF_DIR="/etc/dnsblock"
        DATA_DIR="/var/lib/dnsblock"
        UNIT_DIR="/etc/systemd/system"
        UPDATE_BIN="/usr/local/bin/dnsblock-update"
        SD_UNIT=1
        if [ "$MODE" = "docker" ]; then SD_UNIT=0; fi
    else
        warn "Kein Root – Fallback: Benutzer-Installation (Port 5300, kein Systemdienst, Dashboard lokal)."
        PORT=5300; ADMIN_PORT=8080
        BIN_DIR="$HOME/.local/bin"
        CONF_DIR="$HOME/.config/dnsblock"
        DATA_DIR="$HOME/.local/share/dnsblock"
        UNIT_DIR="$HOME/.config/systemd/user"
        UPDATE_BIN="$BIN_DIR/dnsblock-update"
        SD_UNIT=0
        if [ "$MODE" = "docker" ]; then
            MODE="server"
            warn "Docker-Modus ohne Root übersprungen – nutze nativen Benutzermodus."
        fi
    fi
}

# ---------------------------------------------------------------------------
# Modus wählen
# ---------------------------------------------------------------------------
choose_mode() {
    if [ -n "$MODE" ]; then return 0; fi
    if [ "$NONINT" = "1" ] || [ ! -r /dev/tty ]; then
        local def=server
        openwrt_detected && def="openwrt"
        MODE="$(normalize_mode "${ADBLOCK_MODE:-$def}")"
        warn "Nicht-interaktiv: Modus '${MODE}' (per ADBLOCK_MODE änderbar)."
        return 0
    fi
    {
        printf '\n  %sdnsblock Installer%s – Installationsmodus wählen:\n\n' "$C_B" "$C_0"
        printf '    1) Server   (VM, NAS, root-Server – systemd-Dienst)\n'
        printf '    2) Raspberry Pi / ARM-Gerät (systemd-Dienst)\n'
        printf '    3) Docker / Docker Compose (Container, kein Host-Rust nötig)\n'
        printf '    4) OpenWrt-Gerät (alter Router, procd + dnsmasq-Verkabelung)\n\n'
        printf '    Auswahl [1-4, Standard=1]: '
    } > /dev/tty
    local c=""
    if IFS= read -r c < /dev/tty; then :; else c=""; fi
    case "$(printf '%s' "${c:-1}" | tr -d '[:space:]')" in
        2) MODE="pi" ;;
        3) MODE="docker" ;;
        4) MODE="openwrt" ;;
        *) MODE="server" ;;
    esac
}

# ---------------------------------------------------------------------------
# Quellcode besorgen: Release-Tarball → Repo-Tarball → --repo-dir
# ---------------------------------------------------------------------------
acquire_source() {
    TMPROOT="$(mktemp -d "${TMPDIR:-/tmp}/dnsblock-build.XXXXXX")" || die "mktemp fehlgeschlagen"

    if [ -n "$REPO_DIR" ]; then
        [ -d "$REPO_DIR" ] || die "repo-dir '$REPO_DIR' existiert nicht"
        RSRC="$REPO_DIR"
        ok "Quelle: $RSRC (lokales Verzeichnis – kein Download, keine unbekannte Code-Abhängigkeit)."
        return 0
    fi

    local arch; arch="$(arch_name)"
    if [ "$MODE" = "docker" ]; then
        ok "Docker-Modus: Build läuft im Container – Quellcode statt Release-Binary."
    elif [ -n "$arch" ]; then
        local rel="$URL_RELEASE_LATEST/dnsblock-linux-${arch}.tar.gz"
        if fetch_to_file "$rel" "$TMPROOT/release.tar.gz" 2>/dev/null; then
            if mkdir -p "$TMPROOT/rel" && tar -xzf "$TMPROOT/release.tar.gz" -C "$TMPROOT/rel" 2>/dev/null; then
                local b; b="$(find "$TMPROOT/rel" -type f -name 'dnsblock*' -perm -u+x 2>/dev/null | head -1)"
                if [ -n "$b" ]; then
                    PREBUILT="$b"; RSRC=""
                    ok "Release-Binary gefunden (linux-$arch) – kein Build nötig."
                    return 0
                fi
            fi
        fi
    fi

    if [ "$MODE" = "openwrt" ]; then
        # OpenWrt-Boxen können nicht selbst bauen (RAM/CPU/Flash) –
        # ohne passendes Release-Binary gibt es hier keinen Weg.
        [ -n "$PREBUILT" ] || die "Kein Release-Binary für $(uname -m) (arch '$arch') – OpenWrt-Boxen bauen nicht selbst. Prüfe die veröffentlichten Assets (dnsblock-linux-<arch>.tar.gz)."
        return 0
    fi

    step "Lade Quellcode: $URL_TARBALL"
    fetch_to_file "$URL_TARBALL" "$TMPROOT/src.tar.gz" \
        || die "Download fehlgeschlagen – Internet/DNS/Proxy prüfen"
    mkdir -p "$TMPROOT/src" || die "Arbeitsverzeichnis fehlgeschlagen"
    tar -xzf "$TMPROOT/src.tar.gz" -C "$TMPROOT/src" --strip-components=1 \
        || die "Entpacken fehlgeschlagen"
    if [ -d "$TMPROOT/src/rust-dns-adblocker" ]; then
        RSRC="$TMPROOT/src/rust-dns-adblocker"
    else
        RSRC="$TMPROOT/src"
    fi
    ok "Quellcode bereit"
}

# ---------------------------------------------------------------------------
# Native Build (release → rustup-Fallback)
# ---------------------------------------------------------------------------
cc_present() { echo 'int main(){return 0;}' | cc -x c - -o /dev/null >/dev/null 2>&1; }

ensure_cc() {
    cc_present && return 0
    warn "Kein C-Compiler (cc) – installiere …"
    if have apt-get; then apt-get update -qq && apt-get install -y -qq build-essential >/dev/null;
    elif have dnf; then dnf install -y -q gcc gcc-c++ make >/dev/null;
    elif have apk; then apk add --no-cache build-base >/dev/null;
    elif have zypper; then zypper --non-interactive install gcc make >/dev/null;
    else die "Bitte C-Compiler installieren (z. B. apt install build-essential)."; fi
    cc_present || die "cc weiterhin nicht vorhanden."
}

ensure_toolchain() {
    if have cargo && cargo_version_ok "$(command -v cargo)"; then
        ok "cargo vorhanden: $(cargo --version | awk '{print $1, $2}')"
        return 0
    fi
    if [ -x "$HOME/.cargo/bin/cargo" ] && cargo_version_ok "$HOME/.cargo/bin/cargo"; then
        export PATH="$HOME/.cargo/bin:$PATH"
        ok "cargo vorhanden: $("$HOME/.cargo/bin/cargo" --version | awk '{print $1, $2}')"
        return 0
    fi
    step "Rust-Toolchain fehlt/zu alt (MSRV 1.75) – installiere rustup …"
    fetch_to_file "$URL_RUSTUP" "$TMPROOT/rustup.sh" || die "rustup-Download fehlgeschlagen"
    sh "$TMPROOT/rustup.sh" -y --profile minimal --default-toolchain stable >/dev/null 2>&1 \
        || die "rustup-Installation fehlgeschlagen (siehe ~/.rustup Setup-Log)"
    export PATH="$HOME/.cargo/bin:$PATH"
    ok "Rust installiert: $(cargo --version | awk '{print $1, $2}')"
}

build_native() {
    if [ -n "${PREBUILT:-}" ]; then
        ok "Nutze Release-Binary (kein Build nötig)."
        return 0
    fi
    if [ -x "$RSRC/target/release/dnsblock" ] && [ -n "$REPO_DIR" ] && [ "$UPDATE" = "0" ]; then
        PREBUILT="$RSRC/target/release/dnsblock"
        ok "Nutze bereits gebautes Release-Binary im Quellverzeichnis."
        return 0
    fi
    ensure_cc
    ensure_toolchain
    step "Baue dnsblock (cargo release) – auf dem Pi kann das einige Minuten dauern …"
    ( cd "$RSRC" && CARGO_NET_RETRY=10 CARGO_TERM_COLOR=never cargo build --release -p dnsblock ) \
        || die "cargo build fehlgeschlagen (ggf. Old-Rust? siehe /tmp-Log)"
    PREBUILT="$RSRC/target/release/dnsblock"
    [ -x "$PREBUILT" ] || die "Binary fehlt nach Build"
    ok "Build fertig"
}

# ---------------------------------------------------------------------------
# Native Dateien (Root) installieren
# ---------------------------------------------------------------------------
install_bin() { # robust: temp+rename (läuftender Prozess (Update) bleibt unberührt)
    install -m 755 "$PREBUILT" "$1/.dnsblock.new" || die "Binary-Installation fehlgeschlagen"
    mv -f "$1/.dnsblock.new" "$1/dnsblock"
}

conf_blocklists_paths() { # echo: TOML-Liste der Blocklisten-Pfade
    printf '"%s/blocklists/hosts.txt",\n    "%s/blocklists/adguard.txt",\n    "%s/blocklists/youtube.txt"' \
        "$CONF_DIR" "$CONF_DIR" "$CONF_DIR"
}

install_seed_lists() { # Seed-Blocklisten: kopiert aus der Quelle; Release-Fast-Pfad (OHNE Quellcode): lädt sie von raw
    mkdir -p "$CONF_DIR/blocklists"
    for f in hosts.txt adguard.txt youtube.txt; do
        if [ -n "$RSRC" ] && [ -f "$RSRC/blocklists/$f" ]; then
            install -m 644 "$RSRC/blocklists/$f" "$CONF_DIR/blocklists/$f"
        elif ! [ -s "$CONF_DIR/blocklists/$f" ]; then
            if fetch_to_file "$URL_RAW/rust-dns-adblocker/blocklists/$f" "$CONF_DIR/blocklists/$f" 2>/dev/null; then
                ok "Seed-Liste geladen: $f"
            else
                rm -f "$CONF_DIR/blocklists/$f" 2>/dev/null || true
                warn "Seed-Liste $f fehlt (Quelle ohne Listen, Download scheiterte) – Remote-Listen (Dashboard) decken das ab."
            fi
        fi
    done
}

install_files_native() {
    step "Installiere nach $BIN_DIR / $CONF_DIR / $DATA_DIR …"
    mkdir -p "$BIN_DIR" "$CONF_DIR/blocklists" "$DATA_DIR"
    install_bin "$BIN_DIR"

    install_seed_lists

    if [ ! -f "$CONF_DIR/config.toml" ]; then
        cat > "$CONF_DIR/config.toml" <<EOF
# dnsblock – Konfiguration (vom Installer erzeugt: $CONF_DIR/config.toml)
# Dashboard/Setup: http://127.0.0.1:$ADMIN_PORT/setup

listen_udp = "0.0.0.0:$PORT"
listen_tcp = "0.0.0.0:$PORT"

# Standalone: DoH bevorzugt, UDP-Failover. Mit eigenem Unbound:
#   upstream = "127.0.0.1:5353" und upstream_doh = ""
upstream = "1.1.1.1:53"
upstream_doh = "https://dns.cloudflare.com/dns-query"

block_mode = "zero"

blocklists = [
    $(conf_blocklists_paths)
]
allowlists = []

admin_listen = "0.0.0.0:$ADMIN_PORT"

uplink_test_servers = ["1.1.1.1:53", "8.8.8.8:53", "9.9.9.9:53"]

# Remote-Listen: im Dashboard /setup erweitern/deaktivieren
fetch_remote = true
remote_refresh_hours = 24
remote_blocklists = [
    "https://adguardteam.github.io/HostlistsRegistry/assets/filter_1.txt",
    "https://raw.githubusercontent.com/StevenBlack/hosts/master/hosts",
]
data_dir = "$DATA_DIR"

root_hints_update_days = 0
EOF
        ok "Konfiguration erzeugt: $CONF_DIR/config.toml"
    else
        ok "Bestehende Konfiguration wird behalten: $CONF_DIR/config.toml"
    fi
}

install_units_root() {
    step "Installiere systemd-Dienste + Auto-Update …"
    if [ -d "$RSRC/packaging" ]; then
        install -m 644 "$RSRC/packaging/dnsblock.service"        "$UNIT_DIR/dnsblock.service" || die "dnsblock.service fehlgeschlagen"
        install -m 644 "$RSRC/packaging/dnsblock-update.service" "$UNIT_DIR/dnsblock-update.service"
        install -m 644 "$RSRC/packaging/dnsblock-update.timer"   "$UNIT_DIR/dnsblock-update.timer"
        install -m 755 "$RSRC/packaging/dnsblock-update.sh"      "$UPDATE_BIN"
    else
        warn "packaging/ fehlt in der Quelle – Systemd-Installation übersprungen."
        return 0
    fi

    if [ ! -f "$CONF_DIR/update.conf" ]; then
        install -m 644 "$RSRC/packaging/update.conf" "$CONF_DIR/update.conf"
    fi
    # Nachfrage (nur root): Auto-Update an/aus → update.conf setzen
    local au="y"
    if [ "$NONINT" = "1" ]; then
        au="y"
    elif [ -r /dev/tty ]; then
        if ask_yn y "Automatische Update-Prüfung alle 2 Tage aktivieren?"; then au="y"; else au="n"; fi
    fi
    if [ "$au" = "y" ]; then
        sed -i 's/^AUTO_UPDATE=.*/AUTO_UPDATE=yes/' "$CONF_DIR/update.conf" 2>/dev/null || true
        ok "Auto-Update aktiv (Timer täglich, Script debounct auf 2 Tage;  dnsblock-update --force manuell)."
        systemctl enable --quiet dnsblock-update.timer 2>/dev/null || true
    else
        sed -i 's/^AUTO_UPDATE=.*/AUTO_UPDATE=no/' "$CONF_DIR/update.conf" 2>/dev/null || true
        warn "Auto-Update deaktiviert (update.conf)."
    fi

    systemctl daemon-reload 2>/dev/null || true
    systemctl enable --quiet dnsblock.service 2>/dev/null || true

    # Installer-Cache für den Update-Loop (dnsblock-update.sh fällt sonst auf
    # den GitHub-Download der main-URL zurück).
    install -m 644 "$0" "$CONF_DIR/install.sh" 2>/dev/null \
        || warn "Installer nach $CONF_DIR/install.sh zu spiegeln fehlgeschlagen – Update-Fallback nutzt den Download." || true
    ok "Installer-Cache: $CONF_DIR/install.sh"
}

install_files_user() {
    step "Benutzer-Installation: $BIN_DIR / $CONF_DIR / $DATA_DIR …"
    mkdir -p "$BIN_DIR" "$CONF_DIR/blocklists" "$DATA_DIR" || die "Verzeichnisse"
    install_bin "$BIN_DIR"

    install_seed_lists

    if [ ! -f "$CONF_DIR/config.toml" ]; then
        cat > "$CONF_DIR/config.toml" <<EOF
# dnsblock – Benutzerkonfiguration (ohne Root, Port $PORT statt 53)
listen_udp = "127.0.0.1:$PORT"
listen_tcp = "127.0.0.1:$PORT"
upstream = "1.1.1.1:53"
upstream_doh = "https://dns.cloudflare.com/dns-query"
block_mode = "zero"
blocklists = [
    $(conf_blocklists_paths)
]
allowlists = []
admin_listen = "127.0.0.1:$ADMIN_PORT"
uplink_test_servers = ["1.1.1.1:53", "8.8.8.8:53", "9.9.9.9:53"]
fetch_remote = true
remote_refresh_hours = 24
remote_blocklists = [
    "https://adguardteam.github.io/HostlistsRegistry/assets/filter_1.txt",
    "https://raw.githubusercontent.com/StevenBlack/hosts/master/hosts",
]
data_dir = "$DATA_DIR"
root_hints_update_days = 0
EOF
        ok "Konfiguration erzeugt: $CONF_DIR/config.toml"
    fi
}

# ---------------------------------------------------------------------------
# Docker-Dateien
# ---------------------------------------------------------------------------
install_files_docker() {
    step "Bereite Docker-Struktur vor: $DOCKER_DIR …"
    mkdir -p "$DOCKER_DIR/src" "$DATA_DIR" || die "Docker-Verzeichnisse"
    ( cd "$RSRC" && tar -cf - --exclude='target' --exclude='.git' . ) \
        | ( cd "$DOCKER_SRC" && tar -xf - ) || die "Quellcode-Kopie fehlgeschlagen"
    # compose.yaml: Build-Kontext im Installer-Layout auf ./src ummappen
    sed 's|^\([[:space:]]*\)build: \.[[:space:]]*$|\1build: ./src|' \
        "$RSRC/compose.yaml" > "$COMPOSE_FILE" || die "compose.yaml fehlgeschlagen"
    ok "Compose-Datei: $COMPOSE_FILE  (Baumaterial: $DOCKER_SRC)"
}

# ---------------------------------------------------------------------------
# OpenWrt (alter Router): procd + dnsmasq-Verkabelung, Update via busybox-cron
# ---------------------------------------------------------------------------
openwrt_detected() { [ -f /etc/openwrt_release ]; }

is_openwrt() { [ "$MODE" = "openwrt" ]; }

openwrt_mem_guard() {
    local kib; kib="$(awk '/^MemAvailable:/{print $2; exit}' /proc/meminfo 2>/dev/null)"
    case "$kib" in ''|*[!0-9]*) kib=0 ;; esac
    local mb=$((kib / 1024))
    if [ "$mb" -lt 32 ]; then
        die "Nur ${mb} MB freier RAM – die dnsblock-Engine (260k+ Domänen) braucht ~40 MB+. Alternative: wenige kompakte Listen wählen oder stärkeres Gerät (extroot)."
    fi
    [ "$mb" -lt 64 ] && warn "Nur ${mb} MB freier RAM – im Setup-Panel auf wenige kompakte Listen beschränken." || true
}

install_files_openwrt() {
    step "OpenWrt-Installation nach $BIN_DIR / $CONF_DIR …"
    mkdir -p "$BIN_DIR" "$CONF_DIR/blocklists" "$DATA_DIR" || die "Verzeichnisse"
    install_bin "$BIN_DIR"
    install_seed_lists

    if [ ! -f "$CONF_DIR/config.toml" ]; then
        cat > "$CONF_DIR/config.toml" <<EOF
# dnsblock – OpenWrt (dnsmasq leitet auf 127.0.0.1#$PORT weiter)
listen_udp = "127.0.0.1:$PORT"
listen_tcp = "127.0.0.1:$PORT"
upstream = "1.1.1.1:53"
upstream_doh = ""
block_mode = "zero"
blocklists = [
    $(conf_blocklists_paths)
]
allowlists = []
admin_listen = "0.0.0.0:$ADMIN_PORT"
uplink_test_servers = ["1.1.1.1:53", "8.8.8.8:53", "9.9.9.9:53"]
fetch_remote = true
remote_refresh_hours = 24
remote_blocklists = [
    "https://adguardteam.github.io/HostlistsRegistry/assets/filter_1.txt",
    "https://raw.githubusercontent.com/StevenBlack/hosts/master/hosts",
]
data_dir = "$DATA_DIR"
root_hints_update_days = 0
EOF
        ok "Konfiguration erzeugt: $CONF_DIR/config.toml"
    else
        ok "Bestehende Konfiguration wird behalten: $CONF_DIR/config.toml"
    fi

    # procd-Dienst (eternales Autostart via rc)
    cat > /etc/init.d/dnsblock <<'INITEOF'
#!/bin/sh /etc/rc.common
# dnsblock – OpenWrt-Dienst (procd), vom Installer erzeugt
START=95
STOP=10
USE_PROCD=1

start_service() {
    procd_open_instance dnsblock
    procd_set_param command /usr/bin/dnsblock /etc/dnsblock/config.toml
    procd_set_param respawn 3600 5 5
    procd_set_param stdout 1
    procd_set_param stderr 1
    procd_close_instance
}
INITEOF
    chmod 755 /etc/init.d/dnsblock || die "init.d-Schreibrechte fehlgeschlagen"
    ok "procd-Dienst: /etc/init.d/dnsblock"

    # Update-System: Script (48 h-Debounce innen) + update.conf + cron
    if fetch_to_file "$URL_RAW/rust-dns-adblocker/packaging/dnsblock-update.sh" "$UPDATE_BIN" 2>/dev/null; then
        chmod 755 "$UPDATE_BIN"
        ok "Update-Script: $UPDATE_BIN"
    else
        warn "dnsblock-update.sh nicht ladbar – Auto-Update bleibt unkonfiguriert."
    fi
    cat > "$CONF_DIR/update.conf" <<EOF
# Update-Einstellungen: täglich via cron (04:00), 48 h-Debounce im Script
AUTO_UPDATE=yes
MODE=openwrt
EOF

    install -m 644 "$0" "$CONF_DIR/install.sh" 2>/dev/null \
        || warn "Installer nach $CONF_DIR/install.sh zu spiegeln fehlgeschlagen." || true
    ok "Installer-Cache: $CONF_DIR/install.sh"
}

openwrt_wire_dnsmasq() {
    if [ "$PORT" = "53" ]; then
        warn "OpenWrt: Port 53 kollidiert mit dnsmasq – Verkabelung übersprungen (dnsblock lauscht direkt :53; dnsmasq-DNS muss dann deaktiviert sein)."
        return 0
    fi
    if ! have uci || ! [ -f /etc/init.d/dnsmasq ]; then
        warn "uci/dnsmasq nicht gefunden – Verkabelung manuell: /etc/config/dhcp → noresolv='1', server='127.0.0.1#$PORT'."
        return 0
    fi
    if uci set dhcp.@dnsmasq[0].noresolv='1' 2>/dev/null \
        && uci add_list dhcp.@dnsmasq[0].server="127.0.0.1#$PORT" 2>/dev/null \
        && uci commit dhcp 2>/dev/null; then
        /etc/init.d/dnsmasq restart >/dev/null 2>&1 || true
        ok "dnsmasq verkabelt: LAN-DNS (:53) → dnsblock (127.0.0.1#$PORT)."
        return 0
    fi
    warn "uci-DHCP-Anpassung scheiterte – manuell: noresolv='1', server='127.0.0.1#$PORT' (dhcp.@dnsmasq[0])."
}

openwrt_cron() {
    grep -q 'dnsblock-update' /etc/crontabs/root 2>/dev/null \
        || printf '0 4 * * * %s\n' "$UPDATE_BIN" >> /etc/crontabs/root 2>/dev/null \
        || warn "Crontab-Eintrag fehlgeschlagen – manuell: siehe README (OpenWrt)."
    /etc/init.d/cron restart >/dev/null 2>&1 || true
    ok "Auto-Update: täglich 04:00 (cron) mit 48 h-Debounce im Script."
}

build_docker() {
    have docker || die "Docker fehlt – bitte Docker Engine installieren (docs.docker.com/engine)."
    if docker compose version >/dev/null 2>&1; then DC="docker compose";
    elif have docker-compose; then DC="docker-compose";
    else die "Docker Compose Plugin fehlt (apt install docker-compose-plugin)."; fi
    step "Baue Container-Image (Multi-Stage, Rust im Container – dauert beim 1. Mal) …"
    ( cd "$DOCKER_DIR" && $DC -f "$COMPOSE_FILE" build ) \
        || die "docker compose build fehlgeschlagen"
    ok "Image gebaut."
}

# ---------------------------------------------------------------------------
# Port-53-Konflikt (systemd-resolved Stub)
# ---------------------------------------------------------------------------
port53_conflict() {
    have ss || return 1
    ss -ltn 2>/dev/null | grep -q '127\.0\.0\.53:53'
}

fix_resolved() {
    mkdir -p /etc/systemd/resolved.conf.d
    cat > /etc/systemd/resolved.conf.d/dnsblock-stub.conf <<'EOF'
# vom dnsblock-Installer: Port 53 für den DNS-Adblocker freigeben
[Resolve]
DNSStubListener=no
EOF
    if [ -e /etc/resolv.conf ] || [ -L /etc/resolv.conf ]; then
        ln -sf /run/systemd/resolve/resolv.conf /etc/resolv.conf
    fi
    systemctl restart systemd-resolved 2>/dev/null || true
    ok "systemd-resolved-Stub deaktiviert (127.0.0.53 bindet :53 nicht mehr)."
}

handle_resolved() {
    [ "$NO_RESOLVED_FIX" = "1" ] && return 0
    if port53_conflict; then
        warn "systemd-resolved belegt Port 53 (Stub 127.0.0.53) – :53 können wir sonst nicht binden."
        if [ "$NONINT" = "1" ]; then
            fix_resolved
        elif ask_yn y "Stub deaktivieren (resolved.conf.d/dnsblock-stub.conf + /etc/resolv.conf → echter Resolver)?"; then
            fix_resolved
        else
            warn "Nicht gefixt – dnsblock kann :53 evtl. nicht binden."
            warn "Manuell: echo 'DNSStubListener=no' >> /etc/systemd/resolved.conf und: ln -sf /run/systemd/resolve/resolv.conf /etc/resolv.conf && systemctl restart systemd-resolved"
        fi
    fi
}

# ---------------------------------------------------------------------------
# Start / Neustart
# ---------------------------------------------------------------------------
start_service() {
    if [ "$MODE" = "docker" ]; then
        have docker || die "Docker fehlt plötzlich?"
        if docker compose version >/dev/null 2>&1; then DC="docker compose"; else DC="docker-compose"; fi
        step "Starte Container …"
        ( cd "$DOCKER_DIR" && $DC -f "$COMPOSE_FILE" up -d ) || die "docker compose up fehlgeschlagen"
        ok "Container läuft."
        return 0
    fi
    if [ "$IS_ROOT" = "1" ] && openwrt_detected && [ -x /etc/init.d/dnsblock ]; then
        step "Aktiviere procd-Dienst …"
        /etc/init.d/dnsblock enable 2>/dev/null || true
        /etc/init.d/dnsblock restart 2>/dev/null || /etc/init.d/dnsblock start \
            || die "procd-Start fehlgeschlagen (logread -e dnsblock | tail)"
        sleep 2
        if /etc/init.d/dnsblock status 2>/dev/null | grep -qi running; then
            ok "procd-Dienst läuft (autostartet nach Boot)."
        else
            warn "procd-Status: $(/etc/init.d/dnsblock status 2>/dev/null) – logread -e dnsblock | tail"
        fi
        return 0
    fi
    if [ "$IS_ROOT" = "1" ] && [ "$SD_UNIT" = "1" ] && [ -f "$UNIT_DIR/dnsblock.service" ]; then
        step "Aktiviere systemd-Dienst …"
        systemctl enable --now dnsblock.service 2>&1 | sed 's/^/   /' || true
        sleep 2
        if systemctl is-active --quiet dnsblock; then
            ok "dnsblock.service läuft."
        else
            warn "Service-Status: $(systemctl is-active dnsblock 2>/dev/null || true) — journalctl -u dnsblock -e"
        fi
        return 0
    fi
    # Benutzer-/No-systemd-Fallback
    step "Starte dnsblock im Hintergrund (PID-Datei: $DATA_DIR/dnsblock.pid) …"
    if [ -f "$DATA_DIR/dnsblock.pid" ]; then
        oldpid="$(cat "$DATA_DIR/dnsblock.pid" 2>/dev/null)"
        kill "$oldpid" 2>/dev/null || true
        sleep 1
    fi
    ( nohup "$BIN_DIR/dnsblock" "$CONF_DIR/config.toml" >> "$DATA_DIR/dnsblock.log" 2>&1 & echo $! > "$DATA_DIR/dnsblock.pid" ) \
        || die "Start fehlgeschlagen (Log: $DATA_DIR/dnsblock.log)"
    sleep 2
    if kill -0 "$(cat "$DATA_DIR/dnsblock.pid" 2>/dev/null)" 2>/dev/null; then
        ok "läuft (PID $(cat "$DATA_DIR/dnsblock.pid" 2>/dev/null))."
    else
        warn "Start vermutlich fehlgeschlagen – Log: $DATA_DIR/dnsblock.log"
    fi
}

restart_service() {
    if [ "$IS_ROOT" = "1" ] && openwrt_detected && [ -x /etc/init.d/dnsblock ]; then
        /etc/init.d/dnsblock restart 2>/dev/null || /etc/init.d/dnsblock start || return 1
        return 0
    fi
    if [ "$MODE" = "docker" ]; then
        if docker compose version >/dev/null 2>&1; then DC="docker compose"; else DC="docker-compose"; fi
        ( cd "$DOCKER_DIR" && $DC -f "$COMPOSE_FILE" up -d ) || return 1
        return 0
    fi
    if [ "$IS_ROOT" = "1" ] && [ "$SD_UNIT" = "1" ] && [ -f "$UNIT_DIR/dnsblock.service" ]; then
        systemctl restart dnsblock.service && return 0
        return 1
    fi
    if [ -f "$DATA_DIR/dnsblock.pid" ]; then
        kill "$(cat "$DATA_DIR/dnsblock.pid" 2>/dev/null)" 2>/dev/null || true
        sleep 1
    fi
    ( nohup "$BIN_DIR/dnsblock" "$CONF_DIR/config.toml" >> "$DATA_DIR/dnsblock.log" 2>&1 & echo $! > "$DATA_DIR/dnsblock.pid" ) || return 1
    sleep 1
    kill -0 "$(cat "$DATA_DIR/dnsblock.pid" 2>/dev/null)" 2>/dev/null
}

# ---------------------------------------------------------------------------
# Selbsttest (DNS block/control + Dashboard-API + Prozess)
# ---------------------------------------------------------------------------
PYPROBE=""
make_pyprobe() {
    have python3 || return 1
    PYPROBE="$TMPROOT/dnsprobe.py"
    cat > "$PYPROBE" <<'PYEOF'
#!/usr/bin/env python3
import socket, struct, sys
def a_query(host, port, name):
    hdr = b'\x7d\x31\x01\x00\x00\x01' + b'\x00'*6
    qn  = b''.join(bytes([len(p)]) + p.encode() for p in name.split('.')) + b'\x00'
    pkt = hdr + qn + b'\x00\x01\x00\x01'
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(4)
    s.sendto(pkt, (host, int(port)))
    d, _ = s.recvfrom(4096)
    rcode = d[3] & 0xF
    if rcode == 3:
        return "NXDOMAIN"
    an = struct.unpack('!H', d[6:8])[0]
    if an == 0:
        return "NOERROR/kein-Answer" if rcode == 0 else "RCODE%d" % rcode
    i = d.find(b'\x00\x00\x01\x00\x01')
    while i != -1:
        rd = d[i+10:i+14]
        if len(rd) == 4:
            return "0.0.0.0" if rd == bytes(4) else '.'.join(str(b) for b in rd)
        i = d.find(b'\x00\x00\x01\x00\x01', i+1)
    return "NOERROR/nodata"
print(a_query(sys.argv[1], sys.argv[2], sys.argv[3]))
PYEOF
    chmod +x "$PYPROBE" 2>/dev/null || true
    return 0
}

dns_answer() { # $1=domain $2=port → 0.0.0.0 | NXDOMAIN | IP | FAIL
    if is_openwrt; then
        # OpenWrt: kein dig/python3 – busybox nslookup über den verkabelten
        # dnsmasq (:53 → dnsblock :$PORT) = echter Ende-zu-Ende-Test.
        # Antwortzeilen heißen "Address 1: 0.0.0.0 name" (Server-Zeile ist
        # "Address:   127.0.0.1:53" ohne laufende Nummer).
        local out ip
        out="$(nslookup "$1" 127.0.0.1 2>&1)"
        if printf '%s\n' "$out" | grep -q '0\.0\.0\.0'; then printf '0.0.0.0'; return 0; fi
        if printf '%s\n' "$out" | grep -qi 'NXDOMAIN\|can.t find'; then printf 'NXDOMAIN'; return 0; fi
        ip="$(printf '%s\n' "$out" \
            | sed -n 's/.*[Aa]ddress[[:space:]][0-9][0-9]*: \([0-9a-fA-F:.]*\).*/\1/p' \
            | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' \
            | grep -v '^127\.' | head -1)"
        if [ -n "$ip" ]; then printf '%s' "$ip"; return 0; fi
        printf 'FAIL'; return 0
    fi
    if have dig; then
        local out st ip
        out="$(dig +time=3 +tries=1 +noall +answer +comments @127.0.0.1 -p "$2" "$1" A 2>&1)"
        st="$(printf '%s\n' "$out" | sed -n 's/.*status: \([A-Z]*\).*/\1/p' | head -1)"
        if printf '%s\n' "$out" | grep -q '0\.0\.0\.0'; then printf '0.0.0.0'; return 0; fi
        if [ "$st" = "NXDOMAIN" ]; then printf 'NXDOMAIN'; return 0; fi
        ip="$(printf '%s\n' "$out" | awk '$4=="A"{print $5; exit}')"
        if [ -n "$ip" ]; then printf '%s' "$ip"; return 0; fi
        if [ -n "$st" ] && [ "$st" != "NOERROR" ]; then printf '%s' "$st"; return 0; fi
        printf 'FAIL'; return 0
    fi
    if [ -n "$PYPROBE" ] && [ -x "$PYPROBE" ]; then
        python3 "$PYPROBE" 127.0.0.1 "$2" "$1" 2>/dev/null || printf 'FAIL'
        return 0
    fi
    printf 'FAIL'
}

is_blocked_answer() {
    case "$1" in 0.0.0.0|NXDOMAIN) return 0 ;; *) return 1 ;; esac
}

self_test() { # $1=dns-port $2=admin-port
    local n_ok=0 n_fail=0 res tmp_stats
    verdict() { # $1=1|0 $2=label $3=detail
        if [ "$1" = "1" ]; then
            ok " ✓ $2 ($3)"; n_ok=$((n_ok+1))
        else
            warn " ✗ $2 ($3)"; n_fail=$((n_fail+1))
        fi
    }

    step "Selbsttest …"

    # 1) Prozess
    if is_openwrt; then
        if [ -x /etc/init.d/dnsblock ] && /etc/init.d/dnsblock status 2>/dev/null | grep -qi running; then
            verdict 1 "procd-Dienst läuft" "/etc/init.d/dnsblock status"
        else
            verdict 0 "procd-Dienst läuft nicht" "logread -e dnsblock | tail"
        fi
    elif [ "$MODE" = "docker" ]; then
        if docker compose version >/dev/null 2>&1; then DC="docker compose"; else DC="docker-compose"; fi
        if ( cd "$DOCKER_DIR" && $DC -f "$COMPOSE_FILE" ps 2>/dev/null | grep -q dnsblock ); then
            verdict 1 "Docker-Container läuft" "compose ps"
        else
            verdict 0 "Docker-Container läuft nicht" "docker compose ps"
        fi
    elif [ "$IS_ROOT" = "1" ] && [ "$SD_UNIT" = "1" ] && [ -f "$UNIT_DIR/dnsblock.service" ]; then
        if systemctl is-active --quiet dnsblock 2>/dev/null; then
            verdict 1 "systemd-Dienst läuft" "systemctl is-active"
        else
            verdict 0 "systemd-Dienst läuft nicht" "journalctl -u dnsblock -e"
        fi
    elif [ -f "$DATA_DIR/dnsblock.pid" ]; then
        local pid; pid="$(cat "$DATA_DIR/dnsblock.pid" 2>/dev/null)"
        if kill -0 "$pid" 2>/dev/null; then
            verdict 1 "Prozess läuft (PID $pid)" "pidfile"
        else
            verdict 0 "Prozess nicht aktiv" "$DATA_DIR/dnsblock.log"
        fi
    fi

    # 2) DNS-Filter: Ads MUSS geblockt werden
    is_openwrt || make_pyprobe
    res="$(dns_answer doubleclick.net "$1")"
    if is_blocked_answer "$res"; then
        verdict 1 "DNS-Filter: doubleclick.net geblockt" "$res"
    else
        verdict 0 "DNS-Filter: doubleclick.net NICHT geblockt" "$res"
    fi

    # 3) Kontrolle: normale Domain MUSS durchgehen
    res="$(dns_answer example.com "$1")"
    case "$res" in
        0.0.0.0|NXDOMAIN|"FAIL"|"NOERROR"*) verdict 0 "Kontrolle: example.com blockiert/nicht erreichbar" "$res" ;;
        "") verdict 0 "Kontrolle: example.com keine Antwort" "leer" ;;
        *) verdict 1 "Kontrolle: example.com normal erreichbar" "$res" ;;
    esac

    # 4) Dashboard-API
    if [ -n "$2" ] && ( have curl || have wget ); then
        tmp_stats="$TMPROOT/stats.json"
        if fetch_to_file "http://127.0.0.1:$2/api/stats" "$tmp_stats" 2>/dev/null && grep -q engine_domains "$tmp_stats"; then
            local n_dom; n_dom="$(sed 's/.*"engine_domains":\([0-9]*\).*/\1/' "$tmp_stats")"
            verdict 1 "Dashboard-API antwortet" "engine_domains=$n_dom"
        else
            verdict 0 "Dashboard-API antwortet nicht" "http://127.0.0.1:$2/api/stats"
        fi
    fi

    if [ "$n_fail" -gt 0 ]; then
        warn "Selbsttest: $n_fail von $((n_ok + n_fail)) Prüfungen fehlgeschlagen."
        if [ "$MODE" = "docker" ]; then
            warn "Logs: ( cd $DOCKER_DIR && docker compose logs dnsblock )"
        elif [ "$IS_ROOT" = "1" ]; then
            warn "Logs: journalctl -u dnsblock -e"
        else
            warn "Logs: $DATA_DIR/dnsblock.log"
        fi
        return 1
    fi
    ok "Selbsttest bestanden ($n_ok Prüfung/en)."
    return 0
}

# ---------------------------------------------------------------------------
# Deinstallation
# ---------------------------------------------------------------------------
do_uninstall() {
    step "Deinstalliere dnsblock …"
    local wipe=1
    if [ "$NONINT" = "0" ] && [ -r /dev/tty ]; then
        if ask_yn y "Sämtliche Daten & Konfiguration ($CONF_DIR, $DATA_DIR) löschen?"; then wipe=1; else wipe=0; fi
    else
        [ "$KEEP_DATA" = "1" ] && wipe=0 || wipe=1
    fi

    if is_openwrt; then
        if [ -x /etc/init.d/dnsblock ]; then
            /etc/init.d/dnsblock stop 2>/dev/null || true
            /etc/init.d/dnsblock disable 2>/dev/null || true
            rm -f /etc/init.d/dnsblock
        fi
        # dnsmasq-Rückrollung (Verkabelung rückgängig)
        if have uci && [ -f /etc/init.d/dnsmasq ]; then
            uci del_list dhcp.@dnsmasq[0].server="127.0.0.1#$PORT" 2>/dev/null || true
            uci set dhcp.@dnsmasq[0].noresolv='0' 2>/dev/null || true
            uci commit dhcp 2>/dev/null || true
            /etc/init.d/dnsmasq restart 2>/dev/null || true
        fi
        if [ -f /etc/crontabs/root ]; then
            sed -i '/dnsblock-update/d' /etc/crontabs/root 2>/dev/null || true
            /etc/init.d/cron restart 2>/dev/null || true
        fi
        rm -f "$BIN_DIR/dnsblock" "$UPDATE_BIN" 2>/dev/null || true
    elif [ "$MODE" = "docker" ]; then
        if [ -f "$COMPOSE_FILE" ]; then
            if docker compose version >/dev/null 2>&1; then DC="docker compose"; else DC="docker-compose"; fi
            if [ "$wipe" = "1" ]; then ( cd "$DOCKER_DIR" && $DC -f "$COMPOSE_FILE" down --volumes --remove-orphans 2>/dev/null ) || true;
            else ( cd "$DOCKER_DIR" && $DC -f "$COMPOSE_FILE" down --remove-orphans 2>/dev/null ) || true; fi
        fi
        rm -rf "$DOCKER_DIR"
    else
        if [ "$IS_ROOT" = "1" ]; then
            systemctl disable --now dnsblock.service dnsblock-update.timer 2>/dev/null || true
            rm -f "$UNIT_DIR/dnsblock.service" "$UNIT_DIR/dnsblock-update.service" "$UNIT_DIR/dnsblock-update.timer"
            systemctl daemon-reload 2>/dev/null || true
        elif [ -f "$DATA_DIR/dnsblock.pid" ]; then
            kill "$(cat "$DATA_DIR/dnsblock.pid" 2>/dev/null)" 2>/dev/null || true
        fi
        rm -f "$BIN_DIR/dnsblock" "$UPDATE_BIN" 2>/dev/null || true
    fi

    if [ "$wipe" = "1" ]; then
        rm -rf "$DATA_DIR" "$CONF_DIR"
        ok "Daten & Konfiguration gelöscht."
    else
        ok "Behalten: $CONF_DIR und $DATA_DIR."
    fi
    ok "Uninstall abgeschlossen."
}

# ---------------------------------------------------------------------------
# Hauptfluss
# ---------------------------------------------------------------------------
main() {
    printf '%sdnsblock – Rust DNS Adblocker%s\n' "$C_B" "$C_0"
    printf '%s\n' "Repository: https://github.com/$REPO"

    install_paths

    if [ "$UNINSTALL" = "1" ]; then do_uninstall; return 0; fi

    choose_mode
    # Modus aus bestehender update.conf übernehmen, falls --update ohne --mode
    if [ "$UPDATE" = "1" ] && [ -z "${ADBLOCK_MODE:-}" ] && [ -f "$CONF_DIR/update.conf" ]; then
        _m="$(awk -F= '$1=="MODE"{gsub(/[[:space:]]|"/,"",$2); print $2; exit}' "$CONF_DIR/update.conf" 2>/dev/null)"
        [ -n "$_m" ] && MODE="$(normalize_mode "$_m")"
    fi

    # Pfade anhand des finalen Modus neu berechnen (install_paths lief früher
    # als choose_mode; u. a. OpenWrt-Rewrite: Port 53→5353, /usr/bin, procd).
    install_paths

    step "Modus: $MODE (root: $IS_ROOT, systemd: $HAVE_SD)"
    acquire_source

    case "$MODE" in
        docker)
            install_files_docker
            handle_resolved
            build_docker
            start_service
            ;;
        openwrt)
            openwrt_mem_guard
            install_files_openwrt
            openwrt_wire_dnsmasq
            openwrt_cron
            if [ "$UPDATE" = "1" ]; then
                restart_service || die "Neustart fehlgeschlagen – logread -e dnsblock | tail"
            else
                start_service
            fi
            ;;
        server|pi|user)
            build_native
            if [ "$IS_ROOT" = "1" ]; then
                install_files_native
                install_units_root
                [ "$SD_UNIT" = "1" ] && handle_resolved
                if [ "$UPDATE" = "1" ]; then
                    restart_service || die "Neustart fehlgeschlagen – vorherige Version blieb evtl. aktiv; journalctl -u dnsblock -e"
                else
                    start_service
                fi
            else
                install_files_user
                if [ "$UPDATE" = "1" ]; then
                    restart_service || die "Neustart fehlgeschlagen – Log: $DATA_DIR/dnsblock.log"
                else
                    start_service
                fi
            fi
            ;;
    esac

    if [ "$SKIP_SELFTEST" = "1" ]; then
        warn "Selbsttest übersprungen (--skip-selftest)."
    else
        self_test "$PORT" "$ADMIN_PORT" || TEST_FAILED=1
    fi

    # Abschluss + Dashboard-Hinweis (Init: Netzwerk/Router/Filterlisten)
    dash_ip="$(python3 - <<'PYEOF' 2>/dev/null || true
import socket
s=socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
try:
    s.connect(("8.8.8.8", 53)); print(s.getsockname()[0])
except Exception:
    pass
PYEOF
)"
    [ -n "${dash_ip:-}" ] || dash_ip="127.0.0.1"
    DASH="http://${dash_ip}:${ADMIN_PORT}"
    printf '\n'
    ok "Installation fertig."
    printf '%s\n' "   Dashboard:                $DASH/"
    printf '%s\n' "   Setup (Netzwerk/Router/Filterlisten):  $DASH/setup"
    printf '%s\n' "   Werbetest-Seite:          $DASH/test"
    if [ "$MODE" = "docker" ]; then
        printf '%s\n' "   Logs:                     docker compose -f $COMPOSE_FILE logs -f"
        printf '%s\n' "   Update (Container):       bash $0 --update --mode docker"
        printf '%s\n' "   Deinstall:                bash $0 --uninstall --mode docker"
    elif is_openwrt; then
        printf '%s\n' "   Logs:                     logread -e dnsblock | tail"
        printf '%s\n' "   Update-Check:             täglich 04:00 (cron), 48 h-Debounce"
        printf '%s\n' "   Manuelles Update:         $UPDATE_BIN --force"
        printf '%s\n' "   Deinstall:                bash $0 --uninstall"
    elif [ "$IS_ROOT" = "1" ]; then
        printf '%s\n' "   Logs:                     journalctl -u dnsblock -f"
        printf '%s\n' "   Auto-Update:              aller 2 Tage (update.conf: $CONF_DIR/update.conf)"
        printf '%s\n' "   Manuelles Update:         dnsblock-update --force"
        printf '%s\n' "   Deinstall:                bash $0 --uninstall"
    else
        printf '%s\n' "   Logs:                     $DATA_DIR/dnsblock.log"
        printf '%s\n' "   Update:                   bash $0 --update"
        printf '%s\n' "   Deinstall:                bash $0 --uninstall"
    fi
    printf '%s\n' "   Router-Guide (DNS umstellen): im Dashboard „Netzwerk & Router“."
    printf '\n'
}

main "$@"
exit "$TEST_FAILED"
