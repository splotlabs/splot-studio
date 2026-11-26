// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Disk-backed framing facts with one write page and one read page in memory.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use studio_domain::analysis::{AnalysisFailure, LocateTarget, Row, RowFilter};

use super::{
    DocumentSummary, Framing, Item, IvfHeader, IvfHeaderFact, LayerCount, ObuType, ObuTypeCount,
    Refusal, ScanOutcome, SourceDigest, SourceFingerprint,
};

#[derive(Debug, Clone, Default)]
pub(super) struct Facts {
    packets: u64,
    unparsed: u64,
    types: [u64; 32],
    layers: [[(u64, u64); 8]; 4],
    elements: u64,
}

impl Facts {
    pub(super) fn add(&mut self, item: &Item) {
        self.change(item, |value, amount| *value += amount);
    }

    fn remove(&mut self, item: &Item) {
        self.change(item, |value, amount| *value -= amount);
    }

    fn change(&mut self, item: &Item, change: impl Fn(&mut u64, u64)) {
        change(&mut self.elements, 1);
        match item {
            Item::Packet { .. } => change(&mut self.packets, 1),
            Item::Unparsed { .. } => change(&mut self.unparsed, 1),
            Item::Obu { header, .. } => {
                change(&mut self.types[usize::from(header.obu_type.raw())], 1);
                let (count, inferred) = &mut self.layers
                    [usize::from(header.temporal_layer_id.get())]
                    [usize::from(header.embedded_layer_id.get())];
                change(count, 1);
                change(inferred, u64::from(!header.has_header_extension));
            }
            Item::IvfHeader { .. } => {}
        }
    }

    pub(super) fn summary(
        &self,
        fingerprint: SourceFingerprint,
        framing: Framing,
        header: Option<IvfHeader>,
        outcome: ScanOutcome,
        digest: Option<SourceDigest>,
    ) -> DocumentSummary {
        let obu_types = (0_u8..)
            .zip(self.types)
            .filter(|(_, count)| *count > 0)
            .map(|(obu_type, count)| ObuTypeCount {
                obu_type,
                count,
                type_name: ObuType::from_raw(obu_type).spec_name().to_owned(),
            })
            .collect();
        let mut layers = Vec::new();
        for (temporal, row) in (0_u8..).zip(self.layers) {
            for (embedded, (count, inferred)) in (0_u8..).zip(row) {
                if count > 0 {
                    layers.push(LayerCount {
                        temporal,
                        embedded,
                        count,
                        inferred,
                    });
                }
            }
        }
        DocumentSummary {
            fingerprint,
            digest: digest.map(Box::new),
            framing,
            ivf_header: header.map(|h| IvfHeaderFact {
                version: h.version,
                header_len: h.header_len,
                fourcc: h.fourcc,
                width: h.width,
                height: h.height,
                timebase_numerator: h.timebase_numerator,
                timebase_denominator: h.timebase_denominator,
                frame_count: h.frame_count,
            }),
            elements: self.elements,
            packets: self.packets,
            obus: self.types.iter().sum(),
            unparsed: self.unparsed,
            obu_types,
            layers,
            outcome,
        }
    }
}

const PAGE_ROWS: usize = 256;
pub(super) const MAX_INDEX_BYTES: u64 = 512 << 20;
// Directory memory is bounded independently of page compression or record length.
const MAX_INDEX_PAGES: usize = 8192;
const MAX_PAGE_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    item: Item,
    parent: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Page {
    offset: u64,
    len: usize,
    start: u64,
    digest: [u8; 32],
    kinds: Vec<(u16, u16)>,
}

impl Page {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "validated filter keys fit their bit fields"
    )]
    fn count(&self, filter: RowFilter) -> usize {
        if filter.is_all() {
            return PAGE_ROWS;
        }
        self.kinds
            .iter()
            .filter(|(key, _)| match *key {
                1024 => filter.admits_unparsed(),
                1025 => false,
                key => filter.admits_obu((key / 32) as u8, ((key / 8) % 4) as u8, (key % 8) as u8),
            })
            .map(|(_, count)| usize::from(*count))
            .sum()
    }
}

#[derive(Debug)]
pub(super) struct Index {
    file: Arc<Mutex<File>>,
    base: u64,
    pages: Vec<Page>,
    tail: Vec<Entry>,
    loaded: RefCell<Option<(usize, Vec<Entry>)>>,
    bytes: u64,
    budget: u64,
    end: u64,
    packet: Option<(usize, u64)>,
    facts: Facts,
}

