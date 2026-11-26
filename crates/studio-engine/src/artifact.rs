// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! First-pass statistics for one frozen source and encoder policy.
//!
//! The file contains a versioned signature, two little-endian `u32` values
//! (JSON header length and frame count), the JSON header, one little-endian
//! `u64` bit count per input frame, and a SHA-256 checksum of the preceding
//! bytes. The checksum detects corruption. Provenance checks compare the
//! source and encoder policy with the second-pass job.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use studio_domain::job::{SourceFacts, SourceReader};
use studio_domain::options::{EffectiveOption, OptionRequest};
use studio_domain::protocol::OptionsIdentity;
use thiserror::Error;

const SIGNATURE: &[u8; 8] = b"SPLT1ST\x01";
const MAX_HEADER_BYTES: usize = 1024 * 1024;
const DIGEST_BYTES: usize = 32;
const PREFIX_BYTES: usize = SIGNATURE.len() + 2 * size_of::<u32>();
const COUNT_BYTES: usize = size_of::<u64>();

/// The largest supported first pass, with up to 16 MiB of bit counts.
pub const MAX_FRAMES: usize = 2 * 1024 * 1024;

/// The source bytes and the route that supplied frames to the encoder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceIdentity {
    /// SHA-256 of the complete source file, in lowercase hexadecimal.
    pub sha256: String,
    /// The selected stream and the sample properties used by the encoder.
    pub facts: SourceFacts,
    /// The media adapter identity and decoding parameters. Absent for Y4M.
    pub adapter: Option<String>,
}

/// The provenance that must match before a second pass can use the counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    /// The source used to generate the statistics.
    pub source: SourceIdentity,
    /// The option schema and codec revision. This pins the first-pass transform.
    pub identity: OptionsIdentity,
    /// The frozen request for the final encode.
    pub settings: OptionRequest,
    /// The resolved final policy before the job supplies first-pass statistics.
    pub policy: Vec<EffectiveOption>,
}

/// An artifact whose length, checksum and values passed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// The source and settings recorded by the first pass.
    pub header: Header,
    /// Coded bits of each input frame, in presentation order.
    pub bits: Vec<u64>,
    /// SHA-256 of the complete artifact, in lowercase hexadecimal.
    pub sha256: String,
}

/// A failure to read, write or use first-pass statistics.
#[derive(Debug, Error)]
pub enum ArtifactError {
    /// The user stopped the operation.
    #[error("The first-pass operation stopped.")]
    Cancelled,
    /// The artifact or the source file could not be read or written.
    #[error("Cannot access first-pass data. {0}")]
    Io(#[from] io::Error),
    /// The header could not be encoded or decoded.
    #[error("The first-pass header is invalid.")]
    Json(#[from] serde_json::Error),
    /// The signature or format version is not supported.
    #[error("Use a first-pass artifact in the current format.")]
    Format,
    /// The header length or number of frames is outside the supported bounds.
    #[error("The first-pass header size or frame count exceeds the supported limits.")]
    Limits,
    /// Actual file length differs from the declared length.
    #[error("The first-pass file has missing or extra bytes.")]
    Length,
    /// The embedded checksum does not match the stored bytes.
    #[error("The first-pass checksum does not match the file contents.")]
    Checksum,
    /// The header has incomplete source or codec identity.
    #[error("The first-pass header has an invalid source or codec identity.")]
    Identity,
    /// A count is zero, is not byte-aligned, or makes the bit total overflow.
    #[error("The first-pass bit count for frame {0} is invalid.")]
    Bits(usize),
    /// The artifact records a different source, stream or decoding route.
    #[error("Generate first-pass statistics for this source and selected video stream.")]
    SourceMismatch,
    /// The artifact records a different option schema or codec revision.
    #[error("Generate first-pass statistics with this engine and option schema.")]
    CodecMismatch,
    /// The artifact records a different final policy.
    #[error("Generate first-pass statistics for these encoder settings.")]
    PolicyMismatch,
    /// The actual source frame count differs from the recorded count.
    #[error("The first pass has {stored} frames. The source has {expected} frames.")]
    FrameCount {
        /// The number of frames in the artifact.
        stored: u64,
        /// The exact count from the source.
        expected: u64,
    },
}

impl Artifact {
    /// Checks that these statistics belong to the current job.
    ///
    /// # Errors
    /// Returns a provenance or frame count error when a field differs.
    pub fn verify(
        &self,
        expected: &Header,
        exact_frame_count: Option<u64>,
    ) -> Result<(), ArtifactError> {
        if self.header.source != expected.source {
            return Err(ArtifactError::SourceMismatch);
        }
        if self.header.identity != expected.identity {
            return Err(ArtifactError::CodecMismatch);
        }
        if self.header.settings != expected.settings || self.header.policy != expected.policy {
            return Err(ArtifactError::PolicyMismatch);
        }
        check_frame_count(self.bits.len(), exact_frame_count)
    }
}

/// Hashes the bytes from the reader's current position through EOF.
///
/// # Errors
/// Returns a cancellation or read error.
pub fn hash_source(reader: &mut impl Read, cancel: &AtomicBool) -> Result<String, ArtifactError> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 16 * 1024];
    loop {
        stop_if(cancel)?;
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        hash.update(&buffer[..count]);
    }
    stop_if(cancel)?;
    Ok(hex(&hash.finalize()))
}

