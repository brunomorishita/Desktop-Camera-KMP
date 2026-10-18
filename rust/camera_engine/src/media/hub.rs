use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;

// Fan-out of captured frames to every consumer (preview windows and encoder)
pub struct FrameHub<F> {
    subscribers: Mutex<Vec<(u64, Sender<F>)>>,
    next_id: AtomicU64,
}

impl<F> Default for FrameHub<F> {
    fn default() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
        }
    }
}

impl<F: Clone> FrameHub<F> {
    pub fn subscribe(&self, capacity: usize) -> (u64, Receiver<F>) {
        let (sender, receiver) = bounded(capacity);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.subscribers.lock().push((id, sender));
        (id, receiver)
    }

    pub fn unsubscribe(&self, id: u64) {
        self.subscribers.lock().retain(|(sub_id, _)| *sub_id != id);
    }

    // Lets producers skip work (e.g. decoding) whose result nobody would receive
    #[cfg_attr(windows, allow(dead_code))]
    pub fn has_subscribers(&self) -> bool {
        !self.subscribers.lock().is_empty()
    }

    pub fn publish(&self, frame: F) {
        // Never blocks the capture thread: a consumer that is behind just misses this frame
        for (_, sender) in self.subscribers.lock().iter() {
            let _ = sender.try_send(frame.clone());
        }
    }
}
