// Accepts connections (TLS or plain) and serves the warp routes over hyper.
//
// warp 0.4's own server doesn't expose the client's address to filters, and the
// old TLS proxy in front of it hid every client behind 127.0.0.1. Serving the
// connections ourselves lets us attach the real peer address to each request as
// a `PeerAddr` extension, which the rate limiters need.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower_service::Service;

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
pub struct PeerAddr(pub SocketAddr);

// Key used for rate limiting; requests without an address share one bucket.
pub fn peer_ip(peer: Option<PeerAddr>) -> IpAddr {
    peer.map(|p| p.0.ip()).unwrap_or(IpAddr::from([0, 0, 0, 0]))
}

pub async fn serve<S>(addr: SocketAddr, tls: Option<TlsAcceptor>, routes: S) -> anyhow::Result<()>
where
    S: Service<hyper::Request<Incoming>, Response = warp::reply::Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
{
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("Serving on {addr} ({})", if tls.is_some() { "TLS" } else { "plain HTTP" });

    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!("Accept error: {e}");
                continue;
            }
        };
        let _ = tcp.set_nodelay(true);
        let tls = tls.clone();
        let routes = routes.clone();

        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |mut req: hyper::Request<Incoming>| {
                req.extensions_mut().insert(PeerAddr(peer));
                let mut routes = routes.clone();
                async move { routes.call(req).await }
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
}
