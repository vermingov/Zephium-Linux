//! `declarativeNetRequest` rules as WebKit content-blocker rules.
//!
//! WebKitGTK blocks requests with compiled content-blocker lists, the format
//! Safari uses, so an extension's rules are translated into one such list.
//! The two formats overlap well for blocking, allowing and upgrading; a rule
//! WebKit cannot express (redirects, header changes, request-domain
//! exclusions, regular expressions beyond its subset) is left out and
//! counted, never approximated into one that blocks more than it should.
//!
//! WebKit applies a list top to bottom, a later `ignore-previous-rules`
//! undoing earlier matches. Sorting by priority, with blocks before allows of
//! the same priority, gives Chrome's outcome: the highest priority wins, and
//! on a tie an allow beats a block.

use serde_json::{json, Map, Value};

/// The content types WebKit names for subresources. A rule that names no
/// type applies to all of them; the page itself is a `document` and only
/// a rule that names `main_frame` or `sub_frame` touches it, as in Chrome.
const SUBRESOURCE_TYPES: [&str; 7] = ["image", "style-sheet", "script", "font", "raw", "svg-document", "media"];

/// The list for `rules`, and how many rules it had to leave out.
pub fn translate(rules: &[Value]) -> (String, usize) {
    let mut kept: Vec<(i64, u8, Value)> = Vec::new();
    let mut skipped = 0;
    for rule in rules {
        match translate_rule(rule) {
            Some(translated) => {
                let priority = rule["priority"].as_i64().unwrap_or(1);
                let rank = if translated.iter().any(|rule| rule["action"]["type"] == "ignore-previous-rules") { 1 } else { 0 };
                kept.extend(translated.into_iter().map(|rule| (priority, rank, rule)));
            }
            None => skipped += 1,
        }
    }
    kept.sort_by_key(|(priority, rank, _)| (*priority, *rank));
    let list: Vec<Value> = kept.into_iter().map(|(_, _, rule)| rule).collect();
    (Value::Array(list).to_string(), skipped)
}

/// `isRegexSupported`: whether WebKit can compile `regex`.
pub fn regex_supported(regex: &str) -> bool {
    regex_filter(regex).is_some()
}

/// One DNR rule as one or more WebKit rules (one per request domain).
fn translate_rule(rule: &Value) -> Option<Vec<Value>> {
    let condition = &rule["condition"];
    let action = match rule["action"]["type"].as_str()? {
        "block" => "block",
        "allow" | "allowAllRequests" => "ignore-previous-rules",
        "upgradeScheme" => "make-https",
        _ => return None,
    };
    // Conditions WebKit has no counterpart for.
    if ["excludedRequestDomains", "requestMethods", "excludedRequestMethods", "tabIds", "excludedTabIds", "responseHeaders"]
        .iter()
        .any(|key| !condition[*key].is_null())
    {
        return None;
    }
    if rule["action"]["type"] == "allowAllRequests" {
        return allow_all_requests(condition);
    }

    let mut trigger = Map::new();
    if let Some(types) = resource_types(condition) {
        if types.is_empty() {
            return None;
        }
        trigger.insert("resource-type".into(), json!(types));
    }
    match condition["domainType"].as_str() {
        Some("firstParty") => drop(trigger.insert("load-type".into(), json!(["first-party"]))),
        Some("thirdParty") => drop(trigger.insert("load-type".into(), json!(["third-party"]))),
        Some(_) => return None,
        None => {}
    }
    let initiators = domains(condition, "initiatorDomains", "domains")?;
    let excluded = domains(condition, "excludedInitiatorDomains", "excludedDomains")?;
    if !initiators.is_empty() {
        trigger.insert("if-domain".into(), json!(initiators));
    } else if !excluded.is_empty() {
        trigger.insert("unless-domain".into(), json!(excluded));
    }
    if condition["isUrlFilterCaseSensitive"] == true {
        trigger.insert("url-filter-is-case-sensitive".into(), json!(true));
    }

    let filters = url_filters(condition)?;
    Some(
        filters
            .into_iter()
            .map(|filter| {
                let mut trigger = trigger.clone();
                trigger.insert("url-filter".into(), json!(filter));
                json!({ "trigger": trigger, "action": { "type": action } })
            })
            .collect(),
    )
}

/// `allowAllRequests` lifts every rule for pages of the given domains, the
/// one shape of it WebKit can express.
fn allow_all_requests(condition: &Value) -> Option<Vec<Value>> {
    let only_domains = condition.as_object().is_some_and(|keys| {
        keys.keys().all(|key| ["requestDomains", "resourceTypes"].contains(&key.as_str()))
    });
    let request_domains = domains(condition, "requestDomains", "")?;
    if !only_domains || request_domains.is_empty() {
        return None;
    }
    Some(vec![json!({
        "trigger": { "url-filter": ".*", "if-domain": request_domains },
        "action": { "type": "ignore-previous-rules" },
    })])
}

