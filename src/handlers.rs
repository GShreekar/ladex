use crate::types::*;
use crate::AppState;
use warp::{Rejection, Reply};

pub async fn get_peers(state: AppState) -> Result<impl Reply, Rejection> {
    let peers = {
        let peers = state.peers.read().await;
        peers.values().cloned().collect::<Vec<_>>()
    };

    let stats = PeerStats {
        total_peers: peers.len(),
        peers,
    };

    Ok(warp::reply::json(&stats))
}
