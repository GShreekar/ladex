//! Caps the uploads a node runs at once and shares the slots round-robin between the peers asking.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use crate::types::NodeId;

struct Queue {
    free: usize,
    waiting: HashMap<NodeId, VecDeque<oneshot::Sender<Slot>>>,
    // Peers with waiting requests, in the order they will be served.
    turn: VecDeque<NodeId>,
}

impl Queue {
    fn enqueue(&mut self, who: &NodeId, request: oneshot::Sender<Slot>) {
        let requests = self.waiting.entry(who.clone()).or_default();
        requests.retain(|r| !r.is_closed());
        requests.push_back(request);
        if !self.turn.contains(who) {
            self.turn.push_back(who.clone());
        }
    }

    fn next_waiter(&mut self) -> Option<oneshot::Sender<Slot>> {
        while let Some(who) = self.turn.pop_front() {
            let Some(requests) = self.waiting.get_mut(&who) else { continue };
            let request = requests.pop_front();
            if requests.is_empty() {
                self.waiting.remove(&who);
            } else {
                self.turn.push_back(who);
            }
            if request.is_some() {
                return request;
            }
        }
        None
    }

    fn hand_out(&mut self, shared: &Arc<Mutex<Queue>>) {
        while self.free > 0 {
            let Some(request) = self.next_waiter() else { break };
            match request.send(Slot { queue: Some(shared.clone()) }) {
                Ok(()) => self.free -= 1,
                // The waiter gave up, so the slot goes to the next one instead.
                Err(mut slot) => slot.queue = None,
            }
        }
    }
}

/// The node's upload slots.
#[derive(Clone)]
pub struct UploadSlots {
    queue: Arc<Mutex<Queue>>,
    capacity: usize,
}

/// One running upload; dropping it frees the slot for the next peer in line.
pub struct Slot {
    queue: Option<Arc<Mutex<Queue>>>,
}

impl UploadSlots {
    pub fn new(capacity: usize) -> Self {
        let queue = Queue { free: capacity, waiting: HashMap::new(), turn: VecDeque::new() };
        Self { queue: Arc::new(Mutex::new(queue)), capacity }
    }

    /// Waits for a slot; peers with several requests waiting take turns with the others.
    pub async fn acquire(&self, who: &NodeId) -> Slot {
        let granted = {
            let mut queue = self.queue.lock().unwrap();
            if queue.free > 0 {
                queue.free -= 1;
                return Slot { queue: Some(self.queue.clone()) };
            }
            let (sender, receiver) = oneshot::channel();
            queue.enqueue(who, sender);
            receiver
        };
        granted.await.expect("a waiting request is always answered before the slots are dropped")
    }

    pub fn free(&self) -> usize {
        self.queue.lock().unwrap().free
    }

    /// How many uploads are running right now.
    pub fn in_use(&self) -> usize {
        self.capacity - self.free()
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let Some(shared) = self.queue.take() else { return };
        let mut queue = shared.lock().unwrap();
        queue.free += 1;
        queue.hand_out(&shared);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc;

    use super::*;

    fn node(name: &str) -> NodeId {
        name.to_string()
    }

    async fn is_granted(slots: &UploadSlots, who: &NodeId) -> bool {
        tokio::time::timeout(Duration::from_millis(50), slots.acquire(who)).await.is_ok()
    }

    #[tokio::test(start_paused = true)]
    async fn no_more_uploads_run_than_there_are_slots() {
        let slots = UploadSlots::new(2);
        let _first = slots.acquire(&node("a")).await;
        let _second = slots.acquire(&node("b")).await;

        assert!(!is_granted(&slots, &node("c")).await);
    }

    #[tokio::test]
    async fn a_running_upload_is_counted_until_it_ends() {
        let slots = UploadSlots::new(2);
        let running = slots.acquire(&node("a")).await;
        assert_eq!(slots.in_use(), 1);

        drop(running);
        assert_eq!(slots.in_use(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_upload_frees_its_slot() {
        let slots = UploadSlots::new(1);
        let taken = slots.acquire(&node("a")).await;
        drop(taken);

        assert!(is_granted(&slots, &node("b")).await);
    }

    #[tokio::test]
    async fn peers_with_waiting_requests_take_turns() {
        let slots = UploadSlots::new(1);
        let held = slots.acquire(&node("hog")).await;
        let (granted_tx, mut granted) = mpsc::unbounded_channel();
        for name in ["a", "a", "a", "b"] {
            let (slots, who, granted_tx) = (slots.clone(), node(name), granted_tx.clone());
            tokio::spawn(async move {
                let _slot = slots.acquire(&who).await;
                granted_tx.send(who).unwrap();
            });
            tokio::task::yield_now().await;
        }
        drop(held);

        let mut order = Vec::new();
        for _ in 0..4 {
            order.push(granted.recv().await.unwrap());
        }
        assert_eq!(order, ["a", "b", "a", "a"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_that_gave_up_does_not_keep_a_slot() {
        let slots = UploadSlots::new(1);
        let held = slots.acquire(&node("a")).await;
        assert!(!is_granted(&slots, &node("b")).await);
        drop(held);

        assert_eq!(slots.free(), 1);
        assert!(is_granted(&slots, &node("c")).await);
    }
}
