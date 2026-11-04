//! Which addresses a job may call. Outside development, private, loopback, link-local and
//! the other non-public addresses are refused (SSRF: a job must not reach the network
//! irisqo runs in, the cloud metadata service included), unless `--allow-targets` lists them.
//!
//! Checked twice: an IP address in the URL when the job is created and again before each
//! attempt, and a host name when it is resolved ([`GuardedResolver`]): the connection goes
//! only to the addresses that passed, so a name that resolves differently later (DNS
//! rebinding) changes nothing. hyper follows no redirects, so a `302` cannot lead elsewhere.

use std::{
    error::Error as StdError,
    fmt,
    future::Future,
    io, iter,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};

use hyper_util::client::legacy::connect::dns::{GaiResolver, Name};
use ipnet::IpNet;
use tower_service::Service;

use super::Environment;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPolicy {
    /// Development: every address.
    allow_all: bool,
    /// Non-public networks allowed anyway.
    allow: Vec<IpNet>,
}

/// `--allow-targets`: IP addresses and networks, comma separated
/// (`10.1.0.0/16,192.168.1.10`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AllowTargets(pub Vec<IpNet>);

impl FromStr for AllowTargets {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| {
                item.parse::<IpNet>()
                    .or_else(|_| item.parse::<IpAddr>().map(IpNet::from))
                    .map_err(|_| format!("{item:?} is not an IP address or network"))
            })
            .collect::<Result<_, _>>()
            .map(AllowTargets)
    }
}

impl TargetPolicy {
    pub fn new(environment: Environment, allow: AllowTargets) -> Self {
        TargetPolicy {
            allow_all: environment.is_development(),
            allow: allow.0,
        }
    }

    /// Any address: for the tests of the policy itself.
    #[cfg(test)]
    pub fn allow_all() -> Self {
        TargetPolicy {
            allow_all: true,
            allow: Vec::new(),
        }
    }

    #[must_use = "an unchecked address is not refused"]
    pub fn allows(&self, ip: IpAddr) -> bool {
        self.allow_all || is_public(ip) || self.allow.iter().any(|net| net.contains(&ip))
    }

    /// The host of a URL: an IP address must be allowed, and `localhost` names are
    /// refused like the loopback address. Other names are checked when resolved.
    pub fn check_host(&self, host: &str) -> Result<(), &'static str> {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = host.parse::<IpAddr>() {
            return if self.allows(ip) {
                Ok(())
            } else {
                Err("is a private address")
            };
        }
        let name = host.trim_end_matches('.').to_ascii_lowercase();
        let loopback = name == "localhost" || name.ends_with(".localhost");
        if loopback && !self.allows(IpAddr::V4(Ipv4Addr::LOCALHOST)) {
            return Err("is a private address");
        }
        Ok(())
    }
}

/// Reachable on the public internet: not loopback, private, link-local (the cloud metadata
/// service), shared (CGNAT), reserved, multicast, documentation or unspecified. An IPv4
/// address inside IPv6 (`::ffff:a.b.c.d`, NAT64 `64:ff9b::a.b.c.d`) is judged as IPv4.
#[must_use = "an unchecked address is not refused"]
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => match embedded_v4(ip) {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(ip),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || a == 0 // this network
        || (a == 100 && (b & 0xc0) == 64) // shared address space (CGNAT), 100.64/10
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments, 192.0.0/24
        || (a == 198 && (b & 0xfe) == 18) // benchmarking, 198.18/15
        || a >= 240) // reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (first & 0xfe00) == 0xfc00 // unique local, fc00::/7
        || (first & 0xffc0) == 0xfe80 // link-local, fe80::/10
        || (first == 0x2001 && ip.segments()[1] == 0x0db8)) // documentation, 2001:db8::/32
}

fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }
    let s = ip.segments();
    // NAT64 well-known prefix 64:ff9b::/96
    (s[..6] == [0x64, 0xff9b, 0, 0, 0, 0])
        .then(|| Ipv4Addr::from((u32::from(s[6]) << 16) | u32::from(s[7])))
}

/// A host name whose addresses are all refused.
#[derive(Debug)]
pub struct BlockedTarget(pub String);