/// Reads and validates a first-pass artifact from an open regular file.
///
/// # Errors
/// Returns an error for invalid size, format, content, checksum, cancellation
/// or a file read failure.
pub fn read(file: &File, cancel: &AtomicBool) -> Result<Artifact, ArtifactError> {
    stop_if(cancel)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(ArtifactError::Format);
    }
    let mut reader = BufReader::new(file);
    reader.rewind()?;
    let mut prefix = [0; PREFIX_BYTES];
    reader.read_exact(&mut prefix)?;
    let [signature @ .., h0, h1, h2, h3, c0, c1, c2, c3] = prefix;
    if signature != *SIGNATURE {
        return Err(ArtifactError::Format);
    }
    let header_bytes = u32::from_le_bytes([h0, h1, h2, h3]) as usize;
    let frames = u32::from_le_bytes([c0, c1, c2, c3]) as usize;
    check_limits(header_bytes, frames)?;
    let bytes = (PREFIX_BYTES + header_bytes + frames * COUNT_BYTES + DIGEST_BYTES) as u64;
    if metadata.len() != bytes {
        return Err(ArtifactError::Length);
    }

    let mut hash = Sha256::new();
    hash.update(prefix);
    let mut header_data = vec![0; header_bytes];
    read_hashed(&mut reader, &mut header_data, &mut hash)?;
    stop_if(cancel)?;

    let mut bits = Vec::with_capacity(frames);
    let mut total = 0_u64;
    for index in 0..frames {
        stop_if(cancel)?;
        let mut word = [0; COUNT_BYTES];
        read_hashed(&mut reader, &mut word, &mut hash)?;
        let value = u64::from_le_bytes(word);
        total = check_bits(index, value, total)?;
        bits.push(value);
    }
    let checksum = hash.clone().finalize();
    let mut stored = [0; DIGEST_BYTES];
    reader.read_exact(&mut stored)?;
    if stored.as_slice() != checksum.as_slice() {
        return Err(ArtifactError::Checksum);
    }
    if reader.read(&mut [0])? != 0 {
        return Err(ArtifactError::Length);
    }
    stop_if(cancel)?;

    let header: Header = serde_json::from_slice(&header_data)?;
    check_header(&header)?;
    check_frame_count(frames, header.source.facts.frame_count)?;
    hash.update(stored);
    Ok(Artifact {
        header,
        bits,
        sha256: hex(&hash.finalize()),
    })
}

/// Writes statistics to an empty temporary file and synchronizes the data.
/// The caller can publish the file after this function succeeds.
///
/// # Errors
/// Returns an error for invalid input, cancellation or a write failure.
pub fn write(
    mut file: &File,
    header: &Header,
    bits: &[u64],
    cancel: &AtomicBool,
) -> Result<String, ArtifactError> {
    stop_if(cancel)?;
    check_header(header)?;
    let header_data = serde_json::to_vec(header)?;
    check_limits(header_data.len(), bits.len())?;
    check_frame_count(bits.len(), header.source.facts.frame_count)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != 0 {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "Use an empty regular file.").into(),
        );
    }
    file.rewind()?;

    let mut writer = BufWriter::new(file);
    let mut hash = Sha256::new();
    let header_bytes = u32::try_from(header_data.len()).map_err(|_| ArtifactError::Limits)?;
    let frames = u32::try_from(bits.len()).map_err(|_| ArtifactError::Limits)?;
    write_hashed(&mut writer, SIGNATURE, &mut hash)?;
    write_hashed(&mut writer, &header_bytes.to_le_bytes(), &mut hash)?;
    write_hashed(&mut writer, &frames.to_le_bytes(), &mut hash)?;
    write_hashed(&mut writer, &header_data, &mut hash)?;
    let mut total = 0_u64;
    for (index, &bits) in bits.iter().enumerate() {
        stop_if(cancel)?;
        total = check_bits(index, bits, total)?;
        write_hashed(&mut writer, &bits.to_le_bytes(), &mut hash)?;
    }
    let checksum = hash.clone().finalize();
    writer.write_all(&checksum)?;
    hash.update(checksum);
    writer.flush()?;
    file.sync_all()?;
    stop_if(cancel)?;
    Ok(hex(&hash.finalize()))
}

