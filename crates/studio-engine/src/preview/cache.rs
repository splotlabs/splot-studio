// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Recently sent presentation pictures, bounded in bytes.

use std::collections::VecDeque;

use studio_domain::preview::{ColorBasis, PictureBox, PictureView};

use crate::picture::Rendered;

/// The most pixel bytes kept: about 24 pictures of the largest size.
pub const CACHE_BYTES: usize = 64 << 20;

struct Entry<K> {
    key: K,
    index: u64,
    view: PictureView,
    size: PictureBox,
    rendered: Rendered,
    frame_size: (u32, u32),
    basis: ColorBasis,
}

/// Pictures by file, frame, requested view and box; least recently used
/// first out.
pub struct Cache<K> {
    entries: VecDeque<Entry<K>>,
    bytes: usize,
}

impl<K> Default for Cache<K> {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
        }
    }
}

impl<K: PartialEq> Cache<K> {
    /// A copy of the picture, if kept.
    pub fn get(
        &mut self,
        key: &K,
        index: u64,
        view: PictureView,
        size: PictureBox,
    ) -> Option<(Rendered, (u32, u32), ColorBasis)> {
        let at = self
            .entries
            .iter()
            .position(|e| e.key == *key && e.index == index && e.view == view && e.size == size)?;
        let entry = self.entries.remove(at)?;
        let found = (entry.rendered.clone(), entry.frame_size, entry.basis);
        self.entries.push_back(entry);
        Some(found)
    }

    /// Keeps a copy of `rendered`, dropping the oldest pictures over
    /// [`CACHE_BYTES`].
    #[allow(clippy::too_many_arguments)]
    pub fn put(
        &mut self,
        key: K,
        index: u64,
        view: PictureView,
        size: PictureBox,
        rendered: &Rendered,
        frame_size: (u32, u32),
        basis: ColorBasis,
    ) {
        let bytes = rendered.pixels.len();
        if bytes > CACHE_BYTES {
            return;
        }
        while self.bytes + bytes > CACHE_BYTES {
            let Some(old) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= old.rendered.pixels.len();
        }
        self.bytes += bytes;
        self.entries.push_back(Entry {
            key,
            index,
            view,
            size,
            rendered: rendered.clone(),
            frame_size,
            basis,
        });
    }

    /// Drops every picture of `key`'s file.
    pub fn forget(&mut self, key: &K) {
        self.entries.retain(|e| e.key != *key);
        self.bytes = self.entries.iter().map(|e| e.rendered.pixels.len()).sum();
    }

    /// Pixel bytes kept.
    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(bytes: usize) -> Rendered {
        Rendered {
            pixels: vec![0; bytes],
            width: 1,
            height: 1,
            view: PictureView::Fit,
        }
    }

    #[test]
    fn the_byte_budget_holds() {
        let mut cache = Cache::default();
        let size = PictureBox {
            width: 1280,
            height: 720,
        };
        let basis = ColorBasis {
            matrix: studio_domain::preview::ColorMatrix::Bt709,
            assumed: true,
        };
        let big = CACHE_BYTES / 3;
        for index in 0..10 {
            cache.put(
                1,
                index,
                PictureView::Fit,
                size,
                &picture(big),
                (1, 1),
                basis,
            );
            assert!(cache.bytes() <= CACHE_BYTES);
        }
        assert!(cache.get(&1, 9, PictureView::Fit, size).is_some());
        assert!(cache.get(&1, 0, PictureView::Fit, size).is_none());
        // A use keeps a picture: 7 was used, so 8 goes first.
        assert!(cache.get(&1, 7, PictureView::Fit, size).is_some());
        cache.put(1, 10, PictureView::Fit, size, &picture(big), (1, 1), basis);
        assert!(cache.get(&1, 7, PictureView::Fit, size).is_some());
        assert!(cache.get(&1, 8, PictureView::Fit, size).is_none());
        cache.put(
            2,
            0,
            PictureView::Fit,
            size,
            &picture(CACHE_BYTES + 1),
            (1, 1),
            basis,
        );
        assert!(cache.get(&2, 0, PictureView::Fit, size).is_none());
        cache.forget(&1);
        assert_eq!(cache.bytes(), 0);
    }
}
