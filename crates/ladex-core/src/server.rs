//! Accepts TLS or plain connections and serves the warp routes, attaching each client's real address to its requests.

use std::convert::Infallible;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt};
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower_service::Service;
use warp::Reply;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type ServerBody = UnsyncBoxBody<Bytes, BoxError>;

/// Handles the requests warp can't: streamed upload and download bodies.
pub trait Api: Clone + Send + Sync + 'static {
    fn claims(&self, path: &str) -> bool;
    fn handle(&self, req: hyper::Request<Incoming>, peer: PeerAddr) -> impl Future<Output = hyper::Response<ServerBody>> + Send;
}

/// Two handlers served together; the first takes the paths it claims, the second the rest it claims.
#[derive(Clone)]
pub struct Chain<First, Second>(pub First, pub Second);

impl<First: Api, Second: Api> Api for Chain<First, Second> {
    fn claims(&self, path: &str) -> bool {
        self.0.claims(path) || self.1.claims(path)
    }

    fn handle(&self, req: hyper::Request<Incoming>, peer: PeerAddr) -> impl Future<Output = hyper::Response<ServerBody>> + Send {
        let first_takes_it = self.0.claims(req.uri().path());
        let (first, second) = (self.0.clone(), self.1.clone());
        async move {
            match first_takes_it {
                true => first.handle(req, peer).await,
                false => second.handle(req, peer).await,
            }
        }
    }
}

fn from_warp(response: warp::reply::Response) -> hyper::Response<ServerBody> {
    response.map(|body| body.map_err(|e| Box::new(e) as BoxError).boxed_unsync())
}

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
pub struct PeerAddr {
    pub addr: SocketAddr,
    pub tls: bool,
}

/// Key used for rate limiting; requests without an address share one bucket.
pub fn peer_ip(peer: Option<PeerAddr>) -> IpAddr {
    peer.map(|p| p.addr.ip()).unwrap_or(IpAddr::from([0, 0, 0, 0]))
}

pub fn is_tls(peer: Option<PeerAddr>) -> bool {
    peer.is_some_and(|p| p.tls)
}

/// Host headers accepted on the loopback listener; stops DNS rebinding from making another site look same-origin.
pub type AllowedHosts = Vec<String>;

fn host_allowed(allowed: &AllowedHosts, host: Option<&hyper::header::HeaderValue>) -> bool {
    host.and_then(|h| h.to_str().ok()).is_some_and(|h| allowed.iter().any(|a| a.eq_ignore_ascii_case(h)))
}

pub async fn serve<S, A>(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    allowed_hosts: Option<AllowedHosts>,
    routes: S,
    api: A,
) -> anyhow::Result<()>
where
    S: Service<hyper::Request<Incoming>, Response = warp::reply::Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
    A: Api,
{
    serve_until(listener, tls, allowed_hosts, routes, api, std::future::pending()).await
}

/// Serves like `serve` until `stop` resolves, then closes the listener and waits for every open connection to end.
pub async fn serve_until<S, A>(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    allowed_hosts: Option<AllowedHosts>,
    routes: S,
    api: A,
    stop: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()>
where
    S: Service<hyper::Request<Incoming>, Response = warp::reply::Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
    A: Api,
{
    tracing::info!("Serving on {} ({})", listener.local_addr()?, if tls.is_some() { "TLS" } else { "plain HTTP" });

    tokio::pin!(stop);
    let mut connections = tokio::task::JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = &mut stop => break,
        };
        let (tcp, peer) = match accepted {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!("Accept error: {e}");
                continue;
            }
        };
        let _ = tcp.set_nodelay(true);
        let tls = tls.clone();
        let routes = routes.clone();
        let api = api.clone();
        let allowed_hosts = allowed_hosts.clone();
        let is_tls = tls.is_some();

        connections.spawn(async move {
            let service = hyper::service::service_fn(move |mut req: hyper::Request<Incoming>| {
                let peer_addr = PeerAddr { addr: peer, tls: is_tls };
                req.extensions_mut().insert(peer_addr);
                let mut routes = routes.clone();
                let api = api.clone();
                let allowed_hosts = allowed_hosts.clone();
                async move {
                    if let Some(allowed) = &allowed_hosts {
                        if !host_allowed(allowed, req.headers().get(hyper::header::HOST)) {
                            let refusal = warp::reply::with_status("Unexpected Host header", hyper::StatusCode::MISDIRECTED_REQUEST);
                            return Ok::<_, Infallible>(from_warp(refusal.into_response()));
                        }
                    }
                    if api.claims(req.uri().path()) {
                        return Ok(api.handle(req, peer_addr).await);
                    }
                    Ok(from_warp(routes.call(req).await?))
                }
            });
            let mut http = hyper::server::conn::http1::Builder::new();
            http.timer(TokioTimer::new()).header_read_timeout(HEADER_READ_TIMEOUT);

            let result = match tls {
                Some(acceptor) => {
                    let stream = match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                        Ok(Ok(stream)) => stream,
                        Ok(Err(e)) => {
                            tracing::debug!("TLS handshake with {peer} failed: {e}");
                            return;
                        }
                        Err(_) => {
                            tracing::debug!("TLS handshake with {peer} timed out");
                            return;
                        }
                    };
                    http.serve_connection(TokioIo::new(stream), service).with_upgrades().await
                }
                None => http.serve_connection(TokioIo::new(tcp), service).with_upgrades().await,
            };
            if let Err(e) = result {
                tracing::debug!("Connection with {peer} ended: {e}");
            }
        });
    }
    connections.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;

    fn allowed() -> AllowedHosts {
        vec!["localhost:8081".into(), "127.0.0.1:8081".into()]
    }

    #[test]
    fn only_listed_hosts_pass() {
        let ok = HeaderValue::from_static("localhost:8081");
        let upper = HeaderValue::from_static("LOCALHOST:8081");
        let rebinding = HeaderValue::from_static("attacker.example:8081");
        let wrong_port = HeaderValue::from_static("localhost:9999");
        assert!(host_allowed(&allowed(), Some(&ok)));
        assert!(host_allowed(&allowed(), Some(&upper)));
        assert!(!host_allowed(&allowed(), Some(&rebinding)));
        assert!(!host_allowed(&allowed(), Some(&wrong_port)));
        assert!(!host_allowed(&allowed(), None));
    }

    #[test]
    fn requests_without_a_peer_share_one_rate_limit_bucket() {
        assert_eq!(peer_ip(None), IpAddr::from([0, 0, 0, 0]));
        let peer = PeerAddr { addr: "192.168.1.5:50000".parse().unwrap(), tls: true };
        assert_eq!(peer_ip(Some(peer)), IpAddr::from([192, 168, 1, 5]));
        assert!(is_tls(Some(peer)) && !is_tls(None));
    }
}
