use crate::index::bigram_filter::MAX_BIGRAM_COLUMNS;

// mtime_ns u64 | size u64 | flags u8. Then each key list as a u16 count
// followed by delta-varint keys.
const HEADER_LEN: usize = 17;
const FLAG_BINARY: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) mtime_ns: u64,
    pub(crate) size: u64,
    pub(crate) binary: bool,
    pub(crate) consecutive: Vec<u16>,
    pub(crate) skip1: Vec<u16>,
}

impl Row {
    pub(crate) fn encode(&self) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(HEADER_LEN + 4 + 3 * self.key_count());
        out.extend_from_slice(&self.mtime_ns.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
        out.push(if self.binary { FLAG_BINARY } else { 0 });
        put_keys(&self.consecutive, &mut out)?;
        put_keys(&self.skip1, &mut out)?;
        Some(out)
    }

    #[allow(dead_code)] // a hit reads rows back in ticket 05
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let mut pos = 0;
        let mtime_ns = read_u64(bytes, &mut pos)?;
        let size = read_u64(bytes, &mut pos)?;
        let flags = *bytes.get(pos)?;
        pos += 1;
        if flags & !FLAG_BINARY != 0 {
            return None;
        }

        let consecutive = read_keys(bytes, &mut pos)?;
        let skip1 = read_keys(bytes, &mut pos)?;
        // Anything left means the row is longer than its own counts say.
        if pos != bytes.len() {
            return None;
        }

        Some(Self {
            mtime_ns,
            size,
            binary: flags & FLAG_BINARY != 0,
            consecutive,
            skip1,
        })
    }

    fn key_count(&self) -> usize {
        self.consecutive.len() + self.skip1.len()
    }
}

// Writes a u16 count then the keys as canonical LEB128 deltas; the first key
// is a delta from 0. Refuses counts over the cap and non-ascending lists.
fn put_keys(keys: &[u16], out: &mut Vec<u8>) -> Option<()> {
    if keys.len() > MAX_BIGRAM_COLUMNS || keys.windows(2).any(|w| w[0] >= w[1]) {
        return None;
    }
    out.extend_from_slice(&(keys.len() as u16).to_le_bytes());
    let mut prev = 0u32;
    for &key in keys {
        put_varint(key as u32 - prev, out);
        prev = key as u32;
    }
    Some(())
}

fn read_keys(bytes: &[u8], pos: &mut usize) -> Option<Vec<u16>> {
    let count = read_u16(bytes, pos)? as usize;
    if count > MAX_BIGRAM_COLUMNS {
        return None;
    }

    let mut keys = Vec::with_capacity(count);
    let mut prev = 0u32;
    for i in 0..count {
        let delta = read_varint(bytes, pos)?;
        // Delta 0 after the first key repeats it; keys are strictly ascending.
        if i > 0 && delta == 0 {
            return None;
        }
        let key = prev + delta;
        if key > u16::MAX as u32 {
            return None;
        }
        keys.push(key as u16);
        prev = key;
    }
    Some(keys)
}

fn put_varint(mut value: u32, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

// Rejects non-minimal encodings so that decode/encode is byte-identical.
fn read_varint(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let mut value = 0u32;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*pos)?;
        *pos += 1;
        if shift == 28 && byte > 0x0F {
            return None;
        }
        value |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 {
            return if shift > 0 && byte == 0 {
                None
            } else {
                Some(value)
            };
        }
        shift += 7;
        if shift >= 32 {
            return None;
        }
    }
}

