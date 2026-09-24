//! Test helpers shared by unit tests.

use parking_lot::Mutex;
use space3d_replication::transport::{CloseCode, Lane, LinkTx, TransportCaps, TransportError};

/// A `LinkTx` that records what was sent.
#[derive(Default)]
pub struct FakeLinkTx {
    sent: Mutex<Vec<(Lane, Vec<u8>)>>,
    closed: Mutex<Option<(CloseCode, String)>>,
}

impl FakeLinkTx {
    /// Packets sent so far.
    #[allow(dead_code)]
    pub fn sent(&self) -> Vec<(Lane, Vec<u8>)> {
        self.sent.lock().clone()
    }

    /// Close code + reason, once closed.
    pub fn closed(&self) -> Option<(CloseCode, String)> {
        self.closed.lock().clone()
    }
}

impl LinkTx for FakeLinkTx {
    fn caps(&self) -> TransportCaps {
        TransportCaps::stream(16 * 1024)
    }

    fn send(&self, lane: Lane, packet: Vec<u8>) -> Result<(), TransportError> {
        if self.closed.lock().is_some() {
            return Err(TransportError::Closed);
        }
        self.sent.lock().push((lane, packet));
        Ok(())
    }

    fn backlog_bytes(&self) -> usize {
        0
    }

    fn close(&self, code: CloseCode, reason: &str) {
        self.closed.lock().get_or_insert((code, reason.to_owned()));
    }

    fn is_open(&self) -> bool {
        self.closed.lock().is_none()
    }
}