impl Index {
    pub(super) fn new() -> Result<Self, Refusal> {
        Ok(Self {
            file: Arc::new(Mutex::new(tempfile::tempfile().map_err(io_error)?)),
            base: 0,
            pages: Vec::new(),
            tail: Vec::with_capacity(PAGE_ROWS),
            loaded: RefCell::new(None),
            bytes: 0,
            budget: MAX_INDEX_BYTES,
            end: 0,
            packet: None,
            facts: Facts::default(),
        })
    }

    pub(super) fn export(&self) -> io::Result<IndexExport> {
        let directory = serde_json::to_vec(&(&self.pages, &self.tail)).map_err(io::Error::other)?;
        if directory.len() > MAX_DIRECTORY_BYTES {
            return Err(invalid_cache());
        }
        Ok(IndexExport {
            file: Arc::clone(&self.file),
            base: self.base,
            bytes: self.bytes,
            directory,
        })
    }

    pub(super) fn open_cache(
        mut file: File,
        summary: &DocumentSummary,
        payload_end: u64,
    ) -> io::Result<Self> {
        let base = file.stream_position()?;
        let footer = payload_end
            .checked_sub(8)
            .filter(|&at| at >= base)
            .ok_or_else(invalid_cache)?;
        file.seek(SeekFrom::Start(footer))?;
        let mut size = [0; 8];
        file.read_exact(&mut size)?;
        let size = usize::try_from(u64::from_le_bytes(size))
            .ok()
            .filter(|&size| size <= MAX_DIRECTORY_BYTES)
            .ok_or_else(invalid_cache)?;
        let directory_offset = footer
            .checked_sub(size as u64)
            .filter(|&at| at >= base)
            .ok_or_else(invalid_cache)?;
        let bytes = directory_offset - base;
        if bytes > MAX_INDEX_BYTES {
            return Err(invalid_cache());
        }
        file.seek(SeekFrom::Start(directory_offset))?;
        let mut directory = vec![0; size];
        file.read_exact(&mut directory)?;
        let (pages, tail): (Vec<Page>, Vec<Entry>) =
            serde_json::from_slice(&directory).map_err(io::Error::other)?;
        if pages.len() > MAX_INDEX_PAGES
            || tail.len() >= PAGE_ROWS
            || (pages.len() * PAGE_ROWS + tail.len()) as u64 != summary.elements
        {
            return Err(invalid_cache());
        }
        let mut offset = 0_u64;
        let mut start = 0_u64;
        for page in &pages {
            if page.offset != offset
                || page.start < start
                || page.start >= summary.fingerprint.len
                || page.len == 0
                || page.len > MAX_PAGE_BYTES
                || page.kinds.len() > PAGE_ROWS
                || page
                    .kinds
                    .iter()
                    .any(|&(key, count)| key > 1025 || count == 0)
                || page
                    .kinds
                    .iter()
                    .map(|&(_, count)| usize::from(count))
                    .sum::<usize>()
                    != PAGE_ROWS
            {
                return Err(invalid_cache());
            }
            offset += page.len as u64;
            start = page.start;
        }
        if offset != bytes {
            return Err(invalid_cache());
        }
        let index = Self {
            file: Arc::new(Mutex::new(file)),
            base,
            pages,
            tail,
            loaded: RefCell::new(None),
            bytes,
            budget: MAX_INDEX_BYTES,
            end: summary.fingerprint.len,
            packet: None,
            facts: Facts::default(),
        };
        Ok(index)
    }

    #[cfg(test)]
    pub(super) fn set_disk_budget(&mut self, budget: u64) {
        self.budget = budget;
    }

    pub(super) fn len(&self) -> usize {
        self.pages.len() * PAGE_ROWS + self.tail.len()
    }

    pub(super) fn end(&self) -> u64 {
        self.end
    }

    pub(super) fn push(&mut self, item: Item) -> Result<(), Refusal> {
        let end = self.end;
        let packet = self.packet;
        self.push_entry(item);
        let result = if self.tail.len() == PAGE_ROWS {
            self.flush()
        } else {
            Ok(())
        };
        if result.is_err() {
            if let Some(entry) = self.tail.pop() {
                self.facts.remove(&entry.item);
            }
            self.end = end;
            self.packet = packet;
        }
        result
    }

    pub(super) fn facts(&self) -> &Facts {
        &self.facts
    }

    pub(super) fn push_tail(&mut self, item: Item) {
        self.push_entry(item);
    }