fn check_header(header: &Header) -> Result<(), ArtifactError> {
    let adapter_matches = match header.source.facts.reader {
        SourceReader::Y4m => header.source.adapter.is_none(),
        SourceReader::Ffmpeg => header
            .source
            .adapter
            .as_ref()
            .is_some_and(|s| !s.is_empty()),
    };
    if !adapter_matches
        || !is_lowercase_hex(&header.source.sha256, 2 * DIGEST_BYTES)
        || !is_lowercase_hex(&header.identity.codec_revision, 40)
        || header.identity.schema == 0
    {
        return Err(ArtifactError::Identity);
    }
    Ok(())
}

fn is_lowercase_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn check_limits(header_bytes: usize, frames: usize) -> Result<(), ArtifactError> {
    if !(1..=MAX_HEADER_BYTES).contains(&header_bytes) || !(1..=MAX_FRAMES).contains(&frames) {
        return Err(ArtifactError::Limits);
    }
    Ok(())
}

fn check_frame_count(stored: usize, expected: Option<u64>) -> Result<(), ArtifactError> {
    let stored = stored as u64;
    if let Some(expected) = expected
        && stored != expected
    {
        return Err(ArtifactError::FrameCount { stored, expected });
    }
    Ok(())
}

fn check_bits(index: usize, bits: u64, total: u64) -> Result<u64, ArtifactError> {
    if bits == 0 || !bits.is_multiple_of(8) {
        return Err(ArtifactError::Bits(index));
    }
    total.checked_add(bits).ok_or(ArtifactError::Bits(index))
}

fn stop_if(cancel: &AtomicBool) -> Result<(), ArtifactError> {
    if cancel.load(Ordering::Relaxed) {
        Err(ArtifactError::Cancelled)
    } else {
        Ok(())
    }
}

fn read_hashed(reader: &mut impl Read, bytes: &mut [u8], hash: &mut Sha256) -> io::Result<()> {
    reader.read_exact(bytes)?;
    hash.update(bytes);
    Ok(())
}

fn write_hashed(writer: &mut impl Write, bytes: &[u8], hash: &mut Sha256) -> io::Result<()> {
    writer.write_all(bytes)?;
    hash.update(bytes);
    Ok(())
}

