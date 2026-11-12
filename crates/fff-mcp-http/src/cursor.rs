use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

const MAX_CURSORS: usize = 20;

// Where the next grep page resumes and which stage produced it.
#[derive(Clone, Copy, Default)]
pub(crate) struct GrepCursor {
    pub file_offset: usize,
    pub match_offset: usize,
    pub match_file: u64,
    pub stage: GrepStage,
    pub found_matches: bool,
    pub missing_path: bool,
}

impl GrepCursor {
    pub fn page_limit(self, requested: usize) -> usize {
        self.match_offset
            .saturating_add(requested)
            .saturating_add(1)
    }

    // Matches already shown from the file where the last page stopped.
    pub fn resume_skip(&self, result: &fff::GrepResult<'_>, picker: &fff::FilePicker) -> usize {
        match result.matches.first() {
            Some(first)
                if self.match_offset > 0
                    && file_id(result, first.file_index, picker) == self.match_file =>
            {
                result
                    .matches
                    .iter()
                    .take(self.match_offset)
                    .take_while(|m| m.file_index == first.file_index)
                    .count()
            }
            _ => 0,
        }
    }

    pub fn paginate(
        &mut self,
        result: &mut fff::GrepResult<'_>,
        requested: usize,
        picker: &fff::FilePicker,
    ) {
        let start = self.resume_skip(result, picker);
        let end = start.saturating_add(requested).min(result.matches.len());
        if end < result.matches.len() {
            let last_file = result.matches.last().unwrap().file_index;
            self.file_offset = if result.next_file_offset > 0 {
                result.next_file_offset - 1
            } else {
                result.filtered_file_count.saturating_sub(1)
            };
            self.match_offset = result.matches[..end]
                .iter()
                .rev()
                .take_while(|m| m.file_index == last_file)
                .count();
            self.match_file = file_id(result, last_file, picker);
        } else {
            self.file_offset = result.next_file_offset;
            self.match_offset = 0;
        }
        result.matches.truncate(end);
        result.matches.drain(..start);
        self.found_matches |= !result.matches.is_empty();
    }

    pub fn has_more(self) -> bool {
        self.file_offset > 0 || self.match_offset > 0
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) enum GrepStage {
    #[default]
    Normal,
    Broadened,
    Definitions,
    References,
    FuzzyDefinitions,
    FuzzyMatches,
}

impl GrepStage {
    pub fn next(self) -> Option<Self> {
        match self {
            Self::Definitions => Some(Self::References),
            Self::References => Some(Self::FuzzyDefinitions),
            Self::FuzzyDefinitions => Some(Self::FuzzyMatches),
            _ => None,
        }
    }

    pub fn is_fuzzy(self) -> bool {
        matches!(self, Self::FuzzyDefinitions | Self::FuzzyMatches)
    }

    pub fn definition_mode(self) -> fff::DefinitionMode {
        match self {
            Self::Definitions | Self::FuzzyDefinitions => fff::DefinitionMode::DefinitionOnly,
            // A file-path retry can widen these stages, so keep definitions found elsewhere.
            Self::References | Self::FuzzyMatches | Self::Normal | Self::Broadened => {
                fff::DefinitionMode::Classify
            }
        }
    }

    pub fn notice(self) -> Option<&'static str> {
        match self {
            Self::References => Some("No matching definitions found. Showing other matches:"),
            Self::FuzzyDefinitions => {
                Some("No exact matches found. Maybe you meant these definitions:")
            }
            Self::FuzzyMatches => Some(
                "No exact matches or approximate definitions found. Maybe you meant these other matches:",
            ),
            _ => None,
        }
    }
}

// Opaque page cursors. IDs are salted hashes, not offsets.
pub(crate) struct CursorStore {
    counter: u64,
    salt: u64,
    entries: HashMap<String, Entry>,
    order: VecDeque<String>,
}

enum Entry {
    Offset(usize),
    // A grep page remembers the Root it came from, so a reused id cannot page
    // another Root's file list.
    Grep { root: String, cursor: GrepCursor },
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
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn store(&mut self, offset: usize) -> String {
        self.insert(Entry::Offset(offset))
    }

    pub fn get(&self, id: &str) -> Option<usize> {
        match self.entries.get(id) {
            Some(Entry::Offset(offset)) => Some(*offset),
            _ => None,
        }
    }

    pub(crate) fn store_grep(&mut self, root: &str, cursor: GrepCursor) -> String {
        self.insert(Entry::Grep {
            root: root.to_string(),
            cursor,
        })
    }

    pub(crate) fn get_grep(&self, root: &str, id: &str) -> Option<GrepCursor> {
        match self.entries.get(id) {
            Some(Entry::Grep {
                root: stored,
                cursor,
            }) if stored == root => Some(*cursor),
            _ => None,
        }
    }

    fn insert(&mut self, entry: Entry) -> String {
        self.counter = self.counter.wrapping_add(1);
        let id = format!("{:016x}", mix(self.counter ^ self.salt));
        self.entries.insert(id.clone(), entry);
        self.order.push_back(id.clone());
        while self.entries.len() > MAX_CURSORS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
        id
    }
}

// splitmix64: cheap avalanche so consecutive counters do not look like offsets.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn file_id(result: &fff::GrepResult<'_>, index: usize, picker: &fff::FilePicker) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    result.files[index].relative_path(picker).hash(&mut hasher);
    hasher.finish()
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
        assert_eq!(store.entries.len(), MAX_CURSORS);
    }

    #[test]
    fn grep_cursors_are_root_scoped() {
        let mut store = CursorStore::new();
        let cursor = GrepCursor {
            file_offset: 7,
            ..Default::default()
        };
        let id = store.store_grep("/a", cursor);
        assert_eq!(store.get_grep("/a", &id).map(|c| c.file_offset), Some(7));
        assert!(store.get_grep("/b", &id).is_none());
        assert!(store.get(&id).is_none(), "a grep id is not an offset");
    }
}
