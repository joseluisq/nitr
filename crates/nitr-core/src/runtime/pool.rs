//! A fixed pool of independent Lua runtimes checked out per request.

use std::ops::{Deref, DerefMut};

use crate::runtime::Runtime;

/// A fixed pool of [`Runtime`]s over an MPMC channel.
///
/// A request checks a runtime out with [`get()`](Self::get) and uses it
/// exclusively; the returned [`RuntimeGuard`] sends it back on drop. When all
/// runtimes are busy, `get()` waits fairly (FIFO) — the channel is the
/// backpressure mechanism.
#[derive(Debug)]
pub struct RuntimePool {
    tx: async_channel::Sender<Runtime>,
    rx: async_channel::Receiver<Runtime>,
}

impl RuntimePool {
    /// Creates a pool holding the given runtimes.
    pub fn new(runtimes: Vec<Runtime>) -> Self {
        let (tx, rx) = async_channel::bounded(runtimes.len().max(1));
        for rt in runtimes {
            // Cannot fail: the channel capacity equals the number of runtimes.
            let _ = tx.try_send(rt);
        }
        Self { tx, rx }
    }

    /// Number of runtimes owned by the pool.
    pub fn size(&self) -> usize {
        self.tx.capacity().unwrap_or(0)
    }

    /// Checks a runtime out of the pool, waiting until one is available.
    pub async fn get(&self) -> RuntimeGuard {
        let rt = self
            .rx
            .recv()
            .await
            .expect("runtime pool channel cannot be closed while the pool is alive");
        RuntimeGuard {
            rt: Some(rt),
            tx: self.tx.clone(),
        }
    }
}

/// RAII guard over a checked-out [`Runtime`]; returns it to the pool on drop.
#[derive(Debug)]
pub struct RuntimeGuard {
    rt: Option<Runtime>,
    tx: async_channel::Sender<Runtime>,
}

impl Deref for RuntimeGuard {
    type Target = Runtime;
    fn deref(&self) -> &Self::Target {
        self.rt.as_ref().expect("runtime present until drop")
    }
}

impl DerefMut for RuntimeGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.rt.as_mut().expect("runtime present until drop")
    }
}

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            // Cannot fail: capacity equals the number of runtimes and the
            // receiver lives as long as the pool.
            let _ = self.tx.try_send(rt);
        }
    }
}