pub(crate) fn hex(digest: &[u8]) -> String {
    let mut text = String::with_capacity(2 * digest.len());
    for byte in digest {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::io::SeekFrom;

    use studio_domain::job::{ChromaLayout, Rational, SourceFingerprint};
    use studio_domain::options::{OPTIONS_SCHEMA_VERSION, OptionValue};
    use studio_domain::settings::{EncoderSettings, OutputKind};

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn header() -> Header {
        Header {
            source: SourceIdentity {
                sha256: "ab".repeat(DIGEST_BYTES),
                facts: SourceFacts {
                    reader: SourceReader::Y4m,
                    fingerprint: SourceFingerprint {
                        len: 4096,
                        modified: Some((1, 0)),
                    },
                    video_stream: 0,
                    video_tracks: vec![],
                    omissions: studio_domain::job::Omissions::default(),
                    width: 64,
                    height: 48,
                    frame_rate: Rational { num: 30, den: 1 },
                    pixel_aspect: None,
                    bit_depth: 8,
                    chroma: ChromaLayout::Yuv420,
                    color: studio_domain::job::SourceColor::default(),
                    frame_count: Some(2),
                    disclosures: vec![],
                },
                adapter: None,
            },
            identity: OptionsIdentity {
                schema: OPTIONS_SCHEMA_VERSION,
                codec_revision: env!("SPLOT_CODEC_REVISION").into(),
            },
            settings: EncoderSettings {
                output: OutputKind::Ivf,
                preset: crate::cli::DEFAULT_PRESET,
                crf: crate::cli::DEFAULT_CRF,
            }
            .into(),
            policy: vec![EffectiveOption {
                key: "width".into(),
                requested: None,
                value: OptionValue::Integer(64),
                screen: None,
                origin: OptionValue::Choice("source".into()),
                screen_origin: None,
            }],
        }
    }

    #[test]
    fn durable_format_roundtrip_and_job_checks() -> TestResult {
        let file = tempfile::tempfile()?;
        let cancel = AtomicBool::new(false);
        let header = header();
        let digest = write(&file, &header, &[800, 720], &cancel)?;
        let artifact = read(&file, &cancel)?;
        assert_eq!(artifact.header, header);
        assert_eq!(artifact.bits, [800, 720]);
        assert_eq!(artifact.sha256, digest);
        artifact.verify(&header, Some(2))?;

        let mut reader = &file;
        reader.rewind()?;
        assert_eq!(hash_source(&mut reader, &cancel)?, digest);
        assert!(matches!(
            artifact.verify(&header, Some(3)),
            Err(ArtifactError::FrameCount {
                expected: 3,
                stored: 2
            })
        ));
        let mut wrong = header.clone();
        wrong.source.sha256 = "cd".repeat(DIGEST_BYTES);
        assert!(matches!(
            artifact.verify(&wrong, Some(2)),
            Err(ArtifactError::SourceMismatch)
        ));
        wrong = header.clone();
        wrong.identity.schema += 1;
        assert!(matches!(
            artifact.verify(&wrong, Some(2)),
            Err(ArtifactError::CodecMismatch)
        ));
        wrong = header;
        wrong.policy[0].value = OptionValue::Integer(128);
        assert!(matches!(
            artifact.verify(&wrong, Some(2)),
            Err(ArtifactError::PolicyMismatch)
        ));
        Ok(())
    }

    #[test]
    fn bounds_and_signature_checked_before_allocating() -> TestResult {
        let header_too_large = u32::try_from(MAX_HEADER_BYTES + 1)?;
        let too_many_frames = u32::try_from(MAX_FRAMES + 1)?;
        for (header_size, frames) in [(header_too_large, 1), (1, too_many_frames), (1, 0)] {
            let mut file = tempfile::tempfile()?;
            file.write_all(SIGNATURE)?;
            file.write_all(&header_size.to_le_bytes())?;
            file.write_all(&frames.to_le_bytes())?;
            assert!(matches!(
                read(&file, &AtomicBool::new(false)),
                Err(ArtifactError::Limits)
            ));
        }
        let mut file = tempfile::tempfile()?;
        file.write_all(&[0; PREFIX_BYTES])?;
        assert!(matches!(
            read(&file, &AtomicBool::new(false)),
            Err(ArtifactError::Format)
        ));
        Ok(())
    }

    #[test]
    fn corruption_truncation_and_extra_bytes_are_refused() -> TestResult {
        let mut file = tempfile::tempfile()?;
        let cancel = AtomicBool::new(false);
        write(&file, &header(), &[800, 720], &cancel)?;
        let bytes = file.metadata()?.len();
        file.seek(SeekFrom::End(0))?;
        file.write_all(&[0])?;
        assert!(matches!(read(&file, &cancel), Err(ArtifactError::Length)));
        file.set_len(bytes - 1)?;
        assert!(matches!(read(&file, &cancel), Err(ArtifactError::Length)));
        file.set_len(bytes)?;
        file.seek(SeekFrom::End(-1))?;
        let mut tail = [0];
        file.read_exact(&mut tail)?;
        file.seek(SeekFrom::End(-1))?;
        file.write_all(&[tail[0] ^ 1])?;
        assert!(matches!(read(&file, &cancel), Err(ArtifactError::Checksum)));
        Ok(())
    }

    #[test]
    fn valid_checksum_cannot_hide_invalid_counts() -> TestResult {
        for (count, invalid_index) in [([0, 8], 0), ([8, 15], 1), ([u64::MAX - 7, 8], 1)] {
            let header = serde_json::to_vec(&header())?;
            let mut prefix = Vec::from(*SIGNATURE);
            prefix.extend(u32::try_from(header.len())?.to_le_bytes());
            prefix.extend(u32::try_from(count.len())?.to_le_bytes());
            prefix.extend(header);
            for bits in count {
                prefix.extend(bits.to_le_bytes());
            }
            let mut file = tempfile::tempfile()?;
            file.write_all(&prefix)?;
            file.write_all(&Sha256::digest(&prefix))?;
            let cancel = AtomicBool::new(false);
            assert!(
                matches!(read(&file, &cancel), Err(ArtifactError::Bits(index)) if index == invalid_index)
            );
        }
        Ok(())
    }

    #[test]
    fn cancelled_reads_hashes_and_writes_stop() -> TestResult {
        let mut reader = Cursor::new(b"abc");
        let cancel = AtomicBool::new(false);
        assert_eq!(
            hash_source(&mut reader, &cancel)?,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
        let file = tempfile::tempfile()?;
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            hash_source(&mut reader, &cancel),
            Err(ArtifactError::Cancelled)
        ));
        assert!(matches!(
            read(&file, &cancel),
            Err(ArtifactError::Cancelled)
        ));
        assert!(matches!(
            write(&file, &header(), &[8, 16], &cancel),
            Err(ArtifactError::Cancelled)
        ));
        assert_eq!(file.metadata()?.len(), 0);
        Ok(())
    }
}
