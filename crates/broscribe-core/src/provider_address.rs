//! Validation shared by provider setup and HTTP requests.

use crate::{Error, Result};

/// Validated parts of an HTTP provider’s base URL.
pub struct ProviderAddress<'a> {
    pub trimmed: &'a str,
    pub https: bool,
    pub host: &'a str,
    pub port: Option<u16>,
}

pub fn parse_provider_address(base_url: &str) -> Result<ProviderAddress<'_>> {
    let bad = |why: &str| Err(Error::InvalidInput(format!("base_url {why}")));
    let base = base_url.trim();
    if base.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return bad("contains whitespace");
    }
    let Some((scheme, rest)) = base.split_once("://") else {
        return bad("must start with https:// (or http:// for this computer)");
    };
    if base.contains(['?', '#', '\\']) {
        return bad("must not contain a query, fragment or backslash");
    }
    let authority = rest.split('/').next().unwrap_or("");
    if authority.contains('@') {
        return bad("must not contain credentials; store the API key in the keychain");
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let Some((host, after)) = v6.split_once(']') else {
            return bad("has an invalid IPv6 host");
        };
        if host.parse::<std::net::Ipv6Addr>().is_err()
            || (!after.is_empty() && !after.starts_with(':'))
        {
            return bad("has an invalid IPv6 host");
        }
        (host, after.strip_prefix(':'))
    } else {
        if !authority
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-:".contains(&byte))
        {
            return bad("has an invalid host");
        }
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return bad("has no host");
    }
    let port = match port {
        None => None,
        Some(p) => match p.parse::<u16>() {
            Ok(p) => Some(p),
            Err(_) => return bad("has an invalid port"),
        },
    };
    let https = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => {
            if !loopback_host(host) {
                return bad("uses plain http for a remote host; use https");
            }
            false
        }
        _ => return bad("must use https (or http for this computer)"),
    };
    Ok(ProviderAddress {
        trimmed: base,
        https,
        host,
        port,
    })
}

pub fn loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// A provider server address may contain a path, but query strings and
/// fragments would be forwarded while constructing API requests.
pub fn validate_provider_address(address: &str) -> Result<()> {
    parse_provider_address(address)?;
    Ok(())
}
