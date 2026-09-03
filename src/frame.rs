//! The frame tap — every byte that crosses a connection is recorded here for the
//! inspector to read, with a best-effort classification from the byte shape.
//! Ported from the `Tap` / `Frame` / `classify` parts of the playground's
//! `transport.ts`.
//!
//! The TS `Tap` carried a `now` closure and a listener set; here the pump owns
//! the clock (it passes `at` into [`Tap::record`]) and the UI polls
//! [`Tap::frames`], so both are gone.

use serde::Serialize;

/// `[MAGIC:2][VERSION:1][ir_hash:8][wire:8][framing:8][caps:4]`
const HANDSHAKE_LEN: usize = 31;

/// Best-effort frame classification for the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameKind {
    Handshake,
    Request,
    Response,
}

/// One recorded frame.
#[derive(Clone, Debug, Serialize)]
pub struct Frame {
    /// Monotonic, assigned when the frame is recorded.
    pub seq: u32,
    pub from: String,
    pub to: String,
    /// The raw bytes on the wire.
    pub bytes: Vec<u8>,
    /// `Clock::now()` at send.
    pub at: f64,
    pub kind: FrameKind,
    /// What the wire's fault spec did to this frame, if anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault: Option<String>,
}

/// Best-effort label for the log. The handshake frame is unambiguous (fixed
/// length + `CO` magic); a JSON-RPC frame's `method` key marks it a request. A
/// binary datagram request and response can't be told apart from the bytes
/// alone, so those default to [`FrameKind::Request`] — the engine overrides
/// `kind` when it knows the direction.
pub fn classify(bytes: &[u8]) -> FrameKind {
    if bytes.len() == HANDSHAKE_LEN && bytes.first() == Some(&0x43) && bytes.get(1) == Some(&0x4f) {
        return FrameKind::Handshake;
    }
    if bytes.first() == Some(&0x7b) {
        // `{` — a JSON-RPC frame; the `"method"` key marks the request side.
        if let Ok(text) = std::str::from_utf8(bytes) {
            return if text.contains("\"method\"") {
                FrameKind::Request
            } else {
                FrameKind::Response
            };
        }
    }
    FrameKind::Request
}

/// A shared sink for the frames on one connection.
#[derive(Debug, Default)]
pub struct Tap {
    counter: u32,
    pub frames: Vec<Frame>,
}

impl Tap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `bytes` crossing `from` → `to` at clock time `at`, tagged with an
    /// optional `fault` note. `kind_hint` is the sender's known direction — used
    /// unless the bytes are unambiguously a handshake (a binary datagram request
    /// and response are otherwise indistinguishable). Returns the new frame's
    /// `seq`.
    pub fn record(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        bytes: &[u8],
        at: f64,
        kind_hint: FrameKind,
        fault: Option<String>,
    ) -> u32 {
        let kind = match classify(bytes) {
            FrameKind::Handshake => FrameKind::Handshake,
            _ => kind_hint,
        };
        self.counter += 1;
        self.frames.push(Frame {
            seq: self.counter,
            from: from.into(),
            to: to.into(),
            bytes: bytes.to_vec(),
            at,
            kind,
            fault,
        });
        self.counter
    }

    pub fn clear(&mut self) {
        self.frames.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake_frame() -> Vec<u8> {
        let mut f = vec![0u8; HANDSHAKE_LEN];
        f[0] = 0x43; // 'C'
        f[1] = 0x4f; // 'O'
        f
    }

    #[test]
    fn classifies_a_handshake_by_magic_and_length() {
        assert_eq!(classify(&handshake_frame()), FrameKind::Handshake);
        // right magic, wrong length → not a handshake
        let mut short = handshake_frame();
        short.truncate(20);
        assert_ne!(classify(&short), FrameKind::Handshake);
    }

    #[test]
    fn classifies_json_rpc_by_the_method_key() {
        let req = br#"{"jsonrpc":"2.0","method":"send","id":1}"#;
        let res = br#"{"jsonrpc":"2.0","result":{"seq":1},"id":1}"#;
        assert_eq!(classify(req), FrameKind::Request);
        assert_eq!(classify(res), FrameKind::Response);
    }

    #[test]
    fn a_binary_datagram_defaults_to_request() {
        assert_eq!(classify(&[0x00, 0x01, 0x02, 0x03]), FrameKind::Request);
    }

    #[test]
    fn tap_numbers_frames_and_keeps_them_in_order() {
        let mut tap = Tap::new();
        let s1 = tap.record("a", "b", &[1, 2, 3], 0.0, FrameKind::Request, None);
        let s2 = tap.record(
            "b",
            "a",
            &[4, 5],
            1.0,
            FrameKind::Response,
            Some("dropped".into()),
        );

        assert_eq!((s1, s2), (1, 2));
        assert_eq!(tap.frames.len(), 2);
        assert_eq!(tap.frames[0].from, "a");
        assert_eq!(tap.frames[0].bytes, vec![1, 2, 3]);
        assert_eq!(
            tap.frames[1].kind,
            FrameKind::Response,
            "the hint stands for a datagram"
        );
        assert_eq!(tap.frames[1].fault.as_deref(), Some("dropped"));

        tap.clear();
        assert!(tap.frames.is_empty());
        // the counter keeps climbing after a clear
        assert_eq!(tap.record("a", "b", &[], 2.0, FrameKind::Request, None), 3);
    }

    #[test]
    fn a_handshake_ignores_the_direction_hint() {
        let mut tap = Tap::new();
        tap.record("a", "b", &handshake_frame(), 0.0, FrameKind::Request, None);
        assert_eq!(tap.frames[0].kind, FrameKind::Handshake);
    }

    #[test]
    fn frame_serializes_without_a_null_fault() {
        let mut tap = Tap::new();
        tap.record("a", "b", &[9], 0.0, FrameKind::Request, None);
        let json = serde_json::to_string(&tap.frames[0]).unwrap();
        assert!(!json.contains("fault"), "{json}");
        assert!(json.contains("\"kind\":\"request\""), "{json}");
    }
}
