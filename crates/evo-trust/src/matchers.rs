//! Plugin name against prefix patterns in `name_prefixes`.

use evo_plugin_sdk::manifest::TrustClass;
use regex::Regex;

/// Returns true if any pattern matches. Patterns use `*`
/// as a wildcard for one dot-separated segment or as a trailing `.*`.
pub fn name_matches_prefixes(name: &str, name_prefixes: &[String]) -> bool {
    for p in name_prefixes {
        if pattern_match(name, p) {
            return true;
        }
    }
    false
}

fn pattern_match(name: &str, pat: &str) -> bool {
    let p = pat.trim();
    if p == "*" {
        return true;
    }
    if !p.contains('*') {
        return name == p || name.starts_with(&format!("{p}."));
    }
    if let Some(rx) = glob_to_regex(p) {
        rx.is_match(name)
    } else {
        false
    }
}

/// Converts `org.evo.*` to a regex. Escapes dots; `*` → `[^.]+` or
/// trailing `.*` for suffixes.
fn glob_to_regex(pattern: &str) -> Option<Regex> {
    if pattern.ends_with(".*") && !pattern[0..pattern.len() - 2].contains('*') {
        let prefix = &pattern[0..pattern.len() - 2];
        let re = format!(
            "^{}{}$",
            regex::escape(prefix),
            if prefix.is_empty() { ".*" } else { r"(\..+)*" }
        );
        return Regex::new(&re).ok();
    }
    let mut out = String::new();
    out.push('^');
    for c in pattern.chars() {
        if c == '*' {
            out.push_str("[^.]+");
        } else if c == '.' {
            out.push_str(r"\.");
        } else {
            out.push(c);
        }
    }
    out.push('$');
    Regex::new(&out).ok()
}

/// The effective class after key binding; may downgrade the declared
/// class when the key is weaker than the manifest, if `degrade` is set.
pub fn effective_trust_class(
    declared: TrustClass,
    key_max: TrustClass,
    degrade: bool,
) -> Result<TrustClass, (TrustClass, TrustClass)> {
    if declared < key_max {
        // `declared` is more privileged (smaller Ord) than the key can grant
        if degrade {
            return Ok(key_max);
        }
        return Err((declared, key_max));
    }
    Ok(declared)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_org_evo_star() {
        assert!(name_matches_prefixes(
            "org.evo.echo",
            &["org.evo.*".to_string()]
        ));
        assert!(!name_matches_prefixes(
            "org.evil.echo",
            &["org.evo.*".to_string()]
        ));
    }
}
