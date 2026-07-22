//! Request-level tracker/ad blocking with Adblock-style filter rules.
//!
//! This runs in the shell's resource loader — the single point every subresource
//! request passes through — so a blocked request is never sent at all, rather than
//! being hidden after the fact.
//!
//! Only subresources are filtered. A page the user explicitly navigates to is never
//! blocked; that would be the browser overriding the user.
//!
//! Rules come from a built-in list plus an optional `filters.txt` in the profile
//! directory, so a user can drop in EasyList/EasyPrivacy without a rebuild.
//!
//! ponytail: supports `||domain^`, `|prefix`, `suffix|`, plain substrings, `*`
//! wildcards, `@@` exceptions, and `!`/`[` comments. Filter *options* (`$script`,
//! `$third-party`) are parsed off and ignored, and cosmetic `##` rules are skipped
//! entirely — matching is URL-only.

use std::sync::OnceLock;

/// How a rule's pattern is anchored against the URL.
#[derive(Debug, PartialEq, Eq)]
enum Anchor {
    /// `||example.com^` — the host, or any subdomain of it.
    Domain,
    /// `|https://x` — the URL must start with the pattern.
    Start,
    /// `x|` — the URL must end with the pattern.
    End,
    /// Anywhere in the URL.
    Substring,
}

#[derive(Debug)]
struct Rule {
    anchor: Anchor,
    /// For `||domain^` rules: the host to match, separate from any path pattern.
    domain: Option<String>,
    /// Pattern split on `*`; every part must appear in order.
    parts: Vec<String>,
    /// `@@` rules allow a request even if a blocking rule matched.
    exception: bool,
}

pub struct Blocker {
    rules: Vec<Rule>,
}

/// The default list: well-known advertising, analytics, and session-recording hosts.
const BUILT_IN: &str = "\
||doubleclick.net^
||googlesyndication.com^
||googleadservices.com^
||google-analytics.com^
||googletagmanager.com^
||googletagservices.com^
||adservice.google.com^
||connect.facebook.net^
||ads-twitter.com^
||analytics.tiktok.com^
||adnxs.com^
||criteo.com^
||criteo.net^
||taboola.com^
||outbrain.com^
||amazon-adsystem.com^
||adsrvr.org^
||rubiconproject.com^
||pubmatic.com^
||openx.net^
||casalemedia.com^
||smartadserver.com^
||adform.net^
||media.net^
||moatads.com^
||serving-sys.com^
||sharethrough.com^
||teads.tv^
||yieldmo.com^
||revcontent.com^
||mgid.com^
||propellerads.com^
||popads.net^
||scorecardresearch.com^
||quantserve.com^
||hotjar.com^
||mixpanel.com^
||amplitude.com^
||fullstory.com^
||mouseflow.com^
||crazyegg.com^
||chartbeat.com^
||parsely.com^
||clarity.ms^
||bluekai.com^
||demdex.net^
||omtrdc.net^
||everesttech.net^
||branch.io^
||appsflyer.com^
||adjust.com^
";

impl Blocker {
    /// Parse filter rules, one per line. Unparseable lines are skipped.
    pub fn from_rules(text: &str) -> Blocker {
        Blocker { rules: text.lines().filter_map(parse_rule).collect() }
    }

    /// True if this URL should not be requested.
    pub fn blocks(&self, url: &str) -> bool {
        let host = host_of(url).unwrap_or_default();
        let matches = |r: &&Rule| rule_matches(r, url, &host);
        // An exception anywhere in the list wins, as in Adblock.
        if self.rules.iter().filter(|r| r.exception).any(|r| matches(&r)) {
            return false;
        }
        self.rules.iter().filter(|r| !r.exception).any(|r| matches(&r))
    }

    #[cfg(test)]
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

/// The process-wide blocker: built-in rules plus the user's `filters.txt`.
fn shared() -> &'static Blocker {
    static BLOCKER: OnceLock<Blocker> = OnceLock::new();
    BLOCKER.get_or_init(|| {
        let mut text = BUILT_IN.to_string();
        if let Some(dir) = crate::storage::profile_dir() {
            if let Ok(user) = std::fs::read_to_string(dir.join("filters.txt")) {
                text.push('\n');
                text.push_str(&user);
            }
        }
        Blocker::from_rules(&text)
    })
}

/// True if this URL points at a known tracker/ad host.
///
/// Honours the tracker-blocking preference, so turning it off in `zero://settings`
/// actually lets the request through rather than only hiding the count.
pub fn is_blocked(url: &str) -> bool {
    crate::settings::current().blocking && shared().blocks(url)
}

