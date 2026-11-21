// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Answered pictures, bounded in bytes.

use super::{CACHE_BYTES, Key};
use crate::engine::Picture;

/// Answered pictures, bounded by [`CACHE_BYTES`], oldest use first out.
#[derive(Debug, Default)]
pub(super) struct Cache {
    pub(super) entries: Vec<(Key, Picture)>,
    pub(super) bytes: usize,
}

impl Cache {
    pub(super) fn get(&mut self, key: &Key) -> Option<Picture> {
        let index = self.entries.iter().position(|(k, _)| k == key)?;
        let entry = self.entries.remove(index);
        let picture = entry.1.clone();
        self.entries.push(entry);
        Some(picture)
    }

    pub(super) fn peek(&self, key: &Key) -> Option<&Picture> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, p)| p)
    }

    pub(super) fn contains(&self, key: &Key) -> bool {
        self.peek(key).is_some()
    }

    pub(super) fn put(&mut self, key: Key, picture: Picture) {
        self.retain(|k| *k != key);
        let size = picture.bytes();
        if size > CACHE_BYTES {
            return;
        }
        self.entries.push((key, picture));
        self.bytes += size;
        while self.bytes > CACHE_BYTES && !self.entries.is_empty() {
            let (_, old) = self.entries.remove(0);
            self.bytes -= old.bytes();
        }
    }

    pub(super) fn retain(&mut self, keep: impl Fn(&Key) -> bool) {
        let mut bytes = 0;
        self.entries.retain(|(k, p)| {
            let kept = keep(k);
            if kept {
                bytes += p.bytes();
            }
            kept
        });
        self.bytes = bytes;
    }
}
