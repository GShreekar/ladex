//! A relay that terminates TLS on both sides with its own certificate is caught by the handshake's channel binding.

use std::net::{Ipv4Addr, SocketAddr};

use ladex_core::mesh::AuthFailure;
use ladex_core::testing::{eventually, spawn_node, NodeConfig, NodeHandle};
use ladex_core::tls;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// Relays every connection to `target`; when `terminate_tls`, it decrypts and re-encrypts in between.
async fn spawn_relay(target: SocketAddr, terminate_tls: bool) -> SocketAddr {
    tls::install_crypto_provider();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (attacker_config, _) = tls::ephemeral_server_identity(&["127.0.0.1".to_string()]).unwrap();
    let acceptor = TlsAcceptor::from(attacker_config);
    let connector = TlsConnector::from(tls::build_client_config());
    tokio::spawn(async move {
        while let Ok((dialer, _)) = listener.accept().await {
            let (acceptor, connector) = (acceptor.clone(), connector.clone());
            tokio::spawn(async move {
                let Ok(server) = TcpStream::connect(target).await else { return };
                if !terminate_tls {
                    let (mut dialer, mut server) = (dialer, server);
                    let _ = tokio::io::copy_bidirectional(&mut dialer, &mut server).await;
                    return;
                }
                let Ok(mut dialer) = acceptor.accept(dialer).await else { return };
                let Ok(mut server) = connector.connect(ServerName::IpAddress(target.ip().into()), server).await else { return };
                let _ = tokio::io::copy_bidirectional(&mut dialer, &mut server).await;
            });
        }
    });
    addr
}

async fn two_nodes() -> (NodeHandle, NodeHandle) {
    let dialer = spawn_node(NodeConfig::named("dialer").passphrase("pw").tls()).await;
    let server = spawn_node(NodeConfig::named("server").passphrase("pw").tls()).await;
    (dialer, server)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_only_forwards_bytes_lets_the_nodes_connect() {
    let (dialer, server) = two_nodes().await;
    let relay = spawn_relay(server.addr, false).await;
    dialer.connect(relay).await.unwrap();
    assert!(eventually(async || server.mesh_peers().await.contains(dialer.node_id())).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_terminates_tls_twice_fails_confirmation() {
    let (dialer, server) = two_nodes().await;
    let relay = spawn_relay(server.addr, true).await;

    let failure = dialer.connect(relay).await.unwrap_err();
    assert!(failure.downcast_ref::<AuthFailure>().is_some(), "{failure}");
    assert!(failure.to_string().contains("failed to prove"), "{failure}");
    assert!(dialer.mesh_peers().await.is_empty());
    assert!(server.mesh_peers().await.is_empty());
}
