//! Client address resolution with explicit proxy trust.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

/// The visitor's IP address, for rate limiting and abuse controls.
///
/// This is the socket peer address unless the peer is a proxy configured with
/// [`crate::Server::trust_proxy`]. Then it is the rightmost `X-Forwarded-For`
/// entry that is not itself a trusted proxy, so clients cannot spoof it by
/// sending their own header.
///
/// It is available when serving with [`crate::Server::serve`]. When driving
/// [`crate::Server::build`] directly in tests, add
/// [`MockConnectInfo`](axum::extract::connect_info::MockConnectInfo) to
/// supply a peer address; otherwise extraction fails with `500`.
///
/// ```
/// use webnook::ClientIp;
///
/// async fn contact(ClientIp(ip): ClientIp) -> String {
///     format!("hello, {ip}")
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientIp(pub IpAddr);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<ClientIp>().copied().ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "client address unavailable",
        ))
    }
}

/// An address range in CIDR notation, e.g. `10.0.0.0/8` or `::1`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let (addr, prefix) = match text.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (text, None),
        };
        let network: IpAddr = addr.trim().parse().ok()?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix.trim().parse().ok().filter(|p| *p <= max)?,
            None => max,
        };
        Some(Self { network, prefix })
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// Middleware that resolves [`ClientIp`] for every request.
pub(crate) async fn resolve(
    trusted: Arc<Vec<Cidr>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0)
        .or_else(|| {
            req.extensions()
                .get::<MockConnectInfo<SocketAddr>>()
                .map(|info| info.0)
        });
    if let Some(peer) = peer {
        let ip = client_ip(peer.ip().to_canonical(), &req, &trusted);
        req.extensions_mut().insert(ClientIp(ip));
    }
    next.run(req).await
}

fn client_ip(peer: IpAddr, req: &Request<Body>, trusted: &[Cidr]) -> IpAddr {
    let is_trusted = |ip: IpAddr| trusted.iter().any(|cidr| cidr.contains(ip));
    if !is_trusted(peer) {
        return peer;
    }
    let mut client = peer;
    let hops = req
        .headers()
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .collect::<Vec<_>>();
    for hop in hops.into_iter().rev() {
        let Ok(ip) = hop.trim().parse::<IpAddr>() else {
            break;
        };
        client = ip.to_canonical();
        if !is_trusted(client) {
            break;
        }
    }
    client
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidrs(list: &[&str]) -> Vec<Cidr> {
        list.iter().map(|c| Cidr::parse(c).unwrap()).collect()
    }

    fn forwarded(values: &[&str]) -> Request<Body> {
        let mut builder = Request::builder();
        for value in values {
            builder = builder.header("x-forwarded-for", *value);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn parses_and_matches_ranges() {
        let net = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(net.contains("10.1.2.3".parse().unwrap()));
        assert!(!net.contains("11.0.0.1".parse().unwrap()));
        assert!(net.contains("::ffff:10.0.0.1".parse().unwrap()));
        assert!(Cidr::parse("::1").unwrap().contains("::1".parse().unwrap()));
        assert!(Cidr::parse("0.0.0.0/0")
            .unwrap()
            .contains("8.8.8.8".parse().unwrap()));
        assert!(Cidr::parse("10.0.0.0/33").is_none());
        assert!(Cidr::parse("nonsense").is_none());
    }

    #[test]
    fn untrusted_peers_cannot_spoof() {
        let req = forwarded(&["1.2.3.4"]);
        let peer = "203.0.113.9".parse().unwrap();
        assert_eq!(client_ip(peer, &req, &cidrs(&["127.0.0.1"])), peer);
    }

    #[test]
    fn trusted_proxy_chain_yields_rightmost_untrusted() {
        let trusted = cidrs(&["127.0.0.1", "10.0.0.0/8"]);
        let peer = "127.0.0.1".parse().unwrap();
        let req = forwarded(&["6.6.6.6, 198.51.100.7", "10.0.0.2"]);
        assert_eq!(
            client_ip(peer, &req, &trusted),
            "198.51.100.7".parse::<IpAddr>().unwrap()
        );
        let req = forwarded(&[]);
        assert_eq!(client_ip(peer, &req, &trusted), peer);
        let req = forwarded(&["garbage, 10.0.0.3"]);
        assert_eq!(
            client_ip(peer, &req, &trusted),
            "10.0.0.3".parse::<IpAddr>().unwrap()
        );
    }
}