impl fmt::Display for BlockedTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} resolves only to private addresses (allow them with --allow-targets)",
            self.0
        )
    }
}

impl StdError for BlockedTarget {}

/// The [`BlockedTarget`] in the causes of a client error, if that is why it failed.
pub fn blocked_target<'a>(err: &'a (dyn StdError + 'static)) -> Option<&'a BlockedTarget> {
    iter::successors(Some(err), |&err| err.source()).find_map(|err| err.downcast_ref())
}

/// The system resolver, keeping only the addresses the policy allows.
#[derive(Clone)]
pub struct GuardedResolver {
    policy: Arc<TargetPolicy>,
    inner: GaiResolver,
}

impl GuardedResolver {
    pub fn new(policy: Arc<TargetPolicy>) -> Self {
        GuardedResolver {
            policy,
            inner: GaiResolver::new(),
        }
    }
}

type BoxError = Box<dyn StdError + Send + Sync>;

impl Service<Name> for GuardedResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        self.inner.poll_ready(cx).map_err(BoxError::from)
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let policy = Arc::clone(&self.policy);
        let host = name.as_str().to_owned();
        let resolving = self.inner.call(name);
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = resolving
                .await
                .map_err(|err: io::Error| BoxError::from(err))?
                .filter(|addr| policy.allows(addr.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(BoxError::from(BlockedTarget(host)));
            }
            Ok(addrs.into_iter())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("ip")
    }

    #[test]
    fn public_and_private_addresses() {
        for public in [
            "8.8.8.8",
            "1.1.1.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public(ip(public)), "{public}");
        }
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
        ] {
            assert!(!is_public(ip(private)), "{private}");
        }
    }

    #[test]
    fn policy() {
        let prod = TargetPolicy::new(
            Environment::Production,
            "10.1.0.0/16, 192.168.1.10".parse().expect("allow"),
        );
        assert!(prod.allows(ip("8.8.8.8")));
        assert!(prod.allows(ip("10.1.200.3")));
        assert!(prod.allows(ip("192.168.1.10")));
        assert!(!prod.allows(ip("10.2.0.1")));
        assert!(!prod.allows(ip("192.168.1.11")));
        assert_eq!(
            prod.check_host("169.254.169.254"),
            Err("is a private address")
        );
        assert_eq!(prod.check_host("[::1]"), Err("is a private address"));
        assert_eq!(prod.check_host("LOCALHOST."), Err("is a private address"));
        assert_eq!(
            prod.check_host("api.localhost"),
            Err("is a private address")
        );
        assert_eq!(prod.check_host("example.com"), Ok(()));
        assert_eq!(prod.check_host("10.1.0.5"), Ok(()));

        let dev = TargetPolicy::new(Environment::Development, AllowTargets::default());
        assert_eq!(dev.check_host("127.0.0.1"), Ok(()));
        assert_eq!(dev.check_host("localhost"), Ok(()));
        assert!("10.0.0.0/33".parse::<AllowTargets>().is_err());
        assert!("nope".parse::<AllowTargets>().is_err());
    }

    #[tokio::test]
    async fn resolver_drops_refused_addresses() {
        let name = |host: &str| host.parse::<Name>().expect("name");
        let prod = TargetPolicy::new(Environment::Production, AllowTargets::default());
        let mut resolver = GuardedResolver::new(Arc::new(prod));
        let err = resolver
            .call(name("localhost"))
            .await
            .expect_err("loopback only");
        let blocked = blocked_target(err.as_ref()).expect("blocked target");
        assert!(
            blocked
                .to_string()
                .starts_with("localhost resolves only to private")
        );

        let loopback = TargetPolicy::new(
            Environment::Production,
            "127.0.0.0/8".parse().expect("allow"),
        );
        let mut resolver = GuardedResolver::new(Arc::new(loopback));
        let addrs: Vec<_> = resolver
            .call(name("localhost"))
            .await
            .expect("allowed")
            .collect();
        assert!(addrs.iter().all(|addr| addr.ip().is_loopback()) && !addrs.is_empty());
    }
}
