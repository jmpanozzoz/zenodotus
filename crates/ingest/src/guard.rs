//! The address guard. Every address a fetch connects to is resolved here first, and addresses in private, loopback,
//! link-local, documentation and other special-purpose ranges are dropped, so a public host name that resolves to an
//! internal address (DNS rebinding included) is never reached.

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolving};

/// A boxed future, as [`Resolve`] returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Resolves host names to addresses.
pub trait Resolve: Send + Sync {
    /// Every address of `host`.
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>>;
}

/// The operating system's resolver.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemResolve;

impl Resolve for SystemResolve {
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
        Box::pin(async move {
            let found = tokio::net::lookup_host((host, 0)).await?;
            Ok(found.map(|addr| addr.ip()).collect())
        })
    }
}

/// Blocked IPv4 ranges, as (network, prefix length).
const BLOCKED_V4: [(Ipv4Addr, u32); 16] = [
    (Ipv4Addr::UNSPECIFIED, 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 0, 2, 0), 24),
    (Ipv4Addr::new(192, 88, 99, 0), 24),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(198, 51, 100, 0), 24),
    (Ipv4Addr::new(203, 0, 113, 0), 24),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
    (Ipv4Addr::BROADCAST, 32),
];

/// Blocked IPv6 ranges, as (network, prefix length). IPv4-mapped addresses are checked as IPv4.
const BLOCKED_V6: [(Ipv6Addr, u32); 9] = [
    (Ipv6Addr::UNSPECIFIED, 128),
    (Ipv6Addr::LOCALHOST, 128),
    (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
];

/// Whether `ip` lies in a range that fetches may not reach.
#[must_use]
pub fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_blocked_v4(ip),
        IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or_else(|| is_blocked_v6(ip), is_blocked_v4),
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let ip = u32::from(ip);
    BLOCKED_V4
        .iter()
        .any(|&(network, bits)| ip & (u32::MAX << (32 - bits)) == u32::from(network))
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    let ip = u128::from(ip);
    BLOCKED_V6
        .iter()
        .any(|&(network, bits)| ip & (u128::MAX << (128 - bits)) == u128::from(network))
}

/// Every address of a host was blocked; fetches report it as `blocked_address`.
#[derive(Debug, thiserror::Error)]
#[error("{host} resolves only to blocked addresses")]
pub struct Blocked {
    /// The host name.
    pub host: String,
}

/// A [`Resolve`] whose answers lose every blocked address, unless private networks are allowed.
#[derive(Clone)]
pub struct GuardedResolver {
    inner: Arc<dyn Resolve>,
    allow_private_networks: bool,
}

impl GuardedResolver {
    /// Guards `inner`; `allow_private_networks` turns the guard off.
    #[must_use]
    pub fn new(inner: Arc<dyn Resolve>, allow_private_networks: bool) -> Self {
        Self {
            inner,
            allow_private_networks,
        }
    }

