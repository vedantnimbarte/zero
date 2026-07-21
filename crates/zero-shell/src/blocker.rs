//! Request-level tracker/ad blocking.
//!
//! This runs in the shell's resource loader — the single point every subresource
//! request passes through — so a blocked request is never sent at all, rather than
//! being hidden after the fact.
//!
//! Only subresources are filtered. A page the user explicitly navigates to is never
//! blocked; that would be the browser overriding the user.
//!
//! ponytail: a curated domain list with suffix matching, not real filter-list syntax.
//! EasyList/EasyPrivacy parsing (wildcards, element hiding, per-site exceptions) is the
//! upgrade — see docs/04-SECURITY-PRIVACY.md §5.2.

/// Well-known advertising, analytics, and session-recording hosts.
const BLOCKLIST: &[&str] = &[
    // Google ads / analytics
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "google-analytics.com",
    "googletagmanager.com",
    "googletagservices.com",
    "adservice.google.com",
    // Social trackers
    "connect.facebook.net",
    "ads-twitter.com",
    "analytics.tiktok.com",
    // Ad exchanges / networks
    "adnxs.com",
    "criteo.com",
    "criteo.net",
    "taboola.com",
    "outbrain.com",
    "amazon-adsystem.com",
    "adsrvr.org",
    "rubiconproject.com",
    "pubmatic.com",
    "openx.net",
    "casalemedia.com",
    "smartadserver.com",
    "adform.net",
    "media.net",
    "moatads.com",
    "serving-sys.com",
    "sharethrough.com",
    "teads.tv",
    "yieldmo.com",
    "revcontent.com",
    "mgid.com",
    "propellerads.com",
    "popads.net",
    // Analytics / session recording
    "scorecardresearch.com",
    "quantserve.com",
    "hotjar.com",
    "mixpanel.com",
    "amplitude.com",
    "fullstory.com",
    "mouseflow.com",
    "crazyegg.com",
    "chartbeat.com",
    "parsely.com",
    "clarity.ms",
    "bluekai.com",
    "demdex.net",
    "omtrdc.net",
    "everesttech.net",
    // Attribution
    "branch.io",
    "appsflyer.com",
    "adjust.com",
];

/// Extract the host from an absolute URL (no scheme/port/userinfo).
fn host_of(url: &str) -> Option<&str> {
    let after_scheme = url.split("://").nth(1)?;
    let host = after_scheme.split('/').next()?;
    let host = host.rsplit('@').next()?; // strip any userinfo
    Some(host.split(':').next()?) // strip port
}

/// True if this URL points at a known tracker/ad host.
pub fn is_blocked(url: &str) -> bool {
    let host = match host_of(url) {
        Some(h) => h.trim_end_matches('.').to_ascii_lowercase(),
        None => return false,
    };
    BLOCKLIST
        .iter()
        .any(|bad| host == *bad || host.ends_with(&format!(".{bad}")))
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
}