    fn push_entry(&mut self, item: Item) {
        let index = self.len();
        let parent = self
            .packet
            .filter(|(_, end)| item.start() < *end && item.end() <= *end)
            .filter(|_| {
                matches!(
                    item,
                    Item::Obu {
                        packet: Some(_),
                        ..
                    } | Item::Unparsed { .. }
                )
            })
            .map(|(index, _)| index);
        self.end = item.end();
        if matches!(item, Item::Packet { .. }) {
            self.packet = Some((index, self.end));
        }
        self.facts.add(&item);
        self.tail.push(Entry { item, parent });
    }

    fn flush(&mut self) -> Result<(), Refusal> {
        let bytes = serde_json::to_vec(&self.tail).map_err(io_error)?;
        let end = self.bytes + bytes.len() as u64;
        if self.pages.len() >= MAX_INDEX_PAGES || end > self.budget || bytes.len() > MAX_PAGE_BYTES
        {
            return Err(limit());
        }
        let mut file = self.file.lock().map_err(io_error)?;
        file.seek(SeekFrom::Start(self.bytes))
            .and_then(|_| file.write_all(&bytes))
            .map_err(io_error)?;
        let mut kinds = [0_u16; 1026];
        for entry in &self.tail {
            let key = match &entry.item {
                Item::Obu { header, .. } => {
                    usize::from(header.obu_type.raw()) * 32
                        + usize::from(header.temporal_layer_id.get()) * 8
                        + usize::from(header.embedded_layer_id.get())
                }
                Item::Unparsed { .. } => 1024,
                _ => 1025,
            };
            kinds[key] += 1;
        }
        self.pages.push(Page {
            offset: self.bytes,
            len: bytes.len(),
            start: self.tail[0].item.start(),
            digest: Sha256::digest(bytes).into(),
            kinds: (0..).zip(kinds).filter(|(_, count)| *count > 0).collect(),
        });
        self.bytes = end;
        self.tail.clear();
        Ok(())
    }

    fn with_page<T>(&self, number: usize, read: impl FnOnce(&[Entry]) -> T) -> Result<T, Refusal> {
        let Some(page) = self.pages.get(number) else {
            return Ok(read(&self.tail));
        };
        let mut loaded = self.loaded.borrow_mut();
        if let Some((number_loaded, entries)) = loaded.as_ref()
            && *number_loaded == number
        {
            return Ok(read(entries));
        }
        let mut bytes = vec![0; page.len];
        let mut file = self.file.lock().map_err(io_error)?;
        file.seek(SeekFrom::Start(self.base + page.offset))
            .and_then(|_| file.read_exact(&mut bytes))
            .map_err(io_error)?;
        if <[u8; 32]>::from(Sha256::digest(&bytes)) != page.digest {
            return Err(io_error("Framing index checksum mismatch."));
        }
        let entries: Vec<Entry> = serde_json::from_slice(&bytes).map_err(io_error)?;
        if entries.len() != PAGE_ROWS {
            return Err(io_error("Framing index page has an incorrect row count."));
        }
        let result = read(&entries);
        *loaded = Some((number, entries));
        Ok(result)
    }

    fn entry(&self, at: usize) -> Result<Option<Entry>, Refusal> {
        if at >= self.len() {
            return Ok(None);
        }
        self.with_page(at / PAGE_ROWS, |entries| {
            entries.get(at % PAGE_ROWS).cloned()
        })
    }

    pub(super) fn item(&self, at: usize) -> Result<Option<Item>, Refusal> {
        Ok(self.entry(at)?.map(|entry| entry.item))
    }

    pub(super) fn preceding(&self, offset: u64) -> Result<Option<usize>, Refusal> {
        let number = self.pages.partition_point(|page| page.start <= offset);
        let number = if self
            .tail
            .first()
            .is_some_and(|entry| entry.item.start() <= offset)
        {
            self.pages.len()
        } else {
            let Some(number) = number.checked_sub(1) else {
                return Ok(None);
            };
            number
        };
        self.with_page(number, |entries| {
            entries
                .partition_point(|entry| entry.item.start() <= offset)
                .checked_sub(1)
                .map(|slot| number * PAGE_ROWS + slot)
        })
    }

