//! The `https:` range transport remote media is read through: every redirect target checked
//! against the operator's origins before it is contacted, the address rule on the addresses a
//! connection uses, and the platform's certificate verifier.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Mutex, PoisonError};

use alpha2_host_core::remote::{
    BODY_IDLE_TIMEOUT, CONNECT_TIMEOUT, HEAD_TIMEOUT, RangeRequest, RangeResponse, RangeTransport,
    RefusalPolicy, ResponseHeaders, TransportFailure, shown_url,
};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

use crate::media_policy::{MediaPolicy, PrivateAddresses};

const MAX_REDIRECTS: usize = 5;

/// A connection the address rule refused, carried through ureq's resolver error.
#[derive(Debug)]
struct AddressRefused {
    host: String,
    address: IpAddr,
}

impl std::fmt::Display for AddressRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} resolves to {}, which is not a global address; start the host with \
             --media-private-addresses allow to read it",
            self.host, self.address
        )
    }
}

impl std::error::Error for AddressRefused {}

/// Whether `address` is global unicast: not loopback, unspecified, private, shared, link-local,
/// unique-local, multicast, broadcast, reserved or documentation (RFC 6890), nor an IPv4-mapped
/// form of one.
pub fn is_global(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_global_v4(address),
        IpAddr::V6(address) => match address.to_ipv4_mapped() {
            Some(mapped) => is_global_v4(mapped),
            None => is_global_v6(address),
        },
    }
}

fn is_global_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(a == 0
        || address.is_loopback()
        || address.is_private()
        || (a == 100 && (64..128).contains(&b))
        || address.is_link_local()
        || (a == 192 && b == 0 && c == 0)
        || address.is_documentation()
        || (a == 198 && (b == 18 || b == 19))
        || address.is_multicast()
        || a >= 240)
}

fn is_global_v6(address: Ipv6Addr) -> bool {
    let first = address.segments()[0];
    !(address.is_unspecified()
        || address.is_loopback()
        || (first & 0xffc0) == 0xfe80
        || (first & 0xffc0) == 0xfec0
        || (first & 0xfe00) == 0xfc00
        || address.is_multicast()
        || (first == 0x2001 && address.segments()[1] == 0x0db8))
}

/// ureq's resolver with the address rule applied to the target's addresses. A proxy the
/// environment names is resolved without it: the host connects to the proxy, not the origin.
#[derive(Debug)]
struct AddressRule {
    private: PrivateAddresses,
}

impl Resolver for AddressRule {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let resolved = DefaultResolver::default().resolve(uri, config, timeout)?;
        let to_proxy = config.proxy().is_some_and(|proxy| proxy.uri() == uri);
        if self.private == PrivateAddresses::Allow || to_proxy {
            return Ok(resolved);
        }
        let mut kept = self.empty();
        for address in resolved.iter().filter(|address| is_global(address.ip())) {
            kept.push(*address);
        }
        if kept.is_empty() {
            return Err(ureq::Error::Other(Box::new(AddressRefused {
                host: uri.host().unwrap_or_default().to_owned(),
                address: resolved
                    .first()
                    .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), std::net::SocketAddr::ip),
            })));
        }
        Ok(kept)
    }
}

#[cfg(test)]
static TEST_ROOTS: Mutex<Vec<ureq::tls::Certificate<'static>>> = Mutex::new(Vec::new());

/// Makes every media transport made after it trust `der`, and the other roots tests added,
/// instead of the platform.
#[cfg(test)]
pub fn trust_test_root(der: &[u8]) {
    TEST_ROOTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(ureq::tls::Certificate::from_der(der).to_owned());
}

fn roots() -> ureq::tls::RootCerts {
    #[cfg(test)]
    {
        let roots = TEST_ROOTS.lock().unwrap_or_else(PoisonError::into_inner);
        if !roots.is_empty() {
            return ureq::tls::RootCerts::new_with_certs(&roots);
        }
    }
    ureq::tls::RootCerts::PlatformVerifier
}

fn agent(private: PrivateAddresses) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_resolve(Some(CONNECT_TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(HEAD_TIMEOUT))
        .timeout_per_read(Some(BODY_IDLE_TIMEOUT))
        .tls_config(ureq::tls::TlsConfig::builder().root_certs(roots()).build())
        .build();
    ureq::Agent::with_parts(config, DefaultConnector::new(), AddressRule { private })
}

/// The range transport of one request: its own agent, and the permanent redirects it met.
pub struct MediaTransport {
    agent: ureq::Agent,
    policy: MediaPolicy,
    permanent: Mutex<HashMap<String, String>>,
}

impl MediaTransport {
    pub fn new(policy: MediaPolicy) -> Self {
        Self {
            agent: agent(policy.private_addresses),
            policy,
            permanent: Mutex::new(HashMap::new()),
        }
    }

