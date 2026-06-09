/// Wire protocol messages and stale-move dropper.
use serde::{Deserialize, Serialize};

/// Messages sent from the web client to the server.
///
/// `x` and `y` are normalized coordinates in [0,1] relative to the video content rect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Pointer / touch down.
    Down { x: f64, y: f64 },
    /// Pointer / touch move.  `seq` is a monotonically increasing counter used
    /// by `MoveDropper` to discard out-of-order duplicates.
    Move { x: f64, y: f64, seq: u64 },
    /// Pointer / touch up.
    Up { x: f64, y: f64 },
    /// Press a named key (e.g. "Home", "VolumeUp").
    Key { name: String },
    /// Type a string of text.
    Text { s: String },
    /// Execute a named shortcut (e.g. "cmd+c").
    Shortcut { name: String },
    /// Client requests exclusive input control.
    Acquire,
    /// Client releases exclusive input control.
    Release,
}

/// Drop stale `Move` messages that arrive out of order.
///
/// Only `Move` events carry a `seq`; all other event types are never dropped.
/// Call `accept(seq)` on each incoming `Move`; returns `true` if the message
/// should be processed, `false` if it should be discarded.
#[derive(Debug, Default)]
pub struct MoveDropper {
    /// The highest sequence number seen so far.  `None` = nothing seen yet.
    last_seq: Option<u64>,
}

impl MoveDropper {
    pub fn new() -> Self {
        Self { last_seq: None }
    }

    /// Accept `seq` if it is strictly greater than the last accepted sequence,
    /// or if this is the very first call.  Returns `true` and advances the
    /// internal counter; returns `false` (drop) otherwise.
    pub fn accept(&mut self, seq: u64) -> bool {
        match self.last_seq {
            None => {
                self.last_seq = Some(seq);
                true
            }
            Some(last) if seq > last => {
                self.last_seq = Some(seq);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- serde round-trip tests ---

    #[test]
    fn down_roundtrip() {
        let msg = ClientMsg::Down { x: 0.3, y: 0.7 };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn move_roundtrip() {
        let msg = ClientMsg::Move { x: 0.1, y: 0.9, seq: 42 };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn up_roundtrip() {
        let msg = ClientMsg::Up { x: 0.5, y: 0.5 };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn key_roundtrip() {
        let msg = ClientMsg::Key { name: "Home".to_string() };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn text_roundtrip() {
        let msg = ClientMsg::Text { s: "Hello, 世界".to_string() };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn shortcut_roundtrip() {
        let msg = ClientMsg::Shortcut { name: "cmd+c".to_string() };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn acquire_roundtrip() {
        let msg = ClientMsg::Acquire;
        let json = serde_json::to_string(&msg).unwrap();
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn release_roundtrip() {
        let msg = ClientMsg::Release;
        let json = serde_json::to_string(&msg).unwrap();
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    // --- MoveDropper tests ---

    #[test]
    fn dropper_accepts_first_seq() {
        let mut d = MoveDropper::new();
        assert!(d.accept(1));
    }

    #[test]
    fn dropper_accepts_increasing_seq() {
        let mut d = MoveDropper::new();
        assert!(d.accept(1));
        assert!(d.accept(2));
        assert!(d.accept(100));
    }

    #[test]
    fn dropper_rejects_equal_seq() {
        let mut d = MoveDropper::new();
        assert!(d.accept(5));
        assert!(!d.accept(5));
    }

    #[test]
    fn dropper_rejects_older_seq() {
        let mut d = MoveDropper::new();
        assert!(d.accept(10));
        assert!(!d.accept(9));
        assert!(!d.accept(1));
    }

    #[test]
    fn dropper_out_of_order_keeps_newest() {
        let mut d = MoveDropper::new();
        assert!(d.accept(5));
        assert!(!d.accept(3));   // out-of-order old, dropped
        assert!(d.accept(6));    // new seq accepted
        assert!(!d.accept(4));   // still older than 6
    }

    #[test]
    fn dropper_seq_zero_is_accepted_first() {
        let mut d = MoveDropper::new();
        assert!(d.accept(0));
        assert!(!d.accept(0));
        assert!(d.accept(1));
    }
}
