//! Chrome match patterns (`<scheme>://<host>/<path>`, `<all_urls>`), as
//! content scripts use them to pick the documents they run in.

/// Whether `url` is matched by any of `patterns`.
pub fn any_matches(patterns: &[String], url: &str) -> bool {
    let Some(target) = Target::parse(url) else {
        return false;
    };
    patterns.iter().any(|pattern| matches(pattern, &target))
}

/// Pages no extension may script, whatever it was granted: the add-on
/// stores, as Chrome keeps extensions off its Web Store and Firefox off
/// addons.mozilla.org. An extension there could install others.
pub fn is_restricted(url: &str) -> bool {
    let Some(target) = Target::parse(url) else {
        return false;
    };
    target.scheme == "https"
        && match target.host.to_ascii_lowercase().as_str() {
            "chromewebstore.google.com" | "addons.mozilla.org" => true,
            "chrome.google.com" => target.path.starts_with("/webstore"),
            _ => false,
        }
}

struct Target<'a> {
    scheme: &'a str,
    host: &'a str,
    path: &'a str,
}

impl<'a> Target<'a> {
    fn parse(url: &'a str) -> Option<Self> {
        let (scheme, rest) = url.split_once("://")?;
        let (authority, path) = match rest.find('/') {
            Some(slash) => (&rest[..slash], &rest[slash..]),
            None => (rest, "/"),
        };
        // Drop credentials and the port; patterns match the host name only.
        let host = authority.rsplit('@').next().unwrap_or(authority);
        let host = match host.rfind(':') {
            Some(colon) if !host.ends_with(']') => &host[..colon],
            _ => host,
        };
        Some(Self { scheme, host, path })
    }
}

const WEB_SCHEMES: [&str; 2] = ["http", "https"];
const ALL_URLS_SCHEMES: [&str; 5] = ["http", "https", "file", "ws", "wss"];

fn matches(pattern: &str, target: &Target) -> bool {
    if pattern == "<all_urls>" {
        return ALL_URLS_SCHEMES.contains(&target.scheme);
    }
    let Some((scheme, rest)) = pattern.split_once("://") else {
        return false;
    };
    let scheme_matches = match scheme {
        "*" => WEB_SCHEMES.contains(&target.scheme),
        _ => scheme == target.scheme,
    };
    if !scheme_matches {
        return false;
    }
    let (host, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => return false,
    };
    host_matches(host, target.host) && glob(path, target.path)
}

fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern {
        "*" => true,
        _ => match pattern.strip_prefix("*.") {
            Some(domain) => host == domain || host.ends_with(&format!(".{domain}")),
            None => pattern == host,
        },
    }
}

/// `*` matches any run of characters; everything else matches itself.
fn glob(pattern: &str, text: &str) -> bool {
    let (pattern, text) = (pattern.as_bytes(), text.as_bytes());
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&byte| byte == b'*')
}

#[cfg(test)]
mod tests {
    use super::{any_matches, is_restricted};

    #[test]
    fn the_add_on_stores_are_off_limits() {
        assert!(is_restricted("https://chromewebstore.google.com/detail/x/abc"));
        assert!(is_restricted("https://chrome.google.com/webstore/detail/abc"));
        assert!(is_restricted("https://addons.mozilla.org/en-US/firefox/addon/x/"));
        assert!(!is_restricted("https://chrome.google.com/search"));
        assert!(!is_restricted("https://example.com/"));
    }

    fn yes(pattern: &str, url: &str) -> bool {
        any_matches(&[pattern.to_owned()], url)
    }

    #[test]
    fn all_urls_covers_web_and_files_but_not_internal_schemes() {
        assert!(yes("<all_urls>", "https://example.com/"));
        assert!(yes("<all_urls>", "file:///home/a.html"));
        assert!(!yes("<all_urls>", "chrome-extension://abcdefghijklmnopabcdefghijklmnop/a.html"));
        assert!(!yes("<all_urls>", "about:blank"));
    }

    #[test]
    fn star_scheme_is_http_and_https_only() {
        assert!(yes("*://*/*", "http://a.test/x"));
        assert!(yes("*://*/*", "https://a.test/x"));
        assert!(!yes("*://*/*", "file:///x"));
    }

    #[test]
    fn subdomain_wildcards_include_the_domain_itself() {
        assert!(yes("https://*.example.com/*", "https://example.com/"));
        assert!(yes("https://*.example.com/*", "https://a.b.example.com/p"));
        assert!(!yes("https://*.example.com/*", "https://badexample.com/"));
    }

    #[test]
    fn ports_and_credentials_do_not_change_the_host() {
        assert!(yes("https://example.com/*", "https://user:pw@example.com:8443/x"));
        assert!(!yes("https://example.com/*", "https://example.com.evil.test/x"));
    }

    #[test]
    fn paths_are_globs() {
        assert!(yes("https://a.test/docs/*", "https://a.test/docs/x/y?q=1"));
        assert!(!yes("https://a.test/docs/*", "https://a.test/blog/"));
        assert!(yes("https://a.test/*.pdf", "https://a.test/a/b.pdf"));
    }
}
