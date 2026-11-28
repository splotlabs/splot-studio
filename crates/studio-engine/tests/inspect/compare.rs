// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Comparison sides read by the real inspect process from encodes the job
//! path made here: a result pairs with the source its job recorded, results
//! of other sources pair explicitly, and a replaced source is refused.

use std::error::Error;
use std::fs;
use std::path::Path;

use studio_domain::analysis::AnalysisFailure;
use studio_domain::compare::{
    AlignmentBasis, AlignmentRefusal, Clock, ComparisonSession, ComparisonSetup, Compatibility,
    ConfigComparison, DescribeInput, DomainMismatch, DomainProperty, EncodeProvenance, Reference,
    Side, SideFacts, StreamInput,
};
use studio_domain::job::JobSpec;
use studio_domain::layers::LayerDecodeSelection;
use studio_domain::protocol::{Command, ErrorCode, Event};
use studio_domain::settings::OutputKind;

use super::common::{run_job, spec};
use super::{Clip, Inspect, TestResult};

/// Encodes `clip` and returns the job and what it recorded.
fn encoded(
    dir: &Path,
    clip: &Clip,
    name: &str,
    kind: OutputKind,
) -> Result<(JobSpec, EncodeProvenance), Box<dyn Error>> {
    let source = dir.join(format!("{name}.y4m"));
    clip.write(&source)?;
    let job = spec(
        &source,
        &dir.join(format!("{name}.{}", kind.extension())),
        kind,
    );
    let (_, end) = run_job(job.clone(), |_, _| {});
    let Event::Completed { report } = end else {
        return Err(format!("{end:?}").into());
    };
    let provenance = EncodeProvenance::of(&job, &report, &[]).ok_or("an encode")?;
    Ok((job, provenance))
}

fn stream(job: &JobSpec, encode: Option<EncodeProvenance>) -> StreamInput {
    StreamInput {
        path: job.destination.clone(),
        selection: LayerDecodeSelection::Default,
        encode: encode.map(Box::new),
    }
}

fn described(inspect: &mut Inspect, input: DescribeInput) -> Result<(String, SideFacts), String> {
    match inspect.ask(Command::CompareDescribe { input }) {
        Ok(Event::CompareDescribed {
            codec_revision,
            facts,
        }) => Ok((codec_revision, *facts)),
        other => Err(format!("{other:?}")),
    }
}

/// Describes both sides of `setup` and resolves the session.
fn session(inspect: &mut Inspect, setup: ComparisonSetup) -> Result<ComparisonSession, String> {
    let side = |inspect: &mut Inspect, side| {
        described(inspect, setup.describe(side).map_err(|e| e.to_string())?)
    };
    let (revision, a) = side(inspect, Side::A)?;
    let (_, b) = side(inspect, Side::B)?;
    ComparisonSession::resolve(setup, revision, a, b).map_err(|e| e.to_string())
}

#[test]
fn an_encode_pairs_with_its_verified_source_in_both_framings() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut inspect = Inspect::spawn()?;
    for (name, colorspace, kind) in [
        ("eight", "420", OutputKind::Ivf),
        ("ten", "420p10", OutputKind::Av2AnnexB),
    ] {
        let clip = Clip::new(64, 48, colorspace, 5);
        let (job, provenance) = encoded(dir.path(), &clip, name, kind)?;
        let setup = ComparisonSetup::new(Reference::EncodeSource, stream(&job, Some(provenance)));
        assert_eq!(setup.basis, AlignmentBasis::Provenance);
        let session = session(&mut inspect, setup)?;

        // The source is the file the job read, and the result the file it published.
        assert_eq!(session.a.fingerprint, job.fingerprint);
        assert_eq!(session.a.digest, None);
        assert_eq!(
            session.b.digest.as_ref().map(|digest| digest.bytes),
            Some(fs::metadata(job.destination.to_path_buf()?)?.len())
        );
        assert_eq!((session.outputs(Side::A), session.outputs(Side::B)), (5, 5));
        let (a, b) = (
            session.a.domain.ok_or("source format")?,
            session.b.domain.ok_or("result format")?,
        );
        assert_eq!(
            (a.width, a.height, a.bit_depth, a.chroma),
            (b.width, b.height, b.bit_depth, b.chroma)
        );
        assert_eq!(u32::from(a.bit_depth), clip.depth());
        assert!(
            matches!(session.compatibility(), Compatibility::Exact { .. }),
            "{:?}",
            session.compatibility()
        );
        let mapping = session.alignment().map_err(|e| e.to_string())?;
        assert_eq!(
            (mapping.matched(), mapping.partner(Side::A, 4)),
            (5, Some(4))
        );
        // IVF carries one exact timestamp per picture. Annex B carries none.
        match kind {
            OutputKind::Ivf => assert_eq!(session.timestamps_agree(), Some(true)),
            OutputKind::Av2AnnexB => {
                assert_eq!(session.b.clock, Clock::Absent);
                assert_eq!(session.timestamps_agree(), None);
            }
        }
        assert_eq!(
            session.configuration(),
            ConfigComparison::Unknown { a: false, b: true }
        );
        assert!(session.identity().is_some());
    }
    Ok(())
}