/// The rule's types in WebKit's names: `None` for every subresource.
fn resource_types(condition: &Value) -> Option<Vec<&'static str>> {
    let names = |key: &str| -> Option<Vec<String>> {
        condition[key].as_array().map(|list| list.iter().filter_map(|name| name.as_str().map(str::to_owned)).collect())
    };
    let mut types: Vec<&'static str> = match names("resourceTypes") {
        Some(list) => list.iter().filter_map(|name| webkit_type(name)).collect(),
        None => {
            let excluded: Vec<&'static str> = names("excludedResourceTypes").unwrap_or_default().iter().filter_map(|name| webkit_type(name)).collect();
            if excluded.is_empty() {
                return None;
            }
            // Excluding a type WebKit merges with others (all of `raw`)
            // excludes them all, so nothing is blocked that should not be.
            SUBRESOURCE_TYPES.iter().copied().filter(|kind| !excluded.contains(kind)).collect()
        }
    };
    types.sort_unstable();
    types.dedup();
    Some(types)
}

fn webkit_type(name: &str) -> Option<&'static str> {
    Some(match name {
        "main_frame" | "sub_frame" => "document",
        "stylesheet" => "style-sheet",
        "script" => "script",
        "image" => "image",
        "font" => "font",
        "media" => "media",
        "object" | "xmlhttprequest" | "ping" | "csp_report" | "websocket" | "webtransport" | "webbundle" | "other" => "raw",
        _ => return None,
    })
}

/// A domain list in WebKit's form, `*` taking in the subdomains as DNR
/// does. `None` when a domain is not plain ASCII.
fn domains(condition: &Value, key: &str, legacy: &str) -> Option<Vec<String>> {
    let list = condition[key].as_array().or_else(|| condition.get(legacy).and_then(Value::as_array));
    let Some(list) = list else {
        return Some(Vec::new());
    };
    list.iter()
        .map(|domain| {
            let domain = domain.as_str()?.to_ascii_lowercase();
            let plain = !domain.is_empty() && domain.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte));
            plain.then(|| format!("*{domain}"))
        })
        .collect()
}

/// The URL patterns for the rule: one per request domain, or one.
fn url_filters(condition: &Value) -> Option<Vec<String>> {
    let pattern = match (condition["urlFilter"].as_str(), condition["regexFilter"].as_str()) {
        (Some(_), Some(_)) => return None,
        (Some(filter), None) => Some(url_filter(filter)?),
        (None, Some(regex)) => Some(regex_filter(regex)?),
        (None, None) => None,
    };
    let request_domains = domains(condition, "requestDomains", "")?;
    if request_domains.is_empty() {
        return Some(vec![pattern.unwrap_or_else(|| ".*".to_owned())]);
    }
    // A request domain anchors the pattern to the host, which only works
    // for a pattern that is not anchored itself.
    let rest = match &pattern {
        Some(pattern) if pattern.starts_with('^') => return None,
        Some(pattern) => format!(".*{pattern}"),
        None => String::new(),
    };
    Some(
        request_domains
            .iter()
            .map(|domain| format!("^[^:]+://([^/]*\\.)?{}[/:]{rest}", escape(domain.trim_start_matches('*'))))
            .collect(),
    )
}

/// DNR's `urlFilter` syntax as a WebKit regular expression.
fn url_filter(filter: &str) -> Option<String> {
    if !filter.is_ascii() || filter.is_empty() {
        return None;
    }
    let (start, body) = if let Some(rest) = filter.strip_prefix("||") {
        ("^[^:]+://([^/]*\\.)?", rest)
    } else if let Some(rest) = filter.strip_prefix('|') {
        ("^", rest)
    } else {
        ("", filter)
    };
    let (body, end) = match body.strip_suffix('|') {
        Some(rest) => (rest, "$"),
        None => (body, ""),
    };
    let mut pattern = String::from(start);
    for character in body.chars() {
        match character {
            '*' => pattern.push_str(".*"),
            // A separator: anything but a letter, a digit or `_-.%`. The end
            // of the URL is a separator too, which WebKit cannot say; the
            // host always ends in `/`, `:` or `?`, so host patterns lose
            // nothing.
            '^' => pattern.push_str("[^a-zA-Z0-9_.%-]"),
            other => pattern.push_str(&escape(&other.to_string())),
        }
    }
    pattern.push_str(end);
    Some(pattern)
}

