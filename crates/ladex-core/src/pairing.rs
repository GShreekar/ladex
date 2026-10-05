//! Pairing with another node: a time-limited window for incoming pairings, and codes waiting for each person's answer.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::oneshot;

use crate::handshake::Pairing;
use crate::types::NodeId;

pub const WINDOW: Duration = Duration::from_secs(300);
// Shorter than the handshake's wait for the other device, so this side always answers first.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);
// More than a few codes at once would only be someone flooding the screen.
const MAX_WAITING: usize = 3;
const MAX_RECENT: usize = 8;

/// A code waiting for the person at this node to compare it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Request {
    pub node_id: NodeId,
    pub name: String,
    pub words: Vec<&'static str>,
    /// Whether this node dialed the other, rather than the other way round.
    pub dialed: bool,
}

impl Request {
    pub fn for_pairing(pairing: &Pairing<'_>, dialed: bool) -> Self {
        Request { node_id: pairing.peer_node_id(), name: pairing.peer_name(), words: pairing.code().to_vec(), dialed }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaitingCode {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub name: String,
    pub paired: bool,
    pub message: String,
}

impl Outcome {
    pub fn of<T, E: std::fmt::Display>(name: &str, result: &Result<T, E>) -> Self {
        match result {
            Ok(_) => Outcome { name: name.to_string(), paired: true, message: "paired".to_string() },
            Err(e) => Outcome { name: name.to_string(), paired: false, message: e.to_string() },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub open: bool,
    pub seconds_left: u64,
    pub waiting: Vec<WaitingCode>,
    /// Newest first.
    pub recent: Vec<Outcome>,
}

#[derive(Default)]
pub struct Pairings {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    open_until: Option<Instant>,
    next_id: u64,
    waiting: HashMap<u64, Waiting>,
    recent: VecDeque<Outcome>,
}

struct Waiting {
    request: Request,
    answer: oneshot::Sender<bool>,
}

impl Pairings {
    pub fn new() -> Self {
        Self::default()
    }

    /// Accepts incoming pairings for the next few minutes.
    pub fn open(&self) {
        self.inner.lock().unwrap().open_until = Some(Instant::now() + WINDOW);
    }

    /// Stops accepting pairings and declines every code still waiting.
    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.open_until = None;
        inner.waiting.clear();
    }

    pub fn is_open(&self) -> bool {
        self.inner.lock().unwrap().open_until.is_some_and(|until| until > Instant::now())
    }

    pub fn status(&self) -> Status {
        let inner = self.inner.lock().unwrap();
        let seconds_left = inner.open_until.map_or(0, |until| until.saturating_duration_since(Instant::now()).as_secs());
        let mut waiting: Vec<WaitingCode> =
            inner.waiting.iter().map(|(id, waiting)| WaitingCode { id: *id, request: waiting.request.clone() }).collect();
        waiting.sort_by_key(|code| code.id);
        Status { open: seconds_left > 0, seconds_left, waiting, recent: inner.recent.iter().cloned().collect() }
    }

    /// Shows the code and waits for this person's answer; no answer in time is a no.
    pub async fn ask(&self, request: Request) -> bool {
        let Some((id, answer)) = self.show(request) else {
            return false;
        };
        let _shown = Shown { pairings: self, id };
        matches!(tokio::time::timeout(ANSWER_TIMEOUT, answer).await, Ok(Ok(true)))
    }

    /// Passes on this person's answer. False when no such code is waiting.
    pub fn answer(&self, id: u64, accepted: bool) -> bool {
        let Some(waiting) = self.inner.lock().unwrap().waiting.remove(&id) else {
            return false;
        };
        // The pairing may have given up already; then there is nobody left to tell.
        waiting.answer.send(accepted).is_ok()
    }

    pub fn record(&self, outcome: Outcome) {
        let mut inner = self.inner.lock().unwrap();
        inner.recent.push_front(outcome);
        inner.recent.truncate(MAX_RECENT);
    }

    fn show(&self, request: Request) -> Option<(u64, oneshot::Receiver<bool>)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.waiting.len() >= MAX_WAITING {
            tracing::warn!("Pairing: too many codes waiting; declined {} ({})", request.name, request.node_id);
            return None;
        }
        let (answer, answered) = oneshot::channel();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.waiting.insert(id, Waiting { request, answer });
        Some((id, answered))
    }
}

// Takes the code off the screen once its pairing stops waiting, however that happens.
struct Shown<'a> {
    pairings: &'a Pairings,
    id: u64,
}

impl Drop for Shown<'_> {
    fn drop(&mut self) {
        self.pairings.inner.lock().unwrap().waiting.remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn request(name: &str) -> Request {
        Request { node_id: format!("id_{name}"), name: name.to_string(), words: vec!["apple"; 6], dialed: false }
    }

    async fn ask_in_background(pairings: &Arc<Pairings>, name: &str) -> (u64, tokio::task::JoinHandle<bool>) {
        let asking = tokio::spawn({
            let pairings = pairings.clone();
            let request = request(name);
            async move { pairings.ask(request).await }
        });
        loop {
            if let Some(code) = pairings.status().waiting.into_iter().find(|code| code.request.name == name) {
                return (code.id, asking);
            }
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn pairing_is_closed_until_opened() {
        let pairings = Pairings::new();
        assert!(!pairings.is_open());
        assert_eq!(pairings.status().seconds_left, 0);
    }

    #[test]
    fn opening_accepts_pairings_for_the_window() {
        let pairings = Pairings::new();
        pairings.open();
        assert!(pairings.is_open());
        assert!(pairings.status().seconds_left > WINDOW.as_secs() - 5);
    }

    #[test]
    fn closing_stops_accepting_pairings() {
        let pairings = Pairings::new();
        pairings.open();
        pairings.close();
        assert!(!pairings.is_open());
    }

    #[tokio::test]
    async fn a_waiting_code_is_shown_until_answered() {
        let pairings = Arc::new(Pairings::new());
        let (id, asking) = ask_in_background(&pairings, "laptop").await;
        assert_eq!(pairings.status().waiting[0].request, request("laptop"));
        assert!(pairings.answer(id, true));
        assert!(asking.await.unwrap());
        assert!(pairings.status().waiting.is_empty());
    }

    #[tokio::test]
    async fn saying_no_declines_the_pairing() {
        let pairings = Arc::new(Pairings::new());
        let (id, asking) = ask_in_background(&pairings, "laptop").await;
        pairings.answer(id, false);
        assert!(!asking.await.unwrap());
    }

    #[tokio::test]
    async fn closing_declines_codes_still_waiting() {
        let pairings = Arc::new(Pairings::new());
        let (_, asking) = ask_in_background(&pairings, "laptop").await;
        pairings.close();
        assert!(!asking.await.unwrap());
    }

    #[test]
    fn answering_a_code_that_is_not_waiting_reports_it() {
        assert!(!Pairings::new().answer(7, true));
    }

    #[tokio::test]
    async fn only_a_few_codes_wait_at_once() {
        let pairings = Arc::new(Pairings::new());
        for name in ["a", "b", "c"] {
            ask_in_background(&pairings, name).await;
        }
        assert!(!pairings.ask(request("d")).await);
        assert_eq!(pairings.status().waiting.len(), MAX_WAITING);
    }

    #[test]
    fn recent_outcomes_are_newest_first_and_capped() {
        let pairings = Pairings::new();
        for i in 0..MAX_RECENT + 2 {
            pairings.record(Outcome::of::<(), String>(&format!("node {i}"), &Ok(())));
        }
        let recent = pairings.status().recent;
        assert_eq!(recent.len(), MAX_RECENT);
        assert_eq!(recent[0].name, format!("node {}", MAX_RECENT + 1));
    }

    #[test]
    fn a_failed_outcome_keeps_the_reason() {
        let outcome = Outcome::of::<(), &str>("laptop", &Err("declined on the other device"));
        assert_eq!((outcome.paired, outcome.message.as_str()), (false, "declined on the other device"));
    }
}
