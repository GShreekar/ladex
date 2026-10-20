//! `ladex receive`: the command line as a device files are sent to, which saves what it accepts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ladex_core::control::server::{self, Endpoint};
use ladex_core::control::{Event, Offer, Reply, Request, Settings};
use ladex_core::store::CHUNK_SIZE;
use ladex_core::testing::{eventually, eventually_within, spawn_mesh, NodeHandle};
use ladex_core::types::SessionId;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::task::JoinHandle;

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);
const DEVICE_NAME: &str = "lab-pc";

async fn control_of(node: &NodeHandle, data_dir: &Path) -> Endpoint {
    std::fs::create_dir_all(data_dir).unwrap();
    let (listener, endpoint) = server::open(data_dir).await.unwrap();
    let settings = Settings {
        data_dir: data_dir.to_path_buf(),
        discovery_port: None,
        local_http_port: None,
        use_keychain: false,
        manual_peers: Vec::new(),
    };
    tokio::spawn(server::serve(node.state.clone(), settings, listener, endpoint.clone()));
    endpoint
}

/// A `ladex receive` running against `endpoint`, with the events it has heard so far.
struct Receiver {
    task: JoinHandle<anyhow::Result<Reply>>,
    events: UnboundedReceiver<Event>,
}

impl Receiver {
    fn start(endpoint: &Endpoint) -> Self {
        let (tell, events) = mpsc::unbounded_channel();
        let endpoint = endpoint.clone();
        let task = tokio::spawn(async move {
            server::call(&endpoint, &Request::Receive { name: DEVICE_NAME.into() }, |event| {
                let _ = tell.send(event.clone());
            })
            .await
        });
        Self { task, events }
    }

    async fn joined(&mut self) -> SessionId {
        match self.events.recv().await {
            Some(Event::Joined { device }) => device,
            other => panic!("expected to join first, got {other:?}"),
        }
    }

    async fn next_offer(&mut self) -> Offer {
        match tokio::time::timeout(TRANSFER_TIMEOUT, self.events.recv()).await {
            Ok(Some(Event::Incoming { offer })) => offer,
            other => panic!("expected an offer, got {other:?}"),
        }
    }
}

async fn sees_device(node: &NodeHandle) -> bool {
    node.state.local_peers.read().await.values().any(|device| !device.left && device.nickname.as_deref() == Some(DEVICE_NAME))
}

fn three_chunks() -> Vec<u8> {
    (0..(2 * CHUNK_SIZE as usize + 4321)).map(|i| (i % 251) as u8).collect()
}

/// Two connected nodes, a receiving device on the second that the first can see, and the file the first will send.
async fn sender_and_receiver(dir: &Path) -> (Vec<NodeHandle>, Endpoint, Endpoint, Receiver, SessionId, PathBuf) {
    let nodes = spawn_mesh(2, None).await;
    let (sending, receiving) = (control_of(&nodes[0], &dir.join("a")).await, control_of(&nodes[1], &dir.join("b")).await);
    let mut receiver = Receiver::start(&receiving);
    let device = receiver.joined().await;
    assert!(eventually(async || sees_device(&nodes[0]).await).await, "the sending node never saw the device");
    let source = dir.join("holiday.jpg");
    std::fs::write(&source, three_chunks()).unwrap();
    (nodes, sending, receiving, receiver, device, source)
}

fn send_to_device(endpoint: &Endpoint, source: PathBuf) -> JoinHandle<anyhow::Result<Reply>> {
    let endpoint = endpoint.clone();
    tokio::spawn(async move { server::call(&endpoint, &Request::Send { paths: vec![source], to: Some(DEVICE_NAME.into()) }, |_| {}).await })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_sent_to_the_receiving_device_is_offered_with_its_name_and_size() {
    let dir = tempfile::tempdir().unwrap();
    let (_nodes, sending, _receiving, mut receiver, _device, source) = sender_and_receiver(dir.path()).await;
    let _sent = send_to_device(&sending, source);

    let offer = receiver.next_offer().await;
    assert_eq!((offer.name.as_str(), offer.size, offer.is_direct), ("holiday.jpg", three_chunks().len() as u64, true));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_accepted_file_is_saved_intact_and_the_sender_hears_it_was_delivered() {
    let dir = tempfile::tempdir().unwrap();
    let (nodes, sending, receiving, mut receiver, device, source) = sender_and_receiver(dir.path()).await;
    let sent = send_to_device(&sending, source);
    let offer = receiver.next_offer().await;

    let accept = Request::AcceptOffer { device, file: offer.file_id.clone(), into: dir.path().join("out") };
    let received = server::call(&receiving, &accept, |_| {}).await.unwrap();

    let path = dir.path().canonicalize().unwrap().join("out/holiday.jpg");
    assert_eq!(received, Reply::Received { name: "holiday.jpg".into(), path: path.clone() });
    assert_eq!(std::fs::read(path).unwrap(), three_chunks());
    let delivered = tokio::time::timeout(TRANSFER_TIMEOUT, sent).await.unwrap().unwrap().unwrap();
    assert_eq!(delivered, Reply::Pushed { delivered: vec!["holiday.jpg".into()], declined: vec![] });
    assert!(nodes[1].state.store.get(&offer.file_id).is_none(), "the receiving node kept its copy after saving it");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declined_file_is_reported_to_the_sender_as_declined() {
    let dir = tempfile::tempdir().unwrap();
    let (_nodes, sending, receiving, mut receiver, device, source) = sender_and_receiver(dir.path()).await;
    let sent = send_to_device(&sending, source);
    let offer = receiver.next_offer().await;

    let decline = Request::DeclineOffer { device, file: offer.file_id, from: offer.from };
    server::call(&receiving, &decline, |_| {}).await.unwrap();

    let answer = tokio::time::timeout(TRANSFER_TIMEOUT, sent).await.unwrap().unwrap().unwrap();
    assert_eq!(answer, Reply::Pushed { delivered: vec![], declined: vec!["holiday.jpg".into()] });
}

#[tokio::test(flavor = "multi_thread")]
async fn the_receiving_device_leaves_the_mesh_when_the_command_stops() {
    let dir = tempfile::tempdir().unwrap();
    let (nodes, _sending, _receiving, receiver, device, _source) = sender_and_receiver(dir.path()).await;

    receiver.task.abort();

    let is_gone = async || !nodes[1].state.local_peers.read().await.contains_key(&device);
    assert!(eventually_within(Duration::from_secs(10), is_gone).await, "the device stayed after its command stopped");
    assert!(eventually(async || !sees_device(&nodes[0]).await).await, "the other node still lists the device");
}