    fn send(
        &self,
        url: &str,
        request: &RangeRequest,
    ) -> Result<ureq::http::Response<ureq::Body>, TransportFailure> {
        let mut builder = self
            .agent
            .get(url)
            .header("Range", request.range_header())
            .header("Accept-Encoding", "identity");
        if let Some(precondition) = &request.precondition {
            let (name, value) = precondition.header();
            builder = builder.header(name, value);
        }
        builder.call().map_err(|error| failure(url, error))
    }
}

/// Where a `Location` points, resolved against the URL that answered with it (RFC 3986 section 5).
fn redirect_target(from: &str, location: &str) -> String {
    if location.starts_with("//") {
        return format!("https:{location}");
    }
    if location
        .split_once(':')
        .is_some_and(|(scheme, _)| !scheme.contains('/') && !scheme.is_empty())
    {
        return location.to_owned();
    }
    let after_scheme = from.find("://").map_or(0, |at| at + 3);
    let path_start = from[after_scheme..]
        .find(['/', '?', '#'])
        .map_or(from.len(), |at| after_scheme + at);
    let origin = &from[..path_start];
    if location.starts_with('/') {
        return format!("{origin}{location}");
    }
    let path = from[path_start..].split(['?', '#']).next().unwrap_or("/");
    let directory = path.rfind('/').map_or("/", |at| &path[..=at]);
    format!("{origin}{directory}{location}")
}

fn header(response: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn is_rustls(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.is::<rustls::Error>() {
            return true;
        }
        current = error.source();
    }
    false
}

/// A ureq failure by its variant: TLS refusals are never retried, network failures are.
fn failure(url: &str, error: ureq::Error) -> TransportFailure {
    let shown = shown_url(url);
    let message = error.to_string().replace(url, &shown);
    match error {
        ureq::Error::Other(inner) if inner.is::<AddressRefused>() => TransportFailure::Refused {
            policy: RefusalPolicy::Address,
            message: format!("{shown}: {inner}"),
        },
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => TransportFailure::Tls(message),
        ureq::Error::Io(io) if io.get_ref().is_some_and(|inner| is_rustls(inner)) => {
            TransportFailure::Tls(message)
        }
        ureq::Error::Io(_)
        | ureq::Error::Timeout(_)
        | ureq::Error::HostNotFound
        | ureq::Error::ConnectionFailed
        | ureq::Error::Protocol(_)
        | ureq::Error::ConnectProxyFailed(_)
        | ureq::Error::BodyStalled => TransportFailure::Unreachable(message),
        _ => TransportFailure::Rejected(message),
    }
}

impl RangeTransport for MediaTransport {
    fn get(&self, request: &RangeRequest) -> Result<RangeResponse, TransportFailure> {
        let permanent = |url: &str| {
            self.permanent
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(url)
                .cloned()
        };
        let mut current = request.url.clone();
        self.policy.check(&current, RefusalPolicy::Origins)?;
        for hop in 0..=MAX_REDIRECTS {
            if let Some(moved) = permanent(&current) {
                current = moved;
            }
            if hop > 0 {
                self.policy.check(&current, RefusalPolicy::Redirect)?;
            }
            let response = self.send(&current, request)?;
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let location = header(&response, "location").ok_or_else(|| {
                    TransportFailure::Rejected(format!(
                        "{}: a {status} redirect without a Location",
                        shown_url(&current)
                    ))
                })?;
                let target = redirect_target(&current, &location);
                if matches!(status, 301 | 308) {
                    self.permanent
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(current.clone(), target.clone());
                }
                current = target;
                continue;
            }
            let headers = ResponseHeaders {
                content_range: header(&response, "content-range"),
                content_encoding: header(&response, "content-encoding"),
                content_type: header(&response, "content-type"),
                etag: header(&response, "etag"),
                last_modified: header(&response, "last-modified"),
                retry_after: header(&response, "retry-after"),
            };
            return Ok(RangeResponse {
                status,
                headers,
                body: Box::new(response.into_body().into_reader()),
            });
        }
        Err(TransportFailure::Rejected(format!(
            "{}: more than {MAX_REDIRECTS} redirects",
            shown_url(&request.url)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_global_unicast_addresses_are_global() {
        for local in [
            "127.0.0.1",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "169.254.169.254",
            "224.0.0.1",
            "255.255.255.255",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(!is_global(local.parse().unwrap()), "{local}");
        }
        for global in [
            "93.184.216.34",
            "8.8.8.8",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_global(global.parse().unwrap()), "{global}");
        }
    }

    #[test]
    fn a_redirect_target_resolves_against_the_url_that_answered() {
        let from = "https://a.example/media/take.mp4?sig=1";
        assert_eq!(
            redirect_target(from, "https://b.example/x"),
            "https://b.example/x"
        );
        assert_eq!(
            redirect_target(from, "//b.example/x"),
            "https://b.example/x"
        );
        assert_eq!(redirect_target(from, "/x?y"), "https://a.example/x?y");
        assert_eq!(
            redirect_target(from, "other.mp4"),
            "https://a.example/media/other.mp4"
        );
    }
}
