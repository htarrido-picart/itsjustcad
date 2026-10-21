// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Outbound containment for decks whose `base_url` may come from an untrusted
//! or synced config (the FFI path). The deck attaches the API key to every
//! request, so a `base_url` aimed at an internal host would both SSRF and leak
//! the key. Three layers:
//!   * [`base_url_is_public`] parses the URL (WHATWG, so decimal/hex/octal/short
//!     IPv4, upper-case schemes and userinfo are all normalised) and checks the
//!     host by IP range, never by string prefix;
//!   * [`public_only_client`] resolves names through [`PublicOnlyResolver`],
//!     which drops non-public addresses on EVERY connection — the addresses
//!     checked are the ones connected to, so DNS rebinding cannot slip past;
//!   * the same client follows no redirects (reqwest would otherwise carry the
//!     `x-api-key` header to wherever a public host redirects) and ignores
//!     proxy env vars (a proxy would resolve the name itself).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

/// `true` when `url` may be sent the API key: empty (provider default), or an
/// `http`/`https` URL without userinfo whose host is a public IP or a domain
/// outside the internal-name deny list.
pub fn base_url_is_public(url: &str) -> bool {
    if url.is_empty() {
        return true;
    }
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    if !matches!(u.scheme(), "http" | "https") || !u.username().is_empty() || u.password().is_some()
    {
        return false;
    }
    match u.host() {
        Some(url::Host::Ipv4(ip)) => ipv4_is_public(ip),
        Some(url::Host::Ipv6(ip)) => ipv6_is_public(ip),
        Some(url::Host::Domain(d)) => domain_is_public(d),
        None => false,
    }
}

/// `true` for a globally routable address.
pub fn ip_is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_public(v4),
        IpAddr::V6(v6) => ipv6_is_public(v6),
    }
}

fn ipv4_is_public(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local() // 169.254/16, incl. cloud metadata
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0 // 0/8 "this network"
        || (a == 100 && (64..128).contains(&b)) // CGNAT 100.64/10
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // benchmarking 198.18/15
        || a >= 240) // reserved 240/4
}

fn ipv6_is_public(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return ipv4_is_public(v4); // ::ffff:a.b.c.d
    }
    let s = ip.segments();
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        // NAT64 64:ff9b::/96 embeds an IPv4 address in its low 32 bits.
        let [.., a, b, c, d] = ip.octets();
        return ipv4_is_public(Ipv4Addr::new(a, b, c, d));
    }
    !(s[..6] == [0; 6] // ::/96: unspecified, loopback, IPv4-compatible
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
        || (s[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        || (s[0] & 0xffc0) == 0xfec0 // site-local fec0::/10 (deprecated)
        || (s[0] == 0x2001 && s[1] == 0x0db8)) // documentation
}

fn domain_is_public(domain: &str) -> bool {
    let d = domain.trim_end_matches('.').to_ascii_lowercase();
    !(d.is_empty()
        || d == "localhost"
        || d == "metadata"
        || [".localhost", ".internal", ".local", ".home.arpa"]
            .iter()
            .any(|s| d.ends_with(s)))
}

/// DNS resolver that keeps only public addresses (see [`ip_is_public`]); a name
/// resolving to none fails the connection.
pub struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let lookup = host.clone();
            let addrs = tokio::task::spawn_blocking(move || {
                std::net::ToSocketAddrs::to_socket_addrs(&(lookup.as_str(), 0))
                    .map(Iterator::collect::<Vec<SocketAddr>>)
            })
            .await??;
            let public: Vec<SocketAddr> =
                addrs.into_iter().filter(|a| ip_is_public(a.ip())).collect();
            if public.is_empty() {
                return Err(format!("{host} resolves only to non-public addresses").into());
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// HTTP client for decks with an untrusted `base_url`: public-only DNS, no
/// redirects, no proxy.
pub fn public_only_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(PublicOnlyResolver))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_internal_hosts_in_every_spelling() {
        for url in [
            "http://169.254.169.254/latest",
            "http://localhost:11434/v1",
            "http://127.0.0.1/v1",
            "http://[::1]:8080/v1",
            "http://metadata/computeMetadata",
            "http://foo.internal/v1",
            "http://10.0.0.5/v1",
            "http://172.16.0.1/v1",
            "http://192.168.1.1/v1",
            "http://100.64.0.1/v1",
            "http://0.0.0.0/",
            // IPv4-mapped / NAT64 / IPv4-compatible IPv6.
            "http://[::ffff:127.0.0.1]/v1",
            "http://[::ffff:7f00:1]/v1",
            "http://[::ffff:10.0.0.1]/v1",
            "http://[::ffff:169.254.169.254]/",
            "http://[64:ff9b::a9fe:a9fe]/",
            "http://[::127.0.0.1]/",
            // Unique-local / link-local IPv6.
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            // Userinfo hiding the real host, or carrying credentials.
            "http://api.openai.com@127.0.0.1/v1",
            "https://user:pw@api.openai.com/v1",
            // Decimal / hex / octal / short IPv4.
            "http://2130706433/",
            "http://0x7f000001/",
            "http://0177.0.0.1/",
            "http://127.1/",
            "http://10.1/",
            // Trailing dots.
            "http://localhost./v1",
            "http://metadata../",
            "http://foo.internal./",
            // Upper-case scheme and host.
            "HTTP://127.0.0.1/v1",
            "HTTPS://LOCALHOST/v1",
            // Non-http schemes and junk.
            "file:///etc/passwd",
            "ftp://api.openai.com/",
            "not a url",
        ] {
            assert!(!base_url_is_public(url), "{url} must be rejected");
        }
    }

    #[test]
    fn allows_public_hosts() {
        for url in [
            "",
            "https://api.anthropic.com",
            "https://api.openai.com/v1",
            "HTTPS://API.OPENAI.COM/v1",
            "http://172.32.0.1/v1",
            "http://8.8.8.8/",
            "http://[2606:4700::1111]/",
            "http://[64:ff9b::808:808]/",
        ] {
            assert!(base_url_is_public(url), "{url} must be allowed");
        }
    }

    #[tokio::test]
    async fn resolver_drops_loopback_names() {
        use reqwest::dns::Resolve as _;
        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        assert!(PublicOnlyResolver.resolve(name).await.is_err());
    }
}