    pub(super) fn page(
        &self,
        filter: RowFilter,
        first: u64,
        count: u32,
    ) -> Result<(u64, Vec<Row>), Refusal> {
        if count == 0 || count > super::MAX_PAGE_ELEMENTS {
            return Err(invalid(format!(
                "Request 1 to {} rows.",
                super::MAX_PAGE_ELEMENTS
            )));
        }
        let total = self
            .pages
            .iter()
            .map(|page| page.count(filter))
            .sum::<usize>()
            + self
                .tail
                .iter()
                .filter(|entry| entry.item.admitted(filter))
                .count();
        if first >= total as u64 && !(first == 0 && total == 0) {
            return Err(invalid(format!(
                "Row {first} is outside the index of {total} rows."
            )));
        }
        let mut skip = usize::try_from(first).unwrap_or(total);
        let mut rows = Vec::new();
        let mut remaining = super::PAGE_BUDGET;
        for number in 0..=self.pages.len() {
            if let Some(page) = self.pages.get(number) {
                let admitted = page.count(filter);
                if skip >= admitted {
                    skip -= admitted;
                    continue;
                }
            }
            let done = self.with_page(number, |entries| {
                for (slot, entry) in entries
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| entry.item.admitted(filter))
                {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let row = row(number * PAGE_ROWS + slot, entry);
                    let size = serde_json::to_vec(&row).map_or(usize::MAX, |bytes| bytes.len() + 1);
                    if size > remaining {
                        return true;
                    }
                    remaining -= size;
                    rows.push(row);
                    if rows.len() == count as usize {
                        return true;
                    }
                }
                false
            })?;
            if done {
                break;
            }
        }
        Ok((total as u64, rows))
    }

    pub(super) fn locate(
        &self,
        filter: RowFilter,
        target: LocateTarget,
    ) -> Result<(Option<u64>, Row), Refusal> {
        let (at, entry) = match target {
            LocateTarget::Element { index } => {
                let at = usize::try_from(index).unwrap_or(usize::MAX);
                let entry = self
                    .entry(at)?
                    .ok_or_else(|| invalid(format!("Element {index} is outside the index.")))?;
                (at, entry)
            }
            LocateTarget::Offset { offset } => {
                let absent = || invalid(format!("No indexed element contains byte {offset}."));
                let at = self.preceding(offset)?.ok_or_else(absent)?;
                let entry = self.entry(at)?.ok_or_else(absent)?;
                if offset < entry.item.end() {
                    (at, entry)
                } else {
                    let at = entry.parent.ok_or_else(absent)?;
                    let entry = self
                        .entry(at)?
                        .filter(|parent| offset < parent.item.end())
                        .ok_or_else(absent)?;
                    (at, entry)
                }
            }
        };
        let mut rank = None;
        if entry.item.admitted(filter) {
            let page = at / PAGE_ROWS;
            let before = self.pages[..page]
                .iter()
                .map(|entry| entry.count(filter))
                .sum::<usize>();
            let same_page = self.with_page(page, |entries| {
                entries[..at % PAGE_ROWS]
                    .iter()
                    .filter(|entry| entry.item.admitted(filter))
                    .count()
            })?;
            rank = Some((before + same_page) as u64);
        }
        Ok((rank, row(at, &entry)))
    }
}

const MAX_DIRECTORY_BYTES: usize = 32 << 20;

pub(super) struct IndexExport {
    file: Arc<Mutex<File>>,
    base: u64,
    bytes: u64,
    directory: Vec<u8>,
}

impl IndexExport {
    pub(super) fn len(&self) -> u64 {
        self.bytes + self.directory.len() as u64 + 8
    }

    pub(super) fn write(self, destination: &mut dyn Write) -> io::Result<()> {
        let mut offset = 0_u64;
        let mut buffer = vec![0; 64 << 10];
        while offset < self.bytes {
            let remaining = usize::try_from(self.bytes - offset).unwrap_or(usize::MAX);
            let count = remaining.min(buffer.len());
            let chunk = &mut buffer[..count];
            {
                let mut file = self
                    .file
                    .lock()
                    .map_err(|_| io::Error::other("The framing index file lock failed."))?;
                file.seek(SeekFrom::Start(self.base + offset))?;
                file.read_exact(chunk)?;
            }
            destination.write_all(chunk)?;
            offset += count as u64;
        }
        destination.write_all(&self.directory)?;
        destination.write_all(&(self.directory.len() as u64).to_le_bytes())
    }
}

fn invalid_cache() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "The framing index directory is invalid.",
    )
}

fn row(at: usize, entry: &Entry) -> Row {
    Row {
        index: at as u64,
        parent: entry.parent.map(|parent| parent as u64),
        element: entry.item.element(),
    }
}

fn limit() -> Refusal {
    Refusal::new(
        AnalysisFailure::LimitReached,
        "The framing index reached its disk limit.",
    )
}

fn io_error(error: impl std::fmt::Display) -> Refusal {
    Refusal::new(
        AnalysisFailure::Io,
        format!("Cannot access the framing index. {error}"),
    )
}

fn invalid(message: String) -> Refusal {
    Refusal::new(AnalysisFailure::InvalidRequest, message)
}
