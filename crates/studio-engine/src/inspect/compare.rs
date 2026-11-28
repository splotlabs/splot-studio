// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Facts about one side of a comparison.
//!
//! A source is probed again and must still match the fingerprint its encode
//! job recorded. A stream is decoded once from its first byte to its last:
//! the decoder is the authority on which pictures a stream presents and in
//! what order. Each picture gives its sample format and names the temporal
//! unit that requested its display. The container timestamp of that unit is
//! the time of the picture. Coded frames that present no picture add
//! nothing. The pass keeps counters only.

use std::io::{BufReader, Read};

use splot_core::stream_reader::TemporalUnitReader;
use splot_decode::{DataProps, DecodeLimits, Decoder, Picture, SendStatus};
use studio_domain::analysis::AnalysisFailure;
use studio_domain::compare::{
    Clock, ClockBuilder, ClockFault, DescribeInput, SampleDomain, SideFacts,
};
use studio_domain::job::{FailureCode, Rational, SourceFacts, SourceFingerprint, WirePath};
use studio_domain::layers::LayerDecodeSelection;
use studio_domain::preview::{GrainPolicy, SourceRef};
use studio_domain::protocol::Event;

use super::document::{Checked, HashReader, Refusal};
use super::identity::multiple_temporal_units;
use crate::job::Stop;
use crate::options::config::chroma_position;
use crate::preview::picture_info;
use crate::source::{self, Selection};
use crate::stream::{
    MAX_UNIT_BYTES, PREVIEW_DECODE_THREADS, operating_point_selection, preview_decoder_settings,
};

/// Answers one describe request.
pub(super) fn describe(input: &DescribeInput, cancelled: &dyn Fn() -> bool) -> Event {
    let facts = match input {
        DescribeInput::Source { source } => describe_source(source),
        DescribeInput::Stream { path, selection } => describe_stream(path, *selection, cancelled),
    };
    match facts {
        Ok(facts) => Event::CompareDescribed {
            codec_revision: env!("SPLOT_CODEC_REVISION").into(),
            facts: Box::new(facts),
        },
        Err(refusal) => Event::CompareFailed {
            code: refusal.code,
            message: refusal.message,
        },
    }
}

fn describe_source(source: &SourceRef) -> Result<SideFacts, Refusal> {
    let path = source
        .path
        .to_path_buf()
        .map_err(|error| Refusal::new(AnalysisFailure::InvalidRequest, error.to_string()))?;
    let selection = Selection {
        fingerprint: Some(source.fingerprint),
        video_stream: Some(source.video_stream),
    };
    match source::open(&path, selection) {
        Ok(opened) => Ok(source_facts(&opened.facts)),
        Err(Stop::Cancelled) => Err(cancelled_read()),
        Err(Stop::Failed(failure)) => Err(Refusal::new(
            match failure.code {
                FailureCode::SourceChanged => AnalysisFailure::SourceChanged,
                FailureCode::InvalidSource => AnalysisFailure::Io,
                _ => AnalysisFailure::Unsupported,
            },
            failure.message,
        )),
    }
}

/// A source presents frame `n` at `n` periods of its declared constant rate.
fn source_facts(facts: &SourceFacts) -> SideFacts {
    SideFacts {
        fingerprint: facts.fingerprint,
        digest: None,
        outputs: facts.frame_count,
        domain: Some(SampleDomain {
            width: facts.width,
            height: facts.height,
            bit_depth: facts.bit_depth,
            chroma: facts.chroma,
            primaries: facts.color.primaries,
            transfer: facts.color.transfer,
            matrix: facts.color.matrix,
            full_range: facts.color.full_range,
            // The code the encoder signals for this siting, for both fields.
            chroma_position: facts.color.chroma_siting.map(|siting| {
                let code = chroma_position(siting) as u32;
                (code, code)
            }),
            film_grain: false,
        }),
        clock: Clock::constant(facts.frame_rate),
    }
}