/// A `regexFilter` WebKit can compile: its subset has no alternation, no
/// counted repetition, no shorthand classes and no lookaround.
fn regex_filter(regex: &str) -> Option<String> {
    let mut in_class = false;
    let mut escaped = false;
    for character in regex.chars() {
        if !character.is_ascii() {
            return None;
        }
        if escaped {
            if character.is_ascii_alphanumeric() {
                return None;
            }
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '[' => in_class = true,
            ']' => in_class = false,
            '|' | '{' | '}' if !in_class => return None,
            _ => {}
        }
    }
    (!regex.contains("(?")).then(|| regex.to_owned())
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if ".+?()[]{}\\$|^*".contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(rules: Value) -> (Vec<Value>, usize) {
        let (text, skipped) = translate(rules.as_array().unwrap());
        (serde_json::from_str(&text).unwrap(), skipped)
    }

    #[test]
    fn a_domain_anchored_block_becomes_a_host_pattern() {
        let (rules, skipped) = list(json!([{ "id": 1, "action": { "type": "block" }, "condition": { "urlFilter": "||ads.example.com^" } }]));
        assert_eq!(skipped, 0);
        assert_eq!(rules[0]["trigger"]["url-filter"], "^[^:]+://([^/]*\\.)?ads\\.example\\.com[^a-zA-Z0-9_.%-]");
        assert_eq!(rules[0]["action"]["type"], "block");
        assert!(rules[0]["trigger"]["resource-type"].is_null(), "every subresource, not the page");
    }

    #[test]
    fn allows_follow_blocks_of_the_same_priority_and_precede_higher_blocks() {
        let (rules, _) = list(json!([
            { "id": 1, "priority": 2, "action": { "type": "block" }, "condition": { "urlFilter": "a" } },
            { "id": 2, "action": { "type": "allow" }, "condition": { "urlFilter": "b" } },
            { "id": 3, "action": { "type": "block" }, "condition": { "urlFilter": "c" } },
        ]));
        let order: Vec<&str> = rules.iter().map(|rule| rule["trigger"]["url-filter"].as_str().unwrap()).collect();
        assert_eq!(order, ["c", "b", "a"]);
    }

    #[test]
    fn types_domains_and_party_carry_over() {
        let (rules, _) = list(json!([{ "id": 1, "action": { "type": "block" }, "condition": {
            "urlFilter": "/track", "resourceTypes": ["script", "xmlhttprequest", "ping"],
            "initiatorDomains": ["news.example"], "domainType": "thirdParty" } }]));
        let trigger = &rules[0]["trigger"];
        assert_eq!(trigger["resource-type"], json!(["raw", "script"]));
        assert_eq!(trigger["if-domain"], json!(["*news.example"]));
        assert_eq!(trigger["load-type"], json!(["third-party"]));
        assert_eq!(trigger["url-filter"], "/track");
    }

    #[test]
    fn request_domains_become_one_host_rule_each() {
        let (rules, _) = list(json!([{ "id": 1, "action": { "type": "block" }, "condition": { "requestDomains": ["a.test", "b.test"] } }]));
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[1]["trigger"]["url-filter"], "^[^:]+://([^/]*\\.)?b\\.test[/:]");
    }

    #[test]
    fn what_webkit_cannot_express_is_left_out() {
        let (rules, skipped) = list(json!([
            { "id": 1, "action": { "type": "redirect", "redirect": { "url": "https://x.test" } }, "condition": { "urlFilter": "a" } },
            { "id": 2, "action": { "type": "block" }, "condition": { "regexFilter": "^https://(a|b)\\.test/" } },
            { "id": 3, "action": { "type": "block" }, "condition": { "regexFilter": "\\d+" } },
            { "id": 4, "action": { "type": "block" }, "condition": { "urlFilter": "a", "excludedRequestDomains": ["x.test"] } },
            { "id": 5, "action": { "type": "block" }, "condition": { "regexFilter": "^https://[a-z]+\\.test/ads" } },
        ]));
        assert_eq!(skipped, 4);
        assert_eq!(rules[0]["trigger"]["url-filter"], "^https://[a-z]+\\.test/ads");
    }

    #[test]
    fn allow_all_requests_lifts_rules_for_listed_sites() {
        let (rules, skipped) = list(json!([{ "id": 1, "action": { "type": "allowAllRequests" }, "condition": {
            "requestDomains": ["shop.test"], "resourceTypes": ["main_frame"] } }]));
        assert_eq!(skipped, 0);
        assert_eq!(rules[0]["trigger"], json!({ "url-filter": ".*", "if-domain": ["*shop.test"] }));
    }

    #[test]
    fn plain_patterns_escape_regex_characters() {
        assert_eq!(url_filter("|https://x.test/a?b=1|").unwrap(), "^https://x\\.test/a\\?b=1$");
        assert_eq!(url_filter("/ads/*.gif").unwrap(), "/ads/.*\\.gif");
    }
}
