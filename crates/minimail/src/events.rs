// In-process pub/sub hub for SSE + the typed event schema (SPEC §5/§8). Pure std.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;

/// The ONLY producer of SSE frame strings. UI and every publisher share this.
pub enum Event {
    Message(String), // full Summary JSON (already serialized)
    Delete(String),  // message id
    Clear,
}

impl Event {
    /// Render the exact SSE frame, e.g. "event: message\ndata: {..}\n\n".
    /// Message -> event: message, data: <summary json>
    /// Delete  -> event: delete,  data: {"id":"<id>"}
    /// Clear   -> event: clear,   data: {}
    pub fn frame(&self) -> String {
        match self {
            // ids are constrained by store::valid_id (no quotes/control chars),
            // so a raw interpolation is a well-formed JSON object here.
            Event::Message(json) => format!("event: message\ndata: {json}\n\n"),
            Event::Delete(id) => format!("event: delete\ndata: {{\"id\":\"{id}\"}}\n\n"),
            Event::Clear => String::from("event: clear\ndata: {}\n\n"),
        }
    }
}

pub struct Hub {
    subs: Mutex<Vec<Sender<String>>>,
}

impl Hub {
    pub fn new() -> Self {
        Hub {
            subs: Mutex::new(Vec::new()),
        }
    }

    pub fn subscribe(&self) -> Receiver<String> {
        let (tx, rx) = channel();
        self.subs.lock().unwrap().push(tx);
        rx
    }

    /// Render `ev.frame()` and send to all subscribers; drop senders that Err (closed).
    /// std mpsc is unbounded, so `send` never blocks a publisher — it only fails when
    /// the receiving SSE thread is gone, which is exactly when we reap.
    pub fn publish(&self, ev: Event) {
        let frame = ev.frame();
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|tx| tx.send(frame.clone()).is_ok());
    }

    #[allow(dead_code)] // frozen SPEC §5 Hub API; exercised only by unit tests
    pub fn subscriber_count(&self) -> usize {
        self.subs.lock().unwrap().len()
    }
}

impl Default for Hub {
    fn default() -> Self {
        Hub::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_frame_formatting() {
        let ev = Event::Message(String::from("{\"id\":\"x\"}"));
        assert_eq!(ev.frame(), "event: message\ndata: {\"id\":\"x\"}\n\n");
    }

    #[test]
    fn delete_frame_formatting() {
        let ev = Event::Delete(String::from("0001-2-3a"));
        assert_eq!(
            ev.frame(),
            "event: delete\ndata: {\"id\":\"0001-2-3a\"}\n\n"
        );
    }

    #[test]
    fn clear_frame_formatting() {
        assert_eq!(Event::Clear.frame(), "event: clear\ndata: {}\n\n");
    }

    #[test]
    fn frame_ends_with_blank_line() {
        // every frame is terminated by an empty line (the SSE record separator)
        for ev in [
            Event::Message(String::from("{}")),
            Event::Delete(String::from("id")),
            Event::Clear,
        ] {
            assert!(ev.frame().ends_with("\n\n"));
        }
    }

    #[test]
    fn subscribe_then_broadcast_delivers() {
        let hub = Hub::new();
        let rx = hub.subscribe();
        assert_eq!(hub.subscriber_count(), 1);
        hub.publish(Event::Clear);
        assert_eq!(rx.recv().unwrap(), "event: clear\ndata: {}\n\n");
    }

    #[test]
    fn broadcast_reaches_all_subscribers() {
        let hub = Hub::new();
        let a = hub.subscribe();
        let b = hub.subscribe();
        hub.publish(Event::Delete(String::from("42")));
        let want = "event: delete\ndata: {\"id\":\"42\"}\n\n";
        assert_eq!(a.recv().unwrap(), want);
        assert_eq!(b.recv().unwrap(), want);
    }

    #[test]
    fn dead_subscriber_is_reaped_on_publish() {
        let hub = Hub::new();
        let rx = hub.subscribe();
        assert_eq!(hub.subscriber_count(), 1);
        drop(rx); // receiver gone -> next send errors
        hub.publish(Event::Clear);
        assert_eq!(hub.subscriber_count(), 0);
    }

    #[test]
    fn live_subscriber_survives_while_dead_one_reaped() {
        let hub = Hub::new();
        let dead = hub.subscribe();
        let live = hub.subscribe();
        drop(dead);
        hub.publish(Event::Message(String::from("{}")));
        assert_eq!(hub.subscriber_count(), 1);
        assert_eq!(live.recv().unwrap(), "event: message\ndata: {}\n\n");
    }
}