    /// The addresses of `host` that may be reached.
    ///
    /// # Errors
    /// The resolver's error, or [`Blocked`] (as an `io::Error` of kind `PermissionDenied`) when every address was
    /// dropped.
    pub async fn addresses(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        let mut found = self.inner.resolve(host).await?;
        if found.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{host} has no address"),
            ));
        }
        if !self.allow_private_networks {
            found.retain(|ip| !is_blocked(*ip));
        }
        if found.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                Blocked { host: host.to_owned() },
            ));
        }
        Ok(found)
    }
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let guard = self.clone();
        Box::pin(async move {
            let found = guard.addresses(name.as_str()).await?;
            let addrs: Addrs = Box::new(found.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    /// Answers every name with fixed addresses.
    struct Fixed(Vec<IpAddr>);

    impl Resolve for Fixed {
        fn resolve<'a>(&'a self, _host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn every_special_purpose_range_is_blocked_at_both_ends() {
        let blocked = [
            ("0.0.0.0", "0.255.255.255"),
            ("10.0.0.0", "10.255.255.255"),
            ("100.64.0.0", "100.127.255.255"),
            ("127.0.0.0", "127.255.255.255"),
            ("169.254.0.0", "169.254.255.255"),
            ("172.16.0.0", "172.31.255.255"),
            ("192.0.0.0", "192.0.0.255"),
            ("192.0.2.0", "192.0.2.255"),
            ("192.88.99.0", "192.88.99.255"),
            ("192.168.0.0", "192.168.255.255"),
            ("198.18.0.0", "198.19.255.255"),
            ("198.51.100.0", "198.51.100.255"),
            ("203.0.113.0", "203.0.113.255"),
            ("224.0.0.0", "239.255.255.255"),
            ("240.0.0.0", "255.255.255.255"),
            ("::", "::"),
            ("::1", "::1"),
            ("::ffff:10.0.0.1", "::ffff:127.0.0.1"),
            ("64:ff9b::", "64:ff9b::ffff:ffff"),
            ("2002::", "2002:ffff:ffff:ffff:ffff:ffff:ffff:ffff"),
            ("2001::", "2001:0:ffff:ffff:ffff:ffff:ffff:ffff"),
            ("2001:db8::", "2001:db8:ffff:ffff:ffff:ffff:ffff:ffff"),
            ("fc00::", "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"),
            ("fe80::", "febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff"),
            ("ff00::", "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"),
        ];
        for (first, last) in blocked {
            assert!(is_blocked(ip(first)), "{first}");
            assert!(is_blocked(ip(last)), "{last}");
        }
    }

    #[test]
    fn public_addresses_next_to_the_ranges_pass() {
        let public = [
            "1.0.0.0",
            "9.255.255.255",
            "11.0.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "126.255.255.255",
            "128.0.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.0.1.0",
            "192.167.255.255",
            "198.17.255.255",
            "198.20.0.0",
            "223.255.255.255",
            "8.8.8.8",
            "::2",
            "::ffff:8.8.8.8",
            "2001:1::",
            "2001:4860:4860::8888",
            "2003::",
            "fbff:ffff::",
            "fec0::",
            "feff::",
        ];
        for address in public {
            assert!(!is_blocked(ip(address)), "{address}");
        }
    }

    #[tokio::test]
    async fn blocked_answers_are_dropped_unless_private_networks_are_allowed() {
        let answers = vec![ip("10.0.0.1"), ip("93.184.216.34"), ip("::1")];
        let guard = GuardedResolver::new(Arc::new(Fixed(answers.clone())), false);
        assert_eq!(guard.addresses("docs.example").await.unwrap(), [ip("93.184.216.34")]);
        let open = GuardedResolver::new(Arc::new(Fixed(answers.clone())), true);
        assert_eq!(open.addresses("docs.example").await.unwrap(), answers);
    }

    #[tokio::test]
    async fn a_name_that_resolves_only_to_blocked_addresses_is_refused() {
        let guard = GuardedResolver::new(Arc::new(Fixed(vec![ip("127.0.0.1"), ip("169.254.169.254")])), false);
        let error = guard.addresses("rebind.example").await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let blocked = error.into_inner().unwrap().downcast::<Blocked>().unwrap();
        assert_eq!(blocked.to_string(), "rebind.example resolves only to blocked addresses");
        let empty = GuardedResolver::new(Arc::new(Fixed(Vec::new())), true);
        assert_eq!(
            empty.addresses("none.example").await.unwrap_err().to_string(),
            "none.example has no address"
        );
    }

    #[tokio::test]
    async fn reqwest_gets_socket_addresses_with_port_zero() {
        let guard = GuardedResolver::new(Arc::new(Fixed(vec![ip("93.184.216.34")])), false);
        let resolver: &dyn reqwest::dns::Resolve = &guard;
        let addrs: Vec<SocketAddr> = resolver
            .resolve(Name::from_str("docs.example").unwrap())
            .await
            .unwrap()
            .collect();
        assert_eq!(addrs, [SocketAddr::new(ip("93.184.216.34"), 0)]);
        let refused = GuardedResolver::new(Arc::new(Fixed(vec![ip("10.1.2.3")])), false);
        let resolver: &dyn reqwest::dns::Resolve = &refused;
        assert!(resolver.resolve(Name::from_str("docs.example").unwrap()).await.is_err());
    }

    #[tokio::test]
    async fn the_system_resolver_finds_localhost() {
        let found = SystemResolve.resolve("localhost").await.unwrap();
        assert!(found.iter().all(IpAddr::is_loopback), "{found:?}");
        assert_ne!(found, [] as [IpAddr; 0]);
    }
}
