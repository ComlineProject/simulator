//! The simulator's [`WireFormat`]. Phase 1 of the playground is JSON-only; the
//! runtime's own JSON / MessagePack formats live behind its `std` feature
//! (`rmp-serde`, `libloading`), which won't build for `wasm32`, so the simulator
//! carries its own.

use comline_runtime::contract::{BufMut, RuntimeError, WireFormat};
use serde::{Deserialize, Serialize};

/// JSON via `serde_json`, appended straight into the frame buffer.
pub struct Json;

impl WireFormat for Json {
    fn name(&self) -> &'static str {
        "json"
    }

    fn encode<T: Serialize + ?Sized>(
        &self,
        value: &T,
        out: &mut dyn BufMut,
    ) -> Result<(), RuntimeError> {
        let bytes = serde_json::to_vec(value).map_err(|_| RuntimeError::Serialization)?;
        out.put_slice(&bytes);
        Ok(())
    }

    fn decode<'de, T: Deserialize<'de>>(&self, bytes: &'de [u8]) -> Result<T, RuntimeError> {
        serde_json::from_slice(bytes).map_err(|_| RuntimeError::Serialization)
    }
}
