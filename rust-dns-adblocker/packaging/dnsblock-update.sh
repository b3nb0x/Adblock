#!/usr/bin/env bash
# dnsblock-update – legt eine aktuelle Version nach (Modi: server/pi/docker).
#
# - 2-Tage-Debounce über Stempel-Datei (systemd-Timer ruft täglich, hier wird
#   auf 48 h ausgedünnt – Anforderung „alle 2 Tage prüfen“).
# - Deaktivieren: /etc/dnsblock/update.conf → AUTO_UPDATE=no.
# - Manuell erzwingen: dnsblock-update --force
#
set -u

CONF_DEFAULT="/etc/dnsblock/update.conf"
CONF="${DNSBLOCK_UPDATE_CONF:-$CONF_DEFAULT}"
STATE_DIR="${DNSBLOCK_STATE_DIR:-/var/lib/dnsblock}"
STAMP="$STATE_DIR/.last_update_check"
DEBOUNCE_HOURS="${DNSBLOCK_DEBOUNCE_HOURS:-48}"
INSTALLER="/etc/dnsblock/install.sh"
INSTALLER_URL="https://raw.githubusercontent.com/b3nb0x/Adblock/main/install.sh"

log() { printf '%s\n' "[dnsblock-update] $*"; }

if [ "$(id -u)" != "0" ]; then
    log "Benötigt root (läuft normalerweise über systemd-Timer)."
    exit 1
fi

# ---------- Konfig laden ----------
if [ -f "$CONF" ]; then
    # shellcheck disable=SC1090
    . "$CONF"
fi
AUTO_UPDATE="${AUTO_UPDATE:-yes}"
MODE="${MODE:-server}"

if [ "${1:-}" = "--force" ]; then
    log "Update erzwungen (--force)."
elif [ "$AUTO_UPDATE" != "yes" ]; then
    log "Auto-Update deaktiviert (update.conf: AUTO_UPDATE=$AUTO_UPDATE). Exit."
    exit 0
else
    # ---------- 2-Tage-Debounce ----------
    now="$(date +%s)"
    last="$(stat -c %Y "$STAMP" 2>/dev/null || echo 0)"
    age=$(( now - last ))
    if [ "$age" -lt $(( DEBOUNCE_HOURS * 3600 )) ]; then
        exit 0
    fi
    mkdir -p "$(dirname "$STAMP")" 2>/dev/null || true
    touch "$STAMP" 2>/dev/null || true
fi

log "Starte Update (Modus: $MODE)…"

# ---------- Installer beschaffen (lokal bevorzugt) ----------
TMP="$(mktemp -d /tmp/dnsblock-update.XXXXXX)" || exit 1
trap 'rm -rf "$TMP"' EXIT

if [ -r "$INSTALLER" ]; then
    cp "$INSTALLER" "$TMP/install.sh"
else
    log "Cached-Installer fehlt ($INSTALLER) – lade frisch von GitHub…"
    curl -fsSL "$INSTALLER_URL" -o "$TMP/install.sh" || {
        log "Download fehlgeschlagen (offline?). Abbruch – laufender Dienst bleibt aktiv."
        exit 1
    }
fi

#Installer übernimmt: Binary/Cache tauschen, Konfiguration bleibt erhalten.
exec bash "$TMP/install.sh" --update --mode "$MODE" --non-interactive --yes