fn describe_stream(
    path: &WirePath,
    selection: LayerDecodeSelection,
    cancelled: &dyn Fn() -> bool,
) -> Result<SideFacts, Refusal> {
    let io = |what: &str, error: std::io::Error| {
        Refusal::new(AnalysisFailure::Io, format!("{what}. {error}"))
    };
    let path = path
        .to_path_buf()
        .map_err(|error| Refusal::new(AnalysisFailure::InvalidRequest, error.to_string()))?;
    let mut file = source::open_verified(&path).map_err(|e| io("Cannot open the file", e))?;
    let metadata = file
        .metadata()
        .map_err(|e| io("Cannot inspect the file", e))?;
    if !metadata.is_file() {
        return Err(Refusal::new(
            AnalysisFailure::Io,
            "Select a regular AV2 file.",
        ));
    }
    let fingerprint = SourceFingerprint::of(&metadata);
    let mut hashed = HashReader::new(Checked::new((&mut file).take(fingerprint.len), cancelled));
    let decoded = Pass::default().run(BufReader::new(&mut hashed), selection, cancelled);
    if cancelled() {
        return Err(cancelled_read());
    }
    let pass = decoded?;
    // The demuxer stops at the end of the file, so every byte is hashed.
    let digest = hashed.finish();
    let unchanged = file
        .metadata()
        .is_ok_and(|after| SourceFingerprint::of(&after) == fingerprint);
    if digest.bytes != fingerprint.len || !unchanged {
        return Err(Refusal::new(
            AnalysisFailure::SourceChanged,
            "The file changed while Studio read it.",
        ));
    }
    if pass.outputs == 0 {
        return Err(Refusal::new(
            AnalysisFailure::Unsupported,
            "The stream presents no picture.",
        ));
    }
    Ok(SideFacts {
        fingerprint,
        digest: Some(digest),
        outputs: Some(pass.outputs),
        domain: pass.domain.filter(|_| !pass.varies),
        clock: pass.clock.map_or(Clock::Absent, ClockBuilder::finish),
    })
}

fn cancelled_read() -> Refusal {
    Refusal::new(AnalysisFailure::Cancelled, "The read was stopped.")
}

/// What one decode of a stream found.
#[derive(Default)]
struct Pass {
    outputs: u64,
    domain: Option<SampleDomain>,
    varies: bool,
    /// Present for a container that carries timestamps.
    clock: Option<ClockBuilder>,
}

