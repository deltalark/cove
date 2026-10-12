//! Which hosts a VM may reach, and which secrets go where.

use crate::model::Secret;
use std::net::IpAddr;

/// Model providers and the sign-in hosts their CLIs use.
pub const AGENTS: &[&str] = &[
    "api.anthropic.com",
    "*.anthropic.com",
    "claude.ai",
    "*.claude.ai",
    "api.openai.com",
    "*.openai.com",
    "chatgpt.com",
    "*.chatgpt.com",
    "openrouter.ai",
    "generativelanguage.googleapis.com",
    "oauth2.googleapis.com",
];

/// What day-to-day development needs: git hosts, package registries and the Debian
/// mirrors, on top of the agents.
pub const DEV: &[&str] = &[
    "github.com",
    "*.github.com",
    "*.githubusercontent.com",
    "gitlab.com",
    "*.gitlab.com",
    "bitbucket.org",
    "registry.npmjs.org",
    "*.npmjs.org",
    "registry.yarnpkg.com",
    "nodejs.org",
    "pypi.org",
    "files.pythonhosted.org",
    "crates.io",
    "*.crates.io",
    "static.rust-lang.org",
    "sh.rustup.rs",
    "proxy.golang.org",
    "sum.golang.org",
    "storage.googleapis.com",
    "rubygems.org",
    "repo.maven.apache.org",
    "repo1.maven.org",
    "deb.debian.org",
    "security.debian.org",
    "deb.nodesource.com",
    "registry-1.docker.io",
    "auth.docker.io",
    "production.cloudflare.docker.com",
];

/// The presets `allow` entries can name.
pub const PRESETS: &[(&str, &str)] = &[
    ("@agents", "model providers and their sign-in hosts"),
    (
        "@dev",
        "@agents, plus git hosts, package registries and Debian mirrors",
    ),
];

/// Usual hosts for well-known secret names, used when a secret names none.
pub fn default_hosts(name: &str) -> &'static [&'static str] {
    match name {
        "ANTHROPIC_API_KEY" | "CLAUDE_CODE_OAUTH_TOKEN" | "ANTHROPIC_AUTH_TOKEN" => {
            &["api.anthropic.com"]
        }
        "OPENAI_API_KEY" | "CODEX_API_KEY" => &["api.openai.com"],
        "OPENROUTER_API_KEY" => &["openrouter.ai"],
        "GEMINI_API_KEY" | "GOOGLE_API_KEY" => &["generativelanguage.googleapis.com"],
        "GH_TOKEN" | "GITHUB_TOKEN" => &["github.com", "api.github.com"],
        "GITLAB_TOKEN" => &["gitlab.com"],
        _ => &[],
    }
}

/// Lowercase, trimmed, and checked: a host, `*.host`, `*`, or a known preset.
pub fn normalize_rule(rule: &str) -> Result<String, String> {
    let rule = rule.trim().trim_end_matches('.').to_ascii_lowercase();
    if rule == "*" || PRESETS.iter().any(|(p, _)| *p == rule) {
        return Ok(rule);
    }
    if rule.starts_with('@') {
        let known: Vec<&str> = PRESETS.iter().map(|p| p.0).collect();
        return Err(format!(
            "unknown preset {rule:?} (known: {})",
            known.join(", ")
        ));
    }
    let host = rule.strip_prefix("*.").unwrap_or(&rule);
    let ok = !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == ':')
        && !host.contains("..");
    if !ok {
        return Err(format!(
            "{rule:?} is not a host name, *.domain, * or a preset"
        ));
    }
    Ok(rule)
}

fn expand(rules: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for rule in rules {
        match rule.as_str() {
            "@agents" => out.extend(AGENTS.iter().map(|s| s.to_string())),
            "@dev" => {
                out.extend(AGENTS.iter().map(|s| s.to_string()));
                out.extend(DEV.iter().map(|s| s.to_string()));
            }
            other => out.push(other.to_string()),
        }
    }
    out
}

/// How a host matched the allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allowed {
    /// Listed by name (or as an IP address).
    Exactly,
    /// Through `*` or `*.domain`.
    ByWildcard,
}

/// One VM's rules, ready to check against.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Expanded rules; `None` lets everything through (network `full`).
    rules: Option<Vec<String>>,
    secrets: Vec<Secret>,
}

impl Policy {
    /// `enforce` is false for a VM with its own network device, where the proxy only
    /// brokers secrets.
    pub fn new(allow: &[String], secrets: &[Secret], enforce: bool) -> Policy {
        let rules = enforce.then(|| {
            let mut rules = expand(allow);
            for s in secrets {
                rules.extend(s.hosts.iter().cloned());
            }
            rules
        });
        Policy {
            rules,
            secrets: secrets.to_vec(),
        }
    }