fn parse_rule(line: &str) -> Option<Rule> {
    let line = line.trim();
    // Comments, section headers, and cosmetic (element-hiding) rules.
    if line.is_empty()
        || line.starts_with('!')
        || line.starts_with('[')
        || line.contains("##")
        || line.contains("#@#")
    {
        return None;
    }
    let (line, exception) = match line.strip_prefix("@@") {
        Some(rest) => (rest, true),
        None => (line, false),
    };
    // Filter options are matched on request type, which we don't model.
    let pattern = line.split('$').next().unwrap_or("").trim();
    if pattern.is_empty() {
        return None;
    }

    let (anchor, body) = if let Some(rest) = pattern.strip_prefix("||") {
        (Anchor::Domain, rest)
    } else if let Some(rest) = pattern.strip_prefix('|') {
        (Anchor::Start, rest)
    } else if let Some(rest) = pattern.strip_suffix('|') {
        (Anchor::End, rest)
    } else {
        (Anchor::Substring, pattern)
    };

    // A domain rule's host ends at the first separator; whatever follows is a
    // path pattern matched against the rest of the URL.
    let (domain, body) = if anchor == Anchor::Domain {
        let end = body.find(['/', '^', '*']).unwrap_or(body.len());
        let (host, rest) = body.split_at(end);
        if host.is_empty() {
            return None;
        }
        (Some(host.to_ascii_lowercase()), rest)
    } else {
        (None, body)
    };

    // `^` is a separator placeholder; treat it as a wildcard boundary.
    let parts: Vec<String> =
        body.replace('^', "*").split('*').filter(|p| !p.is_empty()).map(str::to_string).collect();
    // A bare domain rule needs no path parts, but every other kind does.
    if parts.is_empty() && domain.is_none() {
        return None;
    }
    Some(Rule { anchor, domain, parts, exception })
}

fn rule_matches(rule: &Rule, url: &str, host: &str) -> bool {
    match rule.anchor {
        Anchor::Domain => {
            let Some(domain) = &rule.domain else { return false };
            // The host itself, or any subdomain of it.
            let host_ok = host == domain || host.ends_with(&format!(".{domain}"));
            host_ok && contains_in_order(url, &rule.parts)
        }
        Anchor::Start => url.starts_with(rule.parts[0].as_str())
            && contains_in_order(url, &rule.parts[1..]),
        Anchor::End => {
            let last = rule.parts.last().expect("checked non-empty");
            url.ends_with(last.as_str()) && contains_in_order(url, &rule.parts)
        }
        Anchor::Substring => contains_in_order(url, &rule.parts),
    }
}

/// Every part appears in `text`, in order and without overlapping.
fn contains_in_order(text: &str, parts: &[String]) -> bool {
    let mut rest = text;
    for part in parts {
        match rest.find(part.as_str()) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    true
}

/// Extract the host from an absolute URL (no scheme/port/userinfo).
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1)?;
    let host = after_scheme.split('/').next()?;
    let host = host.rsplit('@').next()?;
    Some(host.split(':').next()?.trim_end_matches('.').to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_trackers_and_subdomains_only() {
        assert!(is_blocked("https://www.google-analytics.com/collect?v=1"));
        assert!(is_blocked("https://stats.g.doubleclick.net/x.gif"));
        assert!(is_blocked("http://pagead2.googlesyndication.com:8080/ads"));
        // Not a tracker, and must not match by mere substring.
        assert!(!is_blocked("https://example.com/img.png"));
        assert!(!is_blocked("https://notdoubleclick.net/a.png"));
        assert!(!is_blocked("https://en.wikipedia.org/logo.png"));
        assert!(!is_blocked("data:image/png;base64,AAAA"));
    }

    #[test]
    fn exception_rules_override_blocks() {
        let blocker = Blocker::from_rules("||ads.example.com^\n@@||ads.example.com/allowed^");
        assert!(blocker.blocks("https://ads.example.com/banner.js"));
        assert!(!blocker.blocks("https://ads.example.com/allowed/pixel.gif"));
    }

    #[test]
    fn anchors_and_wildcards() {
        let blocker = Blocker::from_rules(
            "|https://start.example\n\
             /track/*.gif\n\
             endswith.js|",
        );
        assert!(blocker.blocks("https://start.example/x"));
        // Start-anchored means the URL must begin with it.
        assert!(!blocker.blocks("https://cdn.com/https://start.example"));

        // Wildcard parts must appear in order.
        assert!(blocker.blocks("https://a.com/track/abc.gif"));
        assert!(!blocker.blocks("https://a.com/abc.gif/track/"));

        assert!(blocker.blocks("https://a.com/endswith.js"));
        assert!(!blocker.blocks("https://a.com/endswith.js?v=1"));
    }

    #[test]
    fn comments_and_cosmetic_rules_are_skipped() {
        let blocker = Blocker::from_rules(
            "! a comment\n[Adblock Plus 2.0]\nexample.com##.ad-banner\n\n||real.com^",
        );
        assert_eq!(blocker.rule_count(), 1);
        assert!(blocker.blocks("https://real.com/x"));
    }

    #[test]
    fn filter_options_are_stripped_not_fatal() {
        let blocker = Blocker::from_rules("||ads.net^$script,third-party");
        assert_eq!(blocker.rule_count(), 1);
        assert!(blocker.blocks("https://ads.net/a.js"));
    }
}