impl Pass {
    fn run(
        mut self,
        reader: impl Read,
        selection: LayerDecodeSelection,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, Refusal> {
        let unsupported = |message: String| Refusal::new(AnalysisFailure::Unsupported, message);
        let decode_failed = |error: &splot_decode::DecodeError| {
            unsupported(format!("Cannot decode the stream. {error}"))
        };
        let selection = operating_point_selection(selection).map_err(|e| decode_failed(&e))?;
        // Timing and format do not depend on film grain synthesis.
        let settings = preview_decoder_settings(
            PREVIEW_DECODE_THREADS,
            DecodeLimits::DEFAULT,
            GrainPolicy::Skip,
        )
        .with_operating_point_selection(selection);
        let mut decoder = Decoder::new(settings)
            .map_err(|error| unsupported(format!("Cannot start the decoder. {error}")))?;
        let mut reader = TemporalUnitReader::with_max_unit_bytes(reader, MAX_UNIT_BYTES);
        while !cancelled() {
            let Some(unit) = reader
                .next_temporal_unit()
                .map_err(|error| unsupported(format!("Cannot read the stream. {error}")))?
            else {
                if let Some(warning) = reader.ivf_warning() {
                    return Err(unsupported(format!(
                        "The IVF file ends inside a record. ({})",
                        warning.rule_id()
                    )));
                }
                decoder.drain();
                self.receive(&mut decoder, cancelled)
                    .map_err(|e| decode_failed(&e))?;
                break;
            };
            let pts = unit.pts;
            let shared = pts.is_some() && multiple_temporal_units(&unit, cancelled);
            // The unit token is its timestamp: a picture names the unit
            // that requested its display by this token.
            let props = DataProps::default().with_user_data(pts.unwrap_or(0));
            if decoder
                .send_data(unit.bytes, props)
                .map_err(|e| decode_failed(&e))?
                != SendStatus::Accepted
            {
                return Err(unsupported(
                    "The decoder refused a temporal unit.".to_owned(),
                ));
            }
            if pts.is_some() {
                let tick = reader
                    .ivf_header()
                    .map_or(Rational { num: 0, den: 0 }, |header| Rational {
                        num: header.timebase_numerator,
                        den: header.timebase_denominator,
                    });
                let clock = self.clock.get_or_insert_with(|| ClockBuilder::new(tick));
                if shared {
                    clock.fail(ClockFault::SharedPacket);
                }
            }
            self.receive(&mut decoder, cancelled)
                .map_err(|e| decode_failed(&e))?;
        }
        Ok(self)
    }

    fn receive(
        &mut self,
        decoder: &mut Decoder,
        cancelled: &dyn Fn() -> bool,
    ) -> splot_decode::Result<()> {
        while !cancelled() {
            let Some(picture) = decoder.wait_picture()? else {
                break;
            };
            self.output(&picture);
        }
        Ok(())
    }

    fn output(&mut self, picture: &Picture) {
        self.outputs += 1;
        if let Some(clock) = &mut self.clock {
            clock.push(picture.provenance().display.user_data);
        }
        let Some(info) = picture_info(picture) else {
            self.varies = true;
            return;
        };
        let color = info.color;
        let found = SampleDomain {
            width: info.y.width,
            height: info.y.height,
            bit_depth: info.bit_depth,
            chroma: info.chroma,
            primaries: color.map(|c| c.primaries),
            transfer: color.map(|c| c.transfer),
            matrix: color.map(|c| c.matrix),
            full_range: color.map(|c| c.full_range),
            chroma_position: info.chroma_position.map(|p| (p.top, p.bottom)),
            film_grain: info.film_grain,
        };
        let domain = self.domain.get_or_insert(found);
        domain.film_grain |= found.film_grain;
        self.varies |= SampleDomain {
            film_grain: domain.film_grain,
            ..found
        } != *domain;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use studio_domain::compare::{AlignmentRefusal, ClockRun, Side, Track, align_by_timestamps};
    use studio_domain::job::ChromaLayout;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn fixture(bytes: &[u8]) -> std::io::Result<(tempfile::TempDir, PathBuf)> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("stream");
        std::fs::write(&path, bytes)?;
        Ok((directory, path))
    }

    fn stream(bytes: &[u8]) -> Result<SideFacts, Box<dyn std::error::Error>> {
        let (_directory, path) = fixture(bytes)?;
        describe_stream(
            &WirePath::from_path(&path),
            LayerDecodeSelection::Default,
            &|| false,
        )
        .map_err(|refusal| refusal.message.into())
    }

    /// AVM made this stream: six coded frames, four pictures. The last two
    /// pictures are presented in the reverse of their coded order, and two
    /// IVF records hold more than one temporal unit.
    #[test]
    fn reordered_coded_frames_count_as_presented_pictures() -> TestResult {
        use studio_domain::identity::{IdentityKind, IdentityRow};

        use crate::inspect::document::Document;
        use crate::inspect::identity::IdentityIndex;

        let bytes = include_bytes!("../../tests/preview/fixtures/syn-frame-sef-families-64x64.ivf");
        let reference =
            include_bytes!("../../tests/preview/fixtures/syn-frame-sef-families-64x64.ivf.yuv");
        let (_directory, path) = fixture(bytes)?;
        let wire = WirePath::from_path(&path);

        // The independent identity scan shows the coded order.
        let document = Document::open(&wire, &|| false).map_err(|e| e.message)?;
        let summary = document.summary();
        let index = IdentityIndex::new(
            GrainPolicy::Skip,
            summary.fingerprint,
            LayerDecodeSelection::Default,
        )
        .map_err(|e| e.message)?;
        index
            .scan(
                &mut std::fs::File::open(&path)?,
                summary.digest.as_deref(),
                &|| false,
            )
            .map_err(|e| e.message)?;
        let coded_of_outputs: Vec<u64> = index
            .page(IdentityKind::Output, 0, 8)
            .map_err(|e| e.message)?
            .rows
            .iter()
            .filter_map(|row| match row {
                IdentityRow::Output { coded_offset, .. } => Some(*coded_offset),
                _ => None,
            })
            .collect();
        assert_eq!(index.summary().coded, 6);
        assert!(!coded_of_outputs.is_sorted(), "{coded_of_outputs:?}");

        let facts = describe_stream(&wire, LayerDecodeSelection::Default, &|| false)
            .map_err(|e| e.message)?;
        // The AVM reference holds one 64 x 64 I420 picture per output.
        let outputs = (reference.len() / (64 * 64 * 3 / 2)) as u64;
        assert_eq!(outputs, coded_of_outputs.len() as u64);
        assert_eq!(facts.outputs, Some(outputs));
        assert_eq!(
            facts.digest.as_ref().map(|digest| digest.bytes),
            Some(facts.fingerprint.len)
        );
        assert_eq!(facts.digest, summary.digest.as_deref().cloned());
        // A timestamp that covers two temporal units orders nothing.
        assert_eq!(
            facts.clock,
            Clock::Unusable {
                fault: ClockFault::SharedPacket
            }
        );
        let track = Track {
            outputs,
            clock: &facts.clock,
        };
        assert_eq!(
            align_by_timestamps(track, track),
            Err(AlignmentRefusal::UnusableClock(
                Side::A,
                ClockFault::SharedPacket
            ))
        );
        Ok(())
    }

    /// The stream of `ivf` with one temporal unit in each record, at
    /// timestamps 0, 1, 2 and so on. No OBU byte changes.
    fn one_unit_per_record(ivf: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        use splot_core::annexb::AnnexBObuCursor;
        use splot_core::span::ByteOffset;
        use splot_core::types::ObuType;

        let header = usize::from(u16::from_le_bytes([ivf[6], ivf[7]]));
        let mut out = ivf[..header].to_vec();
        let mut records = &ivf[header..];
        let mut pts = 0_u64;
        while let Some((size, rest)) = records.split_first_chunk::<4>() {
            let size = usize::try_from(u32::from_le_bytes(*size))?;
            let payload = rest.get(8..8 + size).ok_or("a short record")?;
            records = &rest[8 + size..];
            // Each temporal delimiter starts a unit at the end of the OBU before it.
            let mut cursor = AnnexBObuCursor::new(payload, ByteOffset::new(0));
            let (mut starts, mut end) = (Vec::new(), 0);
            while let Some(obu) = cursor.next_obu()? {
                if obu.header.obu_type == ObuType::TemporalDelimiter {
                    starts.push(end);
                }
                end = usize::try_from(obu.offset.get())? + usize::try_from(obu.size)?;
            }
            starts.push(payload.len());
            for unit in starts.windows(2) {
                let bytes = &payload[unit[0]..unit[1]];
                out.extend(u32::try_from(bytes.len())?.to_le_bytes());
                out.extend(pts.to_le_bytes());
                out.extend(bytes);
                pts += 1;
            }
        }
        Ok(out)
    }

    /// The reordered stream again, with one timestamp for each temporal
    /// unit. Each picture takes the time of the unit that displayed it, so
    /// the picture that was coded before its neighbour and shown after it
    /// has the later time.
    #[test]
    fn reordered_pictures_take_the_time_of_the_unit_that_displays_them() -> TestResult {
        let ivf = one_unit_per_record(include_bytes!(
            "../../tests/preview/fixtures/syn-frame-sef-families-64x64.ivf"
        ))?;
        let facts = stream(&ivf)?;
        assert_eq!(facts.outputs, Some(4));
        let Clock::Exact { runs, .. } = &facts.clock else {
            return Err(format!("{:?}", facts.clock).into());
        };
        assert_eq!(
            runs.as_slice(),
            [ClockRun {
                first: 0,
                start: 0,
                step: 1
            }]
        );
        Ok(())
    }

    /// One timestamp for each picture: the stream aligns with a longer source
    /// at the same rate, and the source keeps an explicit tail.
    #[test]
    fn exact_container_times_align_with_a_constant_rate() -> TestResult {
        let facts = stream(include_bytes!(
            "../../tests/preview/fixtures/syn-tlayer2-dependent-10bit.ivf"
        ))?;
        assert_eq!(facts.outputs, Some(4));
        assert_eq!(
            facts.clock,
            Clock::Exact {
                tick: Rational { num: 1, den: 30 },
                runs: vec![ClockRun {
                    first: 0,
                    start: 0,
                    step: 1
                }]
            }
        );
        let source = Clock::constant(Rational {
            num: 30_000,
            den: 1_000,
        });
        let stream = Track {
            outputs: 4,
            clock: &facts.clock,
        };
        let longer = Track {
            outputs: 6,
            clock: &source,
        };
        let mapping = align_by_timestamps(longer, stream).map_err(|e| e.to_string())?;
        assert_eq!((mapping.matched(), mapping.unmatched(Side::A)), (4, 2));
        assert_eq!(mapping.partner(Side::B, 3), Some(3));
        Ok(())
    }

    #[test]
    fn a_stream_without_container_times_has_no_clock() -> TestResult {
        let facts = stream(include_bytes!(
            "../../tests/preview/fixtures/syn-mlayer2-derived-sef-8bit.obu"
        ))?;
        assert_eq!(facts.outputs, Some(4));
        assert_eq!(facts.clock, Clock::Absent);
        let other = Clock::Exact {
            tick: Rational { num: 1, den: 25 },
            runs: vec![ClockRun {
                first: 0,
                start: 0,
                step: 1,
            }],
        };
        assert_eq!(
            align_by_timestamps(
                Track {
                    outputs: 4,
                    clock: &other
                },
                Track {
                    outputs: 4,
                    clock: &facts.clock
                }
            ),
            Err(AlignmentRefusal::NoTimestamps(Side::B))
        );
        Ok(())
    }

    /// One IVF record holds two temporal units and a coded frame that is
    /// never shown: three coded frames, two outputs, one timestamp.
    #[test]
    fn a_timestamp_shared_by_two_units_is_not_a_clock() -> TestResult {
        let facts = stream(include_bytes!(
            "../../tests/inspect/fixtures/syn-bridge-celu-64x64.ivf"
        ))?;
        assert_eq!(facts.outputs, Some(2));
        assert_eq!(
            facts.clock,
            Clock::Unusable {
                fault: ClockFault::SharedPacket
            }
        );
        Ok(())
    }

    #[test]
    fn depth_grain_and_colour_are_read_from_the_pictures() -> TestResult {
        let grain = stream(include_bytes!(
            "../../tests/preview/fixtures/syn-3frame-filmgrain-inter-64x64-10bit.ivf"
        ))?;
        let domain = grain.domain.ok_or("one format")?;
        assert_eq!((grain.outputs, domain.bit_depth), (Some(3), 10));
        assert!(domain.film_grain);

        let pq = stream(include_bytes!(
            "../../tests/preview/fixtures/syn-pq-420-10bit-64x64.ivf"
        ))?;
        let domain = pq.domain.ok_or("one format")?;
        assert_eq!(
            (domain.primaries, domain.transfer, domain.matrix),
            (Some(9), Some(16), Some(9))
        );
        assert!(!domain.film_grain);

        let padded = stream(include_bytes!(
            "../../tests/preview/fixtures/syn-3frame-padded-filters-422-134x69-10bit-q150.ivf"
        ))?;
        let domain = padded.domain.ok_or("one format")?;
        // The visible size, not the padded coded size.
        assert_eq!(
            (domain.width, domain.height, domain.chroma),
            (134, 69, ChromaLayout::Yuv422)
        );

        // Two sequences with different visible sizes are not one sample domain.
        let cropped = stream(include_bytes!(
            "../../tests/preview/fixtures/syn-2seq-crop-intra-64x64.obu"
        ))?;
        assert_eq!((cropped.outputs, cropped.domain), (Some(2), None));
        Ok(())
    }

    #[test]
    fn truncation_cancellation_and_other_files_are_refused() -> TestResult {
        let bytes = include_bytes!("../../tests/preview/fixtures/syn-frame-sef-families-64x64.ivf");
        let (_directory, path) = fixture(&bytes[..bytes.len() - 7])?;
        let wire = WirePath::from_path(&path);
        let truncated = describe_stream(&wire, LayerDecodeSelection::Default, &|| false);
        assert_eq!(
            truncated.err().map(|refusal| refusal.code),
            Some(AnalysisFailure::Unsupported)
        );
        let stopped = describe_stream(&wire, LayerDecodeSelection::Default, &|| true);
        assert_eq!(
            stopped.err().map(|refusal| refusal.code),
            Some(AnalysisFailure::Cancelled)
        );
        let (_directory, text) = fixture(b"not a stream")?;
        let refused = describe_stream(
            &WirePath::from_path(&text),
            LayerDecodeSelection::Default,
            &|| false,
        );
        assert!(refused.is_err());
        let missing = describe(
            &DescribeInput::Stream {
                path: WirePath::from_path(&path.with_extension("missing")),
                selection: LayerDecodeSelection::Default,
            },
            &|| false,
        );
        assert!(matches!(
            missing,
            Event::CompareFailed {
                code: AnalysisFailure::Io,
                ..
            }
        ));
        Ok(())
    }
}