    pub fn check(&self, host: &str) -> Option<Allowed> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let Some(rules) = &self.rules else {
            return Some(Allowed::ByWildcard);
        };
        let mut best = None;
        for rule in rules {
            if *rule == host {
                return Some(Allowed::Exactly);
            }
            if rule == "*" {
                best = Some(Allowed::ByWildcard);
            } else if let Some(domain) = rule.strip_prefix("*.") {
                if host.ends_with(&format!(".{domain}")) {
                    best = Some(Allowed::ByWildcard);
                }
            }
        }
        best
    }

    /// Secrets that may be sent to `host`.
    pub fn secrets_for(&self, host: &str) -> Vec<&Secret> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.secrets
            .iter()
            .filter(|s| s.hosts.iter().any(|h| host_matches(h, &host)))
            .collect()
    }
}

fn host_matches(rule: &str, host: &str) -> bool {
    match rule.strip_prefix("*.") {
        Some(domain) => host.ends_with(&format!(".{domain}")),
        None => rule == host,
    }
}

/// Addresses the proxy never connects to: the host itself, link-local (including the
/// cloud metadata service) and the like.
pub fn forbidden(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return forbidden(IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Private networks (a LAN, a VPC, other VMs). Reached only for hosts allowed by
/// name, so `*` doesn't open the whole network the server sits on.
pub fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private() || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return private(IpAddr::V4(v4));
            }
            (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// Make the placeholder a VM sees for a secret. It keeps the value's short public
/// prefix (`sk-ant-api03-`, `ghp_`), since some CLIs check it before sending.
pub fn make_placeholder(value: &str) -> String {
    let head: String = value.chars().take(16).collect();
    let prefix = head.rfind(['-', '_']).map(|i| &head[..=i]).unwrap_or("");
    format!("{prefix}cove{}", crate::random_hex(16))
}

pub fn valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(name: &str, hosts: &[&str]) -> Secret {
        Secret {
            name: name.into(),
            value: "v".into(),
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            placeholder: "p".into(),
        }
    }

    #[test]
    fn rules_match_hosts_and_subdomains() {
        let p = Policy::new(&["example.com".into(), "*.npmjs.org".into()], &[], true);
        assert_eq!(p.check("example.com"), Some(Allowed::Exactly));
        assert_eq!(p.check("EXAMPLE.com."), Some(Allowed::Exactly));
        assert_eq!(p.check("api.example.com"), None);
        assert_eq!(p.check("registry.npmjs.org"), Some(Allowed::ByWildcard));
        assert_eq!(p.check("npmjs.org"), None);
        assert_eq!(p.check("evilnpmjs.org"), None);
        let open = Policy::new(&["*".into()], &[], true);
        assert_eq!(open.check("anything.io"), Some(Allowed::ByWildcard));
        let unenforced = Policy::new(&[], &[], false);
        assert!(unenforced.check("anything.io").is_some());
    }

    #[test]
    fn presets_and_secret_hosts_are_allowed() {
        let p = Policy::new(
            &["@dev".into()],
            &[secret("X", &["internal.example.com"])],
            true,
        );
        assert!(p.check("api.anthropic.com").is_some());
        assert!(p.check("registry.npmjs.org").is_some());
        assert_eq!(p.check("internal.example.com"), Some(Allowed::Exactly));
        assert!(p.check("example.org").is_none());
        let agents = Policy::new(&["@agents".into()], &[], true);
        assert!(agents.check("api.openai.com").is_some());
        assert!(agents.check("github.com").is_none());
    }

    #[test]
    fn secrets_go_only_to_their_hosts() {
        let p = Policy::new(
            &["*".into()],
            &[
                secret("A", &["api.anthropic.com"]),
                secret("G", &["*.github.com"]),
            ],
            true,
        );
        let names = |h| {
            p.secrets_for(h)
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("api.anthropic.com"), vec!["A"]);
        assert_eq!(names("api.github.com"), vec!["G"]);
        assert!(names("github.com").is_empty());
        assert!(names("evil.com").is_empty());
    }

    #[test]
    fn rules_are_checked() {
        assert_eq!(normalize_rule(" GitHub.com ").unwrap(), "github.com");
        assert_eq!(normalize_rule("*.Example.com").unwrap(), "*.example.com");
        assert_eq!(normalize_rule("@dev").unwrap(), "@dev");
        assert!(normalize_rule("@nope").is_err());
        assert!(normalize_rule("http://x.com").is_err());
        assert!(normalize_rule("a..b").is_err());
    }

    #[test]
    fn addresses_off_limits() {
        for ip in [
            "127.0.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(forbidden(ip.parse().unwrap()), "{ip}");
        }
        assert!(!forbidden("93.184.216.34".parse().unwrap()));
        for ip in [
            "10.0.0.1",
            "192.168.1.5",
            "172.16.0.1",
            "100.64.0.1",
            "fd00::1",
        ] {
            assert!(private(ip.parse().unwrap()), "{ip}");
        }
        assert!(!private("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn placeholders_keep_the_public_prefix() {
        let p = make_placeholder("sk-ant-api03-secretsecretsecret");
        assert!(p.starts_with("sk-ant-api03-cove"), "{p}");
        assert!(!p.contains("secret"));
        assert!(make_placeholder("ghp_abcdef").starts_with("ghp_cove"));
        assert!(make_placeholder("plain").starts_with("cove"));
        assert!(valid_secret_name("GH_TOKEN"));
        assert!(!valid_secret_name("gh-token"));
        assert!(!valid_secret_name("1X"));
    }
}
