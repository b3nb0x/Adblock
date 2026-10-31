//! Filter-Engine des Rust-DNS-Adblockers.
//!
//! Unterstützte Listen-Formate (pro Zeile, gemischt möglich):
//! - **hosts**-Format: `0.0.0.0 example.com`
//! - **Domänen-Listen**: eine Domäne pro Zeile (`ads.example.com`)
//! - **AdGuard-Syntax-Subset**:
//!   - `||example.com^` → blockiert example.com und alle Subdomänen
//!   - `@@||example.com^` → Allowlist-Eintrag (gewinnt gegen Block-Regeln)
//!   - Kommentare: `!`, `#`, `[...]`
//!   - kosmetische Regeln (`##...`), Regex- und Option-Regeln (`$...`)
//!     werden bewusst übersprungen (DNS-Ebene braucht nur Domänen).
//!
//! Block- und Allow-Entscheidung über Suffix-Match: ein Eintrag
//! `example.com` blockiert auch `ads.example.com`.

use std::collections::HashSet;

/// Statistik eines `load_str`-Aufrufs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LoadStats {
    /// Neu hinzugefügte Block-Einträge (Duplikate zählen nicht).
    pub blocked_added: usize,
    /// Neu hinzugefügte Allow-Einträge.
    pub allowed_added: usize,
    /// Übersprungene Zeilen (Kommentare, kosmetische Regeln, Müll).
    pub skipped: usize,
}

/// Eine geparte Regel einer Liste.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rule {
    Block(String),
    Allow(String),
}

/// Die Matcher-Engine. Per-Suffix-Lookup, case-insensitive.
#[derive(Debug, Default, Clone)]
pub struct FilterEngine {
    blocked: HashSet<String>,
    allowed: HashSet<String>,
}

impl FilterEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lädt einen Blocklisten-Text in die Engine.
    pub fn load_str(&mut self, content: &str) -> LoadStats {
        let mut stats = LoadStats::default();
        for line in content.lines() {
            match parse_line(line) {
                Some(Rule::Block(d)) => {
                    if self.blocked.insert(d) {
                        stats.blocked_added += 1;
                    }
                }
                Some(Rule::Allow(d)) => {
                    if self.allowed.insert(d) {
                        stats.allowed_added += 1;
                    }
                }
                None => stats.skipped += 1,
            }
        }
        stats
    }

    /// Lädt einen Allowlisten-Text (gleiche Syntax, alles wird Allow).
    pub fn load_allowlist_str(&mut self, content: &str) -> LoadStats {
        let mut stats = LoadStats::default();
        for line in content.lines() {
            let dom = match parse_line(line) {
                Some(Rule::Block(d)) => d,
                Some(Rule::Allow(d)) => d,
                None => {
                    stats.skipped += 1;
                    continue;
                }
            };
            if self.allowed.insert(dom) {
                stats.allowed_added += 1;
            }
        }
        stats
    }

    /// Anzahl geladener Block-Domänen.
    pub fn blocked_count(&self) -> usize {
        self.blocked.len()
    }

    /// Entscheidung für eine Anfrage-Domäne (mit oder ohne trailing dot,
    /// beliebige Groß-/Kleinschreibung). Allow schlägt Block.
    pub fn is_blocked(&self, domain: &str) -> bool {
        let norm = normalize(domain);
        if suffix_match(&self.allowed, &norm) {
            return false;
        }
        suffix_match(&self.blocked, &norm)
    }
}