#[test]
fn results_pair_explicitly_and_incompatible_domains_are_refused() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut inspect = Inspect::spawn()?;
    let (small, small_job) = encoded(
        dir.path(),
        &Clip::new(64, 48, "420", 3),
        "small",
        OutputKind::Ivf,
    )?;
    let (deep, _) = encoded(
        dir.path(),
        &Clip::new(64, 48, "420p10", 4),
        "deep",
        OutputKind::Ivf,
    )?;
    let (wide, _) = encoded(
        dir.path(),
        &Clip::new(80, 48, "420", 3),
        "wide",
        OutputKind::Av2AnnexB,
    )?;

    // Two results of different sources, paired with no job named.
    let setup = ComparisonSetup::new(Reference::Stream(stream(&small, None)), stream(&deep, None));
    assert_eq!(setup.basis, AlignmentBasis::Timestamps);
    let pair = session(&mut inspect, setup)?;
    assert_eq!(
        pair.configuration(),
        ConfigComparison::Unknown { a: false, b: false }
    );
    assert_eq!(
        pair.compatibility(),
        Compatibility::Refused {
            reasons: vec![DomainMismatch::Differs(DomainProperty::BitDepth)]
        }
    );
    // Unequal lengths at one rate: the longer side keeps an explicit tail.
    let mapping = pair.alignment().map_err(|e| e.to_string())?;
    assert_eq!(
        (
            mapping.matched(),
            mapping.unmatched(Side::A),
            mapping.unmatched(Side::B)
        ),
        (3, 0, 1)
    );

    // Another geometry and no container timestamps on one side.
    let pair = session(
        &mut inspect,
        ComparisonSetup::new(Reference::Stream(stream(&small, None)), stream(&wide, None)),
    )?;
    assert_eq!(
        pair.compatibility(),
        Compatibility::Refused {
            reasons: vec![DomainMismatch::Differs(DomainProperty::Width)]
        }
    );
    assert_eq!(
        pair.alignment(),
        Err(AlignmentRefusal::NoTimestamps(Side::B))
    );

    // The settings of one job are not attached to another file.
    let wrong = ComparisonSetup::new(
        Reference::Stream(stream(&small, None)),
        stream(&deep, Some(small_job)),
    );
    assert_eq!(
        session(&mut inspect, wrong).err().as_deref(),
        Some("The file of side B is not the file its encode job published.")
    );
    Ok(())
}

#[test]
fn a_replaced_source_is_refused_and_other_processes_do_not_describe() -> TestResult {
    let dir = tempfile::tempdir()?;
    let clip = Clip::new(64, 48, "420", 3);
    let (job, provenance) = encoded(dir.path(), &clip, "replaced", OutputKind::Ivf)?;
    let source = DescribeInput::Source {
        source: provenance.source.clone(),
    };
    let mut inspect = Inspect::spawn()?;
    assert!(described(&mut inspect, source.clone()).is_ok());

    // One more frame: another length, so another fingerprint.
    Clip::new(64, 48, "420", 4).write(&job.source.to_path_buf()?)?;
    assert!(matches!(
        inspect.ask(Command::CompareDescribe {
            input: source.clone()
        })?,
        Event::CompareFailed {
            code: AnalysisFailure::SourceChanged,
            ..
        }
    ));
    // The result is still readable, but it no longer has a verified source.
    let result = DescribeInput::Stream {
        path: job.destination.clone(),
        selection: LayerDecodeSelection::Default,
    };
    assert!(described(&mut inspect, result).is_ok());

    for mode in ["worker", "preview"] {
        let mut other = Inspect::spawn_mode(mode)?;
        assert!(
            matches!(
                other.ask(Command::CompareDescribe {
                    input: source.clone()
                })?,
                Event::Error {
                    code: ErrorCode::WrongState,
                    ..
                }
            ),
            "{mode}"
        );
    }
    Ok(())
}
