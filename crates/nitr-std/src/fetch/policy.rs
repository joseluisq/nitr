//! Outbound-request policy for the `fetch` builtin: SSRF protection.
//!
//! Once scripts can `fetch(...)`, the server is a potential SSRF primitive,
//! so by default requests to loopback, private, link-local, and other
//! special-purpose addresses are refused — the target host is resolved and
//! *all* of its addresses checked, and every redirect hop is re-validated
//! against the same policy.

use std::net::IpAddr;

use mlua::ExternalResult as _;
use reqwest::Url;

/// Policy and limits applied to every outbound `fetch` request.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// When set, only these exact host names may be fetched (compared
    /// case-insensitively; applies to every redirect hop).
    pub allowed_hosts: Option<Vec<String>>,
    /// Allow requests to loopback/private/link-local addresses. Off by
    /// default; enable for trusted internal aggregation.
    pub allow_private_networks: bool,
    /// Maximum response body size accumulated by `resp:text()` /
    /// `resp:json()`, in bytes.
    pub max_response_bytes: u64,
    /// Maximum concurrent requests per `await_all(...)` call.
    pub max_concurrent: usize,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            allowed_hosts: None,
            allow_private_networks: false,
            max_response_bytes: 8 * 1024 * 1024, // 8 MiB
            max_concurrent: 8,
        }
    }
}

/// Validates one request URL against the policy. Called for the initial
/// URL and again for every redirect hop, so redirects cannot cross the
/// trust boundary.
pub(crate) async fn check_url(url: &Url, opts: &FetchOptions) -> mlua::Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(mlua::Error::RuntimeError(format!(
            "fetch only supports http/https URLs, got `{url}`"
        )));
    }
    let Some(host) = url.host() else {
        return Err(mlua::Error::RuntimeError(format!(
            "fetch URL `{url}` has no host"
        )));
    };

    if let Some(allowed) = &opts.allowed_hosts {
        let name = url.host_str().unwrap_or_default();
        if !allowed.iter().any(|a| a.eq_ignore_ascii_case(name)) {
            return Err(mlua::Error::RuntimeError(format!(
                "fetch host `{name}` is not in fetch.allowed_hosts"
            )));
        }
    }

    if opts.allow_private_networks {
        return Ok(());
    }

    // Resolve-then-check: a domain must not resolve to any special-purpose
    // address. (Known limitation: without connection pinning a malicious
    // DNS server could still rebind between this check and the connect.)
    let ips: Vec<IpAddr> = match host {
        url::Host::Ipv4(ip) => vec![ip.into()],
        url::Host::Ipv6(ip) => vec![ip.into()],
        url::Host::Domain(domain) => {
            let port = url.port_or_known_default().unwrap_or(80);
            tokio::net::lookup_host((domain, port))
                .await
                .into_lua_err()?
                .map(|addr| addr.ip())
                .collect()
        }
    };
    if ips.is_empty() {
        return Err(mlua::Error::RuntimeError(format!(
            "fetch host `{}` did not resolve to any address",
            url.host_str().unwrap_or_default()
        )));
    }
    if ips.iter().any(|ip| is_forbidden_ip(*ip)) {
        return Err(mlua::Error::RuntimeError(format!(
            "fetch host `{}` resolves to a private or local address \
             (set fetch.allow_private_networks to permit this)",
            url.host_str().unwrap_or_default()
        )));
    }
    Ok(())
}

/// Special-purpose address ranges refused unless private networks are
/// explicitly allowed: loopback, RFC1918, link-local (including cloud
/// metadata endpoints), CGNAT, unspecified, broadcast, and their IPv6
/// counterparts (ULA, link-local, v4-mapped forms).
fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                // CGNAT 100.64.0.0/10
                || (octets[0] == 100 && (64..128).contains(&octets[1]))
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // Unique-local fc00::/7
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // Link-local fe80::/10
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().is_some_and(|v4| is_forbidden_ip(v4.into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("ip literal")
    }

    #[test]
    fn special_purpose_addresses_are_forbidden() {
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254", // cloud metadata
            "100.64.0.1",      // CGNAT
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_forbidden_ip(ip(bad)), "{bad} must be forbidden");
        }
        for good in ["93.184.216.34", "1.1.1.1", "2606:4700::1111"] {
            assert!(!is_forbidden_ip(ip(good)), "{good} must be allowed");
        }
    }

    #[tokio::test]
    async fn policy_checks_urls() {
        let default = FetchOptions::default();
        let url: Url = "http://127.0.0.1:8080/x".parse().expect("url");
        assert!(check_url(&url, &default).await.is_err());

        let open = FetchOptions {
            allow_private_networks: true,
            ..Default::default()
        };
        assert!(check_url(&url, &open).await.is_ok());

        // Non-http schemes are always refused.
        let ftp: Url = "ftp://example.com/x".parse().expect("url");
        assert!(check_url(&ftp, &open).await.is_err());

        // The allow-list applies even with private networks allowed.
        let listed = FetchOptions {
            allowed_hosts: Some(vec!["api.example.com".into()]),
            allow_private_networks: true,
            ..Default::default()
        };
        let other: Url = "http://evil.example.com/".parse().expect("url");
        assert!(check_url(&other, &listed).await.is_err());
        let ok: Url = "http://API.example.com/".parse().expect("url");
        assert!(check_url(&ok, &listed).await.is_ok());
    }
}