/// Kleinschreibung, Whitespace und trailing dot entfernen.
fn normalize(domain: &str) -> String {
    domain.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Prüft `name` und alle übergeordneten Suffixe gegen das Set.
fn suffix_match(set: &HashSet<String>, name: &str) -> bool {
    let mut rest = name;
    loop {
        if set.contains(rest) {
            return true;
        }
        match rest.find('.') {
            Some(i) => rest = &rest[i + 1..],
            None => return false,
        }
    }
}

/// Parst eine Listen-Zeile; `None` = überspringen.
fn parse_line(line: &str) -> Option<Rule> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    // Kommentare (AdGuard '!', hosts '#', Sektionen '[...]')
    let first = line.as_bytes()[0];
    if first == b'!' || first == b'#' || first == b'[' {
        return None;
    }
    // AdGuard-Allow: @@||domain^
    if let Some(rest) = line.strip_prefix("@@") {
        return adguard_domain(rest.trim_start_matches("||")).map(Rule::Allow);
    }
    // AdGuard-Block: ||domain^
    if let Some(rest) = line.strip_prefix("||") {
        return adguard_domain(rest).map(Rule::Block);
    }
    // Kosmetische Regeln, Filter-Optionen, Regex, URLs: nichts für DNS
    if line.contains("##") || line.contains('$') || line.contains('/') {
        return None;
    }
    let mut parts = line.split_whitespace();
    let first = parts.next()?;
    // hosts-Format: "0.0.0.0 domain" / "127.0.0.1 domain"
    if first.parse::<std::net::IpAddr>().is_ok() {
        let dom = parts.next()?;
        // typisches Rauschen aus System-hosts-Dateien ignorieren
        if dom.eq_ignore_ascii_case("localhost") || dom.eq_ignore_ascii_case("broadcasthost") {
            return None;
        }
        if dom.contains('.') {
            return Some(Rule::Block(normalize(dom)));
        }
        return None;
    }
    // nackte Domäne
    if first.contains('.') {
        return Some(Rule::Block(normalize(first)));
    }
    None
}

/// Extrahiert die Domäne aus dem Rest einer AdGuard-Regel
/// (`example.com^`, `example.com^$third-party`, `*.example.com^`, ...).
fn adguard_domain(rest: &str) -> Option<String> {
    let dom: &str = rest.split(['^', '/', '$', ',']).next().unwrap_or("");
    let dom = dom.trim_start_matches("*.");
    let dom = dom.trim_start_matches('.');
    if dom.contains('.') {
        Some(normalize(dom))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosts_lines() {
        let mut e = FilterEngine::new();
        let s = e.load_str("0.0.0.0 doubleclick.net\n127.0.0.1 ads.example.com\n");
        assert_eq!(s.blocked_added, 2);
        assert!(e.is_blocked("doubleclick.net"));
        assert!(e.is_blocked("ads.example.com"));
    }

    #[test]
    fn parses_bare_domains_and_normalizes() {
        let mut e = FilterEngine::new();
        e.load_str("Tracker.Example.COM\n");
        assert!(e.is_blocked("tracker.example.com."));
        assert!(e.is_blocked("TRACKER.EXAMPLE.COM"));
    }

    #[test]
    fn parses_adguard_syntax() {
        let mut e = FilterEngine::new();
        let s = e.load_str("! Kommentar\n||googlesyndication.com^\n||ads.x.com^$third-party\n");
        assert_eq!(s.blocked_added, 2);
        assert!(e.is_blocked("googlesyndication.com"));
        assert!(e.is_blocked("page.googlesyndication.com"));
        assert!(e.is_blocked("ads.x.com"));
    }

    #[test]
    fn allow_overrides_block() {
        let mut e = FilterEngine::new();
        e.load_str("||example.com^\n@@||ads.example.com^\n");
        assert!(e.is_blocked("example.com"));
        assert!(e.is_blocked("x.example.com"));
        assert!(!e.is_blocked("ads.example.com"));
        assert!(!e.is_blocked("sub.ads.example.com"));
    }

    #[test]
    fn suffix_matching_blocks_subdomains() {
        let mut e = FilterEngine::new();
        e.load_str("example.com\n");
        assert!(e.is_blocked("example.com"));
        assert!(e.is_blocked("a.example.com"));
        assert!(e.is_blocked("a.b.example.com"));
        assert!(!e.is_blocked("notexample.com"));
        assert!(!e.is_blocked("com"));
    }

    #[test]
    fn skips_garbage_and_cosmetics() {
        let mut e = FilterEngine::new();
        let s = e.load_str("[Adblock Plus 2.0]\n# Kommentar\n##.ad-banner\nexample.com##div\n/foo/^$dnsrewrite\nnur-ein-wort\n");
        assert_eq!(s.skipped, 6);
        assert_eq!(e.blocked_count(), 0);
    }

    #[test]
    fn allowlist_file_grants_pass() {
        let mut e = FilterEngine::new();
        e.load_str("||example.com^\n");
        e.load_allowlist_str("example.com\n");
        assert!(!e.is_blocked("example.com"));
    }

    #[test]
    fn localhost_noise_is_ignored() {
        let mut e = FilterEngine::new();
        e.load_str("127.0.0.1 localhost\n0.0.0.0 broadcasthost\n");
        assert_eq!(e.blocked_count(), 0);
    }
}
