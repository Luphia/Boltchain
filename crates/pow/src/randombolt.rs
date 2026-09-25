//! RandomBOLT hashing with cached light-mode VMs per key.

use alloy_primitives::B256;
use parking_lot::Mutex;
use randomx_rs::{RandomXCache, RandomXDataset, RandomXFlag, RandomXVM};
use std::sync::Arc;

/// Why hashing failed.
#[derive(Debug, thiserror::Error)]
pub enum PowError {
    /// RandomX could not allocate or initialise.
    #[error("randomx: {0}")]
    RandomX(String),
}

/// Light (256 MiB cache, verification) or fast (2 GiB dataset, mining).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Cache only: slower hashes, small memory. Every node verifies this way.
    Light,
    /// Full dataset: ~8x faster hashes, 2 GiB of memory. For miners.
    Fast,
}

/// A VM bound to one key. RandomX VMs hold raw pointers: they may move between threads but must
/// never be used by two at once, which taking them out of the pool guarantees.
struct Vm(RandomXVM);

/// The key's cache (light mode) or dataset (fast mode), shared read-only by all its VMs.
struct Shared {
    cache: Option<RandomXCache>,
    dataset: Option<RandomXDataset>,
}

// SAFETY (the only unsafe code in Boltchain): RandomX VMs, caches and datasets are plain heap
// allocations with no thread affinity. Caches and datasets are read-only once initialised, and
// RandomX supports many VMs over one cache or dataset on different threads. Each VM is used by
// one thread at a time: it is owned by whoever took it from the pool.
#[allow(unsafe_code)]
unsafe impl Send for Vm {}
#[allow(unsafe_code)]
unsafe impl Send for Shared {}
#[allow(unsafe_code)]
unsafe impl Sync for Shared {}

/// Everything for one key: the shared cache (or dataset) and idle VMs over it, so several threads
/// can hash at once (each VM only adds a 2 MiB scratchpad).
struct Slot {
    key: B256,
    shared: Shared,
    idle: Mutex<Vec<Vm>>,
}

/// Idle VMs kept per key.
const MAX_IDLE_VMS: usize = 16;

/// Computes RandomBOLT hashes, keeping VMs for the two most recent keys (a key change happens
/// every 2,048 blocks; reorgs across one need the previous key).
pub struct Hasher {
    mode: Mode,
    flags: RandomXFlag,
    slots: Mutex<Vec<Arc<Slot>>>,
}

impl std::fmt::Debug for Hasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hasher").field("mode", &self.mode).finish()
    }
}

impl Hasher {
    /// A hasher in `mode` with the CPU's recommended RandomX flags (JIT, AES where available).
    pub fn new(mode: Mode) -> Self {
        let mut flags = RandomXFlag::get_recommended_flags();
        if mode == Mode::Fast {
            flags |= RandomXFlag::FLAG_FULL_MEM;
        }
        Self { mode, flags, slots: Mutex::new(Vec::new()) }
    }

    /// Mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    fn slot(&self, key: &B256) -> Result<Arc<Slot>, PowError> {
        if let Some(s) = self.slots.lock().iter().find(|s| s.key == *key) {
            return Ok(s.clone());
        }
        let err = |e: randomx_rs::RandomXError| PowError::RandomX(e.to_string());
        let cache = RandomXCache::new(self.flags, key.as_slice()).map_err(err)?;
        let (cache, dataset) = match self.mode {
            Mode::Light => (Some(cache), None),
            Mode::Fast => (None, Some(RandomXDataset::new(self.flags, cache, 0).map_err(err)?)),
        };
        let slot = Arc::new(Slot {
            key: *key,
            shared: Shared { cache, dataset },
            idle: Mutex::new(Vec::new()),
        });
        let mut slots = self.slots.lock();
        slots.retain(|s| s.key != *key);
        slots.push(slot.clone());
        if slots.len() > 2 {
            slots.remove(0);
        }
        Ok(slot)
    }

    /// Runs `f` on a VM for `key` taken from the pool (created if none is idle).
    fn with_vm<T>(
        &self,
        key: &B256,
        f: impl FnOnce(&RandomXVM) -> Result<T, randomx_rs::RandomXError>,
    ) -> Result<T, PowError> {
        let err = |e: randomx_rs::RandomXError| PowError::RandomX(e.to_string());
        let slot = self.slot(key)?;
        let idle = slot.idle.lock().pop();
        let vm = match idle {
            Some(vm) => vm,
            None => Vm(RandomXVM::new(
                self.flags,
                slot.shared.cache.clone(),
                slot.shared.dataset.clone(),
            )
            .map_err(err)?),
        };
        let out = f(&vm.0).map_err(err);
        let mut idle = slot.idle.lock();
        if idle.len() < MAX_IDLE_VMS {
            idle.push(vm);
        }
        out
    }

    /// RandomBOLT(`key`, `input`).
    pub fn hash(&self, key: &B256, input: &[u8]) -> Result<B256, PowError> {
        self.with_vm(key, |vm| vm.calculate_hash(input)).map(|h| B256::from_slice(&h))
    }

    /// Hashes `inputs` in one go (lets RandomX pipeline them; used by miners).
    pub fn hash_batch(&self, key: &B256, inputs: &[&[u8]]) -> Result<Vec<B256>, PowError> {
        let out = self.with_vm(key, |vm| vm.calculate_hash_set(inputs))?;
        Ok(out.iter().map(|h| B256::from_slice(h)).collect())
    }
}
