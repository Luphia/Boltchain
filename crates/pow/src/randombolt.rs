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
/// never be used by two at once, which the surrounding `Mutex` guarantees.
struct Vm(RandomXVM);
// SAFETY: RandomX VMs and caches are plain heap allocations with no thread affinity; the cache is
// read-only after initialisation and every VM is only reached through a `Mutex`.
#[allow(unsafe_code)] // the only unsafe in Boltchain; see SAFETY above
unsafe impl Send for Vm {}

struct Slot {
    key: B256,
    vm: Mutex<Vm>,
}

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
        let vm = match self.mode {
            Mode::Light => RandomXVM::new(self.flags, Some(cache), None).map_err(err)?,
            Mode::Fast => {
                let dataset = RandomXDataset::new(self.flags, cache, 0).map_err(err)?;
                RandomXVM::new(self.flags, None, Some(dataset)).map_err(err)?
            }
        };
        let slot = Arc::new(Slot { key: *key, vm: Mutex::new(Vm(vm)) });
        let mut slots = self.slots.lock();
        slots.retain(|s| s.key != *key);
        slots.push(slot.clone());
        if slots.len() > 2 {
            slots.remove(0);
        }
        Ok(slot)
    }

    /// RandomBOLT(`key`, `input`).
    pub fn hash(&self, key: &B256, input: &[u8]) -> Result<B256, PowError> {
        let slot = self.slot(key)?;
        let vm = slot.vm.lock();
        let out = vm.0.calculate_hash(input).map_err(|e| PowError::RandomX(e.to_string()))?;
        Ok(B256::from_slice(&out))
    }

    /// Hashes `inputs` in one go (lets RandomX pipeline them; used by miners).
    pub fn hash_batch(&self, key: &B256, inputs: &[&[u8]]) -> Result<Vec<B256>, PowError> {
        let slot = self.slot(key)?;
        let vm = slot.vm.lock();
        let out = vm.0.calculate_hash_set(inputs).map_err(|e| PowError::RandomX(e.to_string()))?;
        Ok(out.iter().map(|h| B256::from_slice(h)).collect())
    }
}
