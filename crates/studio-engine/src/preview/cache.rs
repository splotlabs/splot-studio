// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Recently sent presentation pictures, bounded in bytes.

use std::collections::VecDeque;

use studio_domain::preview::PictureHeader;

use super::Display;

/// The most pixel bytes kept: about 24 pictures of the largest size.
pub const CACHE_BYTES: usize = 64 << 20;

/// Bounds retained headers and file paths even for one-pixel views.
const MAX_ENTRIES: usize = 256;

struct Entry<K> {
    key: K,
    display: Display,
    header: PictureHeader,
    pixels: Vec<u8>,
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
        display: Display,
    ) -> Option<(PictureHeader, Vec<u8>)> {
        let at = self
            .entries
            .iter()
            .position(|e| e.key == *key && e.header.frame == index && e.display == display)?;
        let entry = self.entries.remove(at)?;
        let found = (entry.header.clone(), entry.pixels.clone());
        self.entries.push_back(entry);
        Some(found)
    }

    /// Keeps a copy of the picture, dropping the oldest pictures over
    /// [`CACHE_BYTES`].
    pub fn put(&mut self, key: K, display: Display, header: &PictureHeader, pixels: &[u8]) {
        let bytes = pixels.len();
        if bytes > CACHE_BYTES {
            return;
        }
        while self.entries.len() >= MAX_ENTRIES || self.bytes + bytes > CACHE_BYTES {
            let Some(old) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= old.pixels.len();
        }
        self.bytes += bytes;
        self.entries.push_back(Entry {
            key,
            display,
            header: header.clone(),
            pixels: pixels.to_vec(),
        });
    }

    /// Drops every picture of `key`'s file.
    pub fn forget(&mut self, key: &K) {
        self.forget_matching(|candidate| candidate == key);
    }

    /// Drops the pictures of every matching file.
    pub fn forget_matching(&mut self, matches: impl Fn(&K) -> bool) {
        self.entries.retain(|entry| !matches(&entry.key));
        self.bytes = self.entries.iter().map(|e| e.pixels.len()).sum();
    }

    /// Pixel bytes kept.
    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use studio_domain::preview::{PictureBox, PictureOptions, PictureView};

    use super::*;

    fn display(size: PictureBox) -> Display {
        Display {
            view: PictureView::Fit,
            size,
            options: PictureOptions::default(),
        }
    }

    fn header(frame: u64) -> PictureHeader {
        use studio_domain::preview::{ColorBasis, ColorMatrix, PictureOrigin};
        PictureHeader {
            generation: 0,
            origin: PictureOrigin::Result,
            output: None,
            options: PictureOptions::default(),
            document_info: None,
            frame,
            time: None,
            frame_count: None,
            width: 1,
            height: 1,
            frame_width: 1,
            frame_height: 1,
            view: PictureView::Fit,
            basis: ColorBasis {
                matrix: ColorMatrix::Bt709,
                assumed: true,
            },
        }
    }

    #[test]
    fn the_cache_checks_surface_and_grain() {
        use studio_domain::preview::{GrainPolicy, PictureSurface};

        let mut cache = Cache::default();
        let original = display(PictureBox::clamped(1, 1));
        let mut alternate = original;
        cache.put(1, original, &header(0), &[0; 3]);

        alternate.options.surface = PictureSurface::Y;
        assert!(cache.get(&1, 0, alternate).is_none());
        alternate.options.surface = PictureSurface::Rgb;
        alternate.options.grain = GrainPolicy::Skip;
        assert!(cache.get(&1, 0, alternate).is_none());
        assert!(cache.get(&1, 0, original).is_some());
    }

    #[test]
    fn tiny_pictures_do_not_fill_the_cache_with_headers() {
        let mut cache = Cache::default();
        let display = display(PictureBox::clamped(1, 1));
        for index in 0..=MAX_ENTRIES as u64 {
            cache.put(1, display, &header(index), &[0; 3]);
        }
        assert!(cache.get(&1, 0, display).is_none());
        assert!(cache.get(&1, 1, display).is_some());
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn the_byte_budget_holds() {
        let mut cache = Cache::default();
        let display = display(PictureBox {
            width: 1280,
            height: 720,
        });
        let big = vec![0; CACHE_BYTES / 3];
        for index in 0..10 {
            cache.put(1, display, &header(index), &big);
            assert!(cache.bytes() <= CACHE_BYTES);
        }
        assert!(cache.get(&1, 9, display).is_some());
        assert!(cache.get(&1, 0, display).is_none());
        // A use keeps a picture: 7 was used, so 8 goes first.
        assert!(cache.get(&1, 7, display).is_some());
        cache.put(1, display, &header(10), &big);
        assert!(cache.get(&1, 8, display).is_none());
        cache.put(2, display, &header(0), &vec![0; CACHE_BYTES + 1]);
        assert!(cache.get(&2, 0, display).is_none());
        cache.forget(&1);
        assert_eq!(cache.bytes(), 0);
    }
}