fn read_u16(bytes: &[u8], pos: &mut usize) -> Option<u16> {
    let end = *pos + 2;
    let value = u16::from_le_bytes(bytes.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(value)
}

fn read_u64(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let end = *pos + 8;
    let value = u64::from_le_bytes(bytes.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn sample_keys(count: usize, salt: u64) -> Vec<u16> {
        let mut set = BTreeSet::new();
        let mut x = 0x2545_F491_4F6C_DD1D ^ salt;
        while set.len() < count {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let hi = 32 + (x % 95) as u8;
            let lo = 32 + ((x >> 8) % 95) as u8;
            set.insert(u16::from_be_bytes([hi, lo]));
        }
        set.into_iter().collect()
    }

    fn indexable_row() -> Row {
        Row {
            mtime_ns: 1_700_000_000_123_456_789,
            size: 4096,
            binary: false,
            consecutive: sample_keys(464, 1),
            skip1: sample_keys(603, 2),
        }
    }

    // A declared count whose key bytes are missing must fail rather than
    // return a row holding fewer keys than its own header promised.
    fn raw(flags: u8, n_consec: u16, consec: &[u8], n_skip: u16, skip: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&7u64.to_le_bytes());
        out.extend_from_slice(&11u64.to_le_bytes());
        out.push(flags);
        out.extend_from_slice(&n_consec.to_le_bytes());
        out.extend_from_slice(consec);
        out.extend_from_slice(&n_skip.to_le_bytes());
        out.extend_from_slice(skip);
        out
    }

    #[test]
    fn indexable_row_round_trips_byte_for_byte() {
        let row = indexable_row();
        let bytes = row.encode().expect("encode");
        let back = Row::decode(&bytes).expect("decode");

        assert_eq!(back.mtime_ns, row.mtime_ns);
        assert_eq!(back.size, row.size);
        assert!(!back.binary);
        assert_eq!(back.consecutive, row.consecutive);
        assert_eq!(back.skip1, row.skip1);
        assert_eq!(back.encode().as_deref(), Some(bytes.as_slice()));
    }

    #[test]
    fn empty_key_row_keeps_its_binary_verdict() {
        for binary in [false, true] {
            let row = Row {
                mtime_ns: 1,
                size: 0,
                binary,
                consecutive: Vec::new(),
                skip1: Vec::new(),
            };
            let bytes = row.encode().expect("encode");
            assert_eq!(bytes.len(), HEADER_LEN + 4);
            let back = Row::decode(&bytes).expect("decode");
            assert_eq!(back.binary, binary);
            assert!(back.consecutive.is_empty());
            assert!(back.skip1.is_empty());
            assert_eq!(back.encode().as_deref(), Some(bytes.as_slice()));
        }
    }

    #[test]
    fn the_skip_list_survives_independently_of_the_consecutive_list() {
        let row = Row {
            mtime_ns: 42,
            size: 3,
            binary: false,
            consecutive: vec![0x6162, 0x6263],
            skip1: vec![0x6163, 0x7A7A],
        };
        assert!(!row.skip1.iter().all(|k| row.consecutive.contains(k)));

        let bytes = row.encode().expect("encode");
        let back = Row::decode(&bytes).expect("decode");
        assert_eq!(back.consecutive, row.consecutive);
        assert_eq!(back.skip1, row.skip1);
        assert_eq!(back.encode().as_deref(), Some(bytes.as_slice()));
    }

    #[test]
    fn the_row_records_the_identity_its_verdict_is_valid_for() {
        let row = Row {
            mtime_ns: u64::MAX,
            size: u64::MAX - 1,
            binary: true,
            consecutive: Vec::new(),
            skip1: Vec::new(),
        };
        let bytes = row.encode().expect("encode");
        assert_eq!(
            u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            u64::MAX
        );
        assert_eq!(
            u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            u64::MAX - 1
        );

        let back = Row::decode(&bytes).expect("decode");
        assert_eq!((back.mtime_ns, back.size), (u64::MAX, u64::MAX - 1));
    }

    #[test]
    fn every_truncation_decodes_to_none() {
        let bytes = indexable_row().encode().expect("encode");
        for cut in 0..bytes.len() {
            assert!(
                Row::decode(&bytes[..cut]).is_none(),
                "truncation at {cut} of {} bytes decoded",
                bytes.len()
            );
        }
    }

    #[test]
    fn trailing_bytes_decode_to_none() {
        let mut bytes = indexable_row().encode().expect("encode");
        bytes.push(0);
        assert!(Row::decode(&bytes).is_none());
    }

    #[test]
    fn a_declared_count_that_overruns_the_buffer_decodes_to_none() {
        assert!(Row::decode(&raw(0, 3, &[], 0, &[])).is_none());
        assert!(Row::decode(&raw(0, 0, &[], 2, &[0x05])).is_none());
        // Count agrees with the buffer but the second key equals the first.
        assert!(Row::decode(&raw(0, 2, &[0x05, 0x00], 0, &[])).is_none());
        // A varint cut in half.
        assert!(Row::decode(&raw(0, 1, &[0x80], 0, &[])).is_none());
    }

    #[test]
    fn out_of_order_and_duplicate_keys_are_rejected() {
        // Strictly ascending: delta 0 after the first key is a duplicate.
        assert!(Row::decode(&raw(0, 2, &[0x05, 0x00], 0, &[])).is_none());
        // 0x6162 + 0xFFFF overflows u16, i.e. the second key would descend.
        assert!(Row::decode(&raw(0, 2, &[0xF6, 0xC2, 0x03, 0xFF, 0xFF, 0x03], 0, &[])).is_none());
        // Non-minimal varint: 0x80 0x00 is a zero delta written long.
        assert!(Row::decode(&raw(0, 1, &[0x80, 0x00], 0, &[])).is_none());
    }

    #[test]
    fn a_count_above_the_column_cap_is_rejected() {
        let over = MAX_BIGRAM_COLUMNS as u16 + 1;
        assert!(Row::decode(&raw(0, over, &[], 0, &[])).is_none());
        assert!(Row::decode(&raw(0, 0, &[], over, &[])).is_none());

        let base = Row {
            mtime_ns: 1,
            size: 1,
            binary: false,
            consecutive: Vec::new(),
            skip1: Vec::new(),
        };
        let keys: Vec<u16> = (0..=MAX_BIGRAM_COLUMNS as u16).collect();
        let consec = Row {
            consecutive: keys.clone(),
            ..base.clone()
        };
        assert!(consec.encode().is_none());
        assert!(
            Row {
                skip1: keys,
                ..base
            }
            .encode()
            .is_none()
        );
    }

    #[test]
    fn encoding_refuses_unsorted_or_duplicated_keys() {
        let base = Row {
            mtime_ns: 1,
            size: 1,
            binary: false,
            consecutive: vec![10, 20],
            skip1: Vec::new(),
        };
        assert!(base.encode().is_some());
        for bad in [vec![20, 10], vec![10, 10], vec![10, 20, 15]] {
            let row = Row {
                consecutive: bad.clone(),
                skip1: bad,
                ..base.clone()
            };
            assert!(row.encode().is_none(), "{row:?}");
        }
    }

    #[test]
    fn unknown_flag_bits_and_short_headers_are_rejected() {
        assert!(Row::decode(&raw(0x02, 0, &[], 0, &[])).is_none());
        assert!(Row::decode(&raw(0xFE, 0, &[], 0, &[])).is_none());
        for len in 0..HEADER_LEN + 4 {
            let bytes = raw(0, 0, &[], 0, &[]);
            assert!(Row::decode(&bytes[..len]).is_none());
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic_and_any_decode_is_canonical() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut buf = Vec::with_capacity(64);
        for _ in 0..4000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 49) as usize;
            buf.clear();
            let mut y = x;
            for _ in 0..len {
                y ^= y << 13;
                y ^= y >> 7;
                y ^= y << 17;
                buf.push(y as u8);
            }
            if buf.len() < HEADER_LEN + 4 {
                assert!(Row::decode(&buf).is_none());
                continue;
            }
            if let Some(row) = Row::decode(&buf) {
                assert_eq!(row.encode().as_deref(), Some(buf.as_slice()));
            }
        }
    }
}
