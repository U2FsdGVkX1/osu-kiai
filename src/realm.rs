//! A minimal read-only reader for Realm files (osu!lazer's client.realm),
//! covering only what's needed to read the beatmap library.
//!
//! A Realm file is made of nodes, each an 8-byte header followed by data:
//! - header[4] holds flags: 0x80 = B+ tree inner node, 0x40 = elements are refs,
//!   0x20 = context flag; the low 3 bits encode the element width ((1 << n) >> 1 bits)
//! - header[5..8] is the element count (24-bit big endian)
//!
//! An even element is a ref (file offset of another node); an odd one is a tagged integer (value << 1 | 1).

use std::{collections::HashMap, path::Path};

pub struct Realm(Vec<u8>);

impl Realm {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        std::fs::read(path).map(Self)
    }

    /// Element `i` of node `r` as an unsigned integer. Refs, keys and the values we read are all non-negative.
    pub fn get(&self, r: usize, i: usize) -> usize {
        let w = (1 << (self.0[r + 4] & 7)) >> 1;
        let d = &self.0[r + 8..];
        if w < 8 {
            (d[i * w / 8] as usize >> (i * w % 8)) & ((1 << w) - 1)
        } else {
            d[i * w / 8..][..w / 8].iter().rev().fold(0, |a, &b| a << 8 | b as usize)
        }
    }

    pub fn len(&self, r: usize) -> usize {
        u32::from_be_bytes([0, self.0[r + 5], self.0[r + 6], self.0[r + 7]]) as usize
    }

    fn blob(&self, r: usize) -> &[u8] {
        &self.0[r + 8..][..self.len(r)]
    }

    /// String arrays come in three layouts:
    /// - short: fixed-width elements, the last byte of each holds the padding length
    /// - long (refs + context flag): each element refs its own NUL-terminated blob
    /// - medium (refs): [end offsets, one blob of concatenated NUL-terminated strings]
    pub fn string(&self, r: usize, i: usize) -> Option<String> {
        let flags = self.0[r + 4];
        let bytes = if flags & 0x40 == 0 {
            let w = (1 << (flags & 7)) >> 1;
            if w == 0 {
                return Some(String::new());
            }
            let e = &self.0[r + 8 + i * w..][..w];
            &e[..(w - 1).checked_sub(e[w - 1] as usize)?]
        } else if flags & 0x20 != 0 {
            let b = self.get(r, i);
            self.blob(b).split_last()?.1
        } else {
            let (offsets, blob) = (self.get(r, 0), self.get(r, 1));
            let start = if i == 0 { 0 } else { self.get(offsets, i - 1) };
            self.blob(blob).get(start..self.get(offsets, i).checked_sub(1)?)?
        };
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    /// Finds a table by name and returns (root of its cluster tree, column name → slot in a cluster leaf).
    pub fn table(&self, name: &str) -> Option<(usize, HashMap<String, usize>)> {
        // The file starts with two top refs; the low bit of byte 23 selects the current one
        let at = 8 * (self.0[23] & 1) as usize;
        let top = u64::from_le_bytes(self.0[at..at + 8].try_into().unwrap()) as usize;
        let (names, tables) = (self.get(top, 0), self.get(top, 1));
        let t = self.get(tables, (0..self.len(names)).find(|&i| self.string(names, i).as_deref() == Some(name))?);

        // spec[1] holds column names, spec[5] column keys whose low 16 bits are the column index;
        // slot 0 of every cluster holds the row keys, so columns start at slot 1
        let spec = self.get(t, 0);
        let (cols, keys) = (self.get(spec, 1), self.get(spec, 5));
        let cols = (0..self.len(cols)).filter_map(|i| Some((self.string(cols, i)?, (self.get(keys, i) & 0xffff) + 1)));
        Some((self.get(t, 2), cols.collect()))
    }

    /// Visits every row of a cluster tree as (row key, leaf node, index within the leaf).
    pub fn rows(&self, r: usize, base: usize, f: &mut impl FnMut(usize, usize, usize)) {
        let keys = self.get(r, 0);
        if self.0[r + 4] & 0x80 != 0 {
            // Inner node: [child key offsets, depth, row count, children...];
            // without an offsets array each child covers 256^depth keys
            let shift = 8 * (self.get(r, 1) >> 1);
            for c in 3..self.len(r) {
                let offset = if keys == 0 { (c - 3) << shift } else { self.get(keys, c - 3) };
                self.rows(self.get(r, c), base + offset, f);
            }
        } else if keys & 1 == 1 {
            // Compact leaf: keys are 0..n and only n is stored
            (0..keys >> 1).for_each(|i| f(base + i, r, i));
        } else {
            (0..self.len(keys)).for_each(|i| f(base + self.get(keys, i), r, i));
        }
    }

    /// Reads a link list; past 1000 items it becomes a B+ tree: [offsets, children..., total].
    pub fn list(&self, r: usize, out: &mut Vec<usize>) {
        if self.0[r + 4] & 0x80 != 0 {
            (1..self.len(r) - 1).for_each(|c| self.list(self.get(r, c), out));
        } else {
            out.extend((0..self.len(r)).map(|i| self.get(r, i)));
        }
    }
}
