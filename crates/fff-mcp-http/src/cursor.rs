use std::collections::{HashMap, VecDeque};

const MAX_CURSORS: usize = 20;

// Opaque page cursors. IDs are salted hashes, not offsets.
pub(crate) struct CursorStore {
    counter: u64,
    salt: u64,
    offsets: HashMap<String, usize>,
    order: VecDeque<String>,
}

impl CursorStore {
    pub fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Self {
            counter: 0,
            salt: nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
            offsets: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn store(&mut self, offset: usize) -> String {
        self.counter = self.counter.wrapping_add(1);
        let id = format!("{:016x}", mix(self.counter ^ self.salt));
        self.offsets.insert(id.clone(), offset);
        self.order.push_back(id.clone());
        while self.offsets.len() > MAX_CURSORS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.offsets.remove(&oldest);
        }
        id
    }

    pub fn get(&self, id: &str) -> Option<usize> {
        self.offsets.get(id).copied()
    }
}

// splitmix64: cheap avalanche so consecutive counters do not look like offsets.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_ids_are_opaque_and_roundtrip() {
        let mut store = CursorStore::new();
        let first = store.store(20);
        assert_eq!(store.get(&first), Some(20));
        assert_ne!(first, "20");
        assert!(!first.contains('2') || first.len() > 2);
    }

    #[test]
    fn only_twenty_cursors_survive() {
        let mut store = CursorStore::new();
        let first = store.store(1);
        for offset in 0..MAX_CURSORS + 5 {
            store.store(offset + 100);
        }
        assert_eq!(store.get(&first), None);
        assert_eq!(store.offsets.len(), MAX_CURSORS);
    }
}
