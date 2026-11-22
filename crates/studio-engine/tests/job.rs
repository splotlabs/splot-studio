// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Job tests: real encodes of generated Y4M through the job path, in process.

mod common;

use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;

use common::{Clip, TestResult, decode, leftovers, run_announced, run_job, settings, spec};
use splot_encode::SourceFrame;
use studio_domain::job::{
    CheckKind, CheckOutcome, ChromaLayout, Disclosure, FailureCode, Rational, Stage,
};
use studio_domain::protocol::Event;
use studio_domain::settings::OutputKind;
use studio_engine::job::Stop;
use studio_engine::source;

fn code(end: &Event) -> Option<FailureCode> {
    match end {
        Event::Failed { failure } => Some(failure.code),
        _ => None,
    }
}

/// Encodes `clip` to `kind` and asserts a verified, published file.
fn encode_ok(
    clip: &Clip,
    kind: OutputKind,
) -> Result<(tempfile::TempDir, Vec<Event>), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    clip.write(&src)?;
    let dst = dir.path().join(format!("out.{}", kind.extension()));
    let (events, end, working) = run_announced(spec(&src, &dst, kind), |_, _| {});
    assert!(working.is_some(), "no Working before {end:?}");
    let Event::Completed { report } = end else {
        return Err(format!("{clip:?} {kind:?}: {end:?}").into());
    };
    assert_eq!(report.frames, clip.frames as u64);
    assert_eq!(report.bytes, fs::metadata(&dst)?.len());
    let outcomes: Vec<_> = report.checks.iter().map(|c| (c.kind, &c.outcome)).collect();
    assert!(
        matches!(
            outcomes.as_slice(),
            [
                (CheckKind::Structure, CheckOutcome::Passed),
                (CheckKind::SplotDecode, CheckOutcome::Passed),
                (CheckKind::AvmDecode, CheckOutcome::NotRun { .. }),
            ]
        ),
        "{outcomes:?}"
    );
    assert_eq!(
        leftovers(dir.path(), &[&src, &dst]),
        Vec::<std::path::PathBuf>::new()
    );
    Ok((dir, events))
}

#[test]
fn one_frame_8_bit_420_to_ivf() -> TestResult {
    let clip = Clip::new(16, 16, "420", 1);
    let (dir, events) = encode_ok(&clip, OutputKind::Ivf)?;
    let stages: Vec<Stage> = events
        .iter()
        .filter_map(|e| match e {
            Event::Stage { stage } => Some(*stage),
            _ => None,
        })
        .collect();
    assert_eq!(
        stages,
        [
            Stage::Encoding,
            Stage::Finalizing,
            Stage::Verifying,
            Stage::Publishing
        ]
    );
    let decoded = decode(&dir.path().join("out.ivf"))?;
    assert_eq!(decoded.len(), 16 * 16 * 3 / 2);
    Ok(())
}

/// Ten frames: more than the eight-frame lookahead, so packets arrive late
/// and the tail only after the flush. The IVF and OBU of one job decode to
/// the same pictures.
#[test]
fn multi_frame_10_bit_ivf_and_obu_decode_identically() -> TestResult {
    let clip = Clip::new(24, 16, "420p10", 10);
    let (ivf_dir, events) = encode_ok(&clip, OutputKind::Ivf)?;
    let lagged = events.iter().any(|e| {
        matches!(e, Event::Progress { progress } if progress.frames_encoded < progress.frames_read)
    });
    assert!(lagged, "the encoder never held frames back");
    // The second mini-GOP's first packet needs seven coded frames.
    let ahead = events.iter().any(|e| {
        matches!(e, Event::Progress { progress }
            if progress.frames_coded_milli > progress.frames_encoded * 1000)
    });
    assert!(ahead, "the coded count never ran ahead of the packets");
    let coded: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Event::Progress { progress } => Some(progress.frames_coded_milli),
            _ => None,
        })
        .collect();
    assert!(coded.is_sorted(), "the coded count went back: {coded:?}");
    assert_eq!(coded.last(), Some(&10_000));
    let (obu_dir, _) = encode_ok(&clip, OutputKind::Av2AnnexB)?;
    let ivf = decode(&ivf_dir.path().join("out.ivf"))?;
    let obu = decode(&obu_dir.path().join("out.obu"))?;
    assert_eq!(ivf.len(), 24 * 16 * 3 / 2 * 2 * 10);
    assert!(ivf == obu, "IVF and OBU decode differently");

    let bytes = fs::read(ivf_dir.path().join("out.ivf"))?;
    let header = splot_core::ivf::parse_ivf_header(&bytes)?;
    assert_eq!(header.fourcc, *b"AV02");
    assert_eq!(
        (header.timebase_denominator, header.timebase_numerator),
        (30000, 1001)
    );
    // The upstream writer leaves the header count at zero; verification
    // counts the records instead.
    assert_eq!(header.frame_count, 0);
    // The OBU stream is the IVF payloads, back to back.
    let obu_bytes = fs::read(obu_dir.path().join("out.obu"))?;
    let mut cursor = splot_core::ivf::IvfFrameCursor::new(&bytes, header);
    let mut payloads = Vec::new();
    while let splot_core::ivf::IvfFrameRead::Frame(frame) = cursor.next_frame_record()? {
        payloads.extend_from_slice(frame.payload);
    }
    assert!(
        payloads == obu_bytes,
        "OBU bytes differ from the IVF payloads"
    );
    Ok(())
}

#[test]
fn every_advertised_chroma_layout_at_both_depths() -> TestResult {
    for colorspace in ["422", "422p10", "444", "444p10", "mono", "mono10"] {
        let clip = Clip::new(16, 8, colorspace, 2);
        encode_ok(&clip, OutputKind::Av2AnnexB)?;
    }
    // Odd sizes round chroma up.
    encode_ok(&Clip::new(17, 9, "420p10", 2), OutputKind::Ivf)?;
    Ok(())
}

/// The samples handed to the encoder are the fixture's, low bits included.
#[test]
fn import_preserves_8_and_10_bit_samples() -> TestResult {
    for colorspace in ["420", "420p10", "444p10", "mono10"] {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("in.y4m");
        let clip = Clip::new(9, 5, colorspace, 2);
        clip.write(&src)?;
        let mut source =
            source::open(&src, source::Selection::default()).map_err(|e| e.to_string())?;
        let config =
            studio_engine::job::config(&source.header, source.content, settings(OutputKind::Ivf))
                .map_err(|e| e.to_string())?;
        for frame in 0..clip.frames {
            let planes = source
                .read_frame()
                .map_err(|e| e.to_string())?
                .ok_or("missing frame")?;
            let frame_value = SourceFrame::new(&config, frame as u64, planes)?;
            let mut expected = clip.samples(frame);
            expected.resize(3, Vec::new());
            assert_eq!(
                frame_value.planes().as_slice(),
                expected.as_slice(),
                "{colorspace}"
            );
        }
        if clip.depth() == 10 {
            assert!(clip.samples(0)[0].iter().any(|s| s & 3 != 0));
        }
    }
    Ok(())
}

/// What the Y4M header states reaches the output: a clip that states square
/// pixels and left chroma siting has a content interpretation OBU that says
/// so; a clip that states neither has none.
#[test]
fn stated_content_is_written_to_the_output() -> TestResult {
    use splot_core::annexb::parse_annex_b_obus_partial;
    use splot_core::bitio::BitReader;
    use splot_core::headers::content_interpretation::parse_content_interpretation;
    use splot_core::span::ByteOffset;
    use splot_core::types::ObuType;

    let described = |clip: &Clip| -> Result<_, Box<dyn std::error::Error>> {
        let (dir, _) = encode_ok(clip, OutputKind::Av2AnnexB)?;
        let bytes = fs::read(dir.path().join("out.obu"))?;
        let parsed = parse_annex_b_obus_partial(&bytes);
        assert!(parsed.error.is_none(), "{:?}", parsed.error);
        let content: Vec<_> = parsed
            .obus
            .iter()
            .filter(|obu| obu.header.obu_type == ObuType::ContentInterpretation)
            .map(|obu| {
                parse_content_interpretation(&mut BitReader::new(obu.payload, ByteOffset::new(0)))
            })
            .collect::<Result<_, _>>()?;
        Ok(content)
    };
    let content = described(&Clip::new(16, 8, "420mpeg2", 2))?;
    let [content] = content.as_slice() else {
        return Err(format!("{} content interpretation OBUs", content.len()).into());
    };
    assert_eq!(content.derived_sample_aspect_ratio(), Some((1, 1)));
    assert_eq!(content.chroma_sample_position.map(|p| p.top), Some(0));
    assert_eq!(content.color_description, None);

    let silent = Clip {
        tags: "Ip",
        ..Clip::new(16, 8, "420", 2)
    };
    assert_eq!(described(&silent)?, []);
    Ok(())
}

#[test]
fn probe_reports_header_facts() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    Clip::new(17, 9, "444p10", 3).write(&src)?;
    let facts = source::probe(&src, None).map_err(|e| e.to_string())?;
    assert_eq!((facts.width, facts.height, facts.bit_depth), (17, 9, 10));
    assert_eq!(facts.chroma, ChromaLayout::Yuv444);
    assert_eq!(
        facts.frame_rate,
        Rational {
            num: 30000,
            den: 1001
        }
    );
    assert_eq!(facts.pixel_aspect, Some(Rational { num: 1, den: 1 }));
    assert_eq!(facts.frame_count, Some(3));
    assert_eq!(facts.disclosures, [Disclosure::ColorUnspecified]);

    // No I or A tag: scan unknown, aspect unknown.
    let clip = Clip {
        tags: "",
        ..Clip::new(16, 8, "420", 2)
    };
    clip.write(&src)?;
    let facts = source::probe(&src, None).map_err(|e| e.to_string())?;
    assert_eq!(facts.pixel_aspect, None);
    assert_eq!(
        facts.disclosures,
        [Disclosure::ColorUnspecified, Disclosure::ScanUnspecified]
    );

    // A frame header with parameters: the count is not derivable.
    let mut bytes = clip.bytes();
    let at = clip.header().len();
    bytes.splice(at..at + 6, b"FRAME Ixyz\n".iter().copied());
    fs::write(&src, &bytes)?;
    assert_eq!(
        source::probe(&src, None)
            .map_err(|e| e.to_string())?
            .frame_count,
        None
    );
    Ok(())
}

/// Every rejected source fails with its code, publishes nothing and leaves
/// no temporary file.
#[test]
#[allow(clippy::too_many_lines)]
fn rejected_sources() -> TestResult {
    let good = Clip::new(16, 8, "420", 2);
    let truncated = {
        let mut bytes = good.bytes();
        bytes.truncate(bytes.len() - 5);
        bytes
    };
    let out_of_range = {
        let clip = Clip::new(16, 8, "420p10", 1);
        let mut bytes = clip.bytes();
        let at = clip.header().len() + 6;
        bytes[at..at + 2].copy_from_slice(&1024_u16.to_le_bytes());
        bytes
    };
    let header = |text: &str| text.as_bytes().to_vec();
    let cases: Vec<(&str, Vec<u8>, OutputKind, FailureCode)> = vec![
        (
            "zero frames",
            header(&good.header()),
            OutputKind::Ivf,
            FailureCode::InvalidSource,
        ),
        (
            "empty file",
            Vec::new(),
            OutputKind::Ivf,
            FailureCode::InvalidSource,
        ),
        (
            "truncated frame",
            truncated,
            OutputKind::Av2AnnexB,
            FailureCode::InvalidSource,
        ),
        (
            "bad signature",
            header("YUV4MPEG W16 H8 F25:1\nFRAME\n"),
            OutputKind::Ivf,
            FailureCode::InvalidSource,
        ),
        (
            "12-bit",
            header("YUV4MPEG2 W16 H8 F25:1 C420p12\n"),
            OutputKind::Ivf,
            FailureCode::UnsupportedSource,
        ),
        (
            "interlaced",
            header("YUV4MPEG2 W16 H8 F25:1 It\n"),
            OutputKind::Ivf,
            FailureCode::UnsupportedSource,
        ),
        (
            "non-square",
            header("YUV4MPEG2 W16 H8 F25:1 A16:15\n"),
            OutputKind::Ivf,
            FailureCode::UnsupportedSource,
        ),
        (
            "full range",
            header("YUV4MPEG2 W16 H8 F25:1 XCOLORRANGE=FULL\n"),
            OutputKind::Ivf,
            FailureCode::UnsupportedSource,
        ),
        (
            "411",
            header("YUV4MPEG2 W16 H8 F25:1 C411\n"),
            OutputKind::Ivf,
            FailureCode::UnsupportedSource,
        ),
        (
            "beyond AV2",
            header("YUV4MPEG2 W70000 H8 F25:1\n"),
            OutputKind::Av2AnnexB,
            FailureCode::TooLarge,
        ),
        (
            "beyond the budget",
            header("YUV4MPEG2 W60000 H60000 F25:1\n"),
            OutputKind::Av2AnnexB,
            FailureCode::TooLarge,
        ),
        (
            "sample above 10 bits",
            out_of_range,
            OutputKind::Ivf,
            FailureCode::InvalidSource,
        ),
    ];
    for (name, bytes, kind, expected) in cases {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("in.y4m");
        let dst = dir.path().join("out");
        fs::write(&src, bytes)?;
        let (_, end) = run_job(spec(&src, &dst, kind), |_, _| {});
        assert_eq!(code(&end), Some(expected), "{name}: {end:?}");
        assert_eq!(
            leftovers(dir.path(), &[&src]),
            Vec::<std::path::PathBuf>::new(),
            "{name}"
        );
    }
    Ok(())
}

/// IVF stores sizes in 16 bits; AV2 itself allows 65536.
#[test]
fn ivf_size_limit_is_checked_before_output() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    fs::write(&src, "YUV4MPEG2 W65536 H2 F25:1\nFRAME\n")?;
    let dst = dir.path().join("out.ivf");
    let (_, end) = run_job(spec(&src, &dst, OutputKind::Ivf), |_, _| {});
    assert_eq!(code(&end), Some(FailureCode::TooLarge), "{end:?}");
    assert_eq!(
        leftovers(dir.path(), &[&src]),
        Vec::<std::path::PathBuf>::new()
    );
    Ok(())
}

#[test]
fn the_source_is_never_the_destination() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    let clip = Clip::new(16, 8, "420", 1);
    clip.write(&src)?;
    let hard = dir.path().join("hard.ivf");
    fs::hard_link(&src, &hard)?;
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut aliases = vec![src.clone(), dir.path().join(".").join("in.y4m")];
    #[cfg(unix)]
    {
        let soft = dir.path().join("soft.ivf");
        std::os::unix::fs::symlink(&src, &soft)?;
        aliases.extend([hard, soft]);
    }
    // Without inode identity a hard link is not recognized as the source; it
    // is still refused, as an existing destination.
    #[cfg(not(unix))]
    {
        let (_, end) = run_job(spec(&src, &hard, OutputKind::Ivf), |_, _| {});
        assert_eq!(code(&end), Some(FailureCode::DestinationExists), "{end:?}");
    }
    for dst in &aliases {
        let (_, end) = run_job(spec(&src, dst, OutputKind::Ivf), |_, _| {});
        assert_eq!(
            code(&end),
            Some(FailureCode::SourceIsDestination),
            "{dst:?}: {end:?}"
        );
    }
    assert!(fs::read(&src)? == clip.bytes(), "the source changed");
    Ok(())
}

#[test]
fn an_existing_destination_is_never_replaced() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    Clip::new(16, 8, "420", 1).write(&src)?;
    let dst = dir.path().join("out.ivf");
    fs::write(&dst, "keep me")?;
    let (_, end) = run_job(spec(&src, &dst, OutputKind::Ivf), |_, _| {});
    assert_eq!(code(&end), Some(FailureCode::DestinationExists), "{end:?}");
    #[cfg(unix)]
    {
        // A dangling symlink is something that exists, too.
        let link = dir.path().join("dangling.ivf");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &link)?;
        let (_, end) = run_job(spec(&src, &link, OutputKind::Ivf), |_, _| {});
        assert_eq!(code(&end), Some(FailureCode::DestinationExists), "{end:?}");
        assert!(!dir.path().join("nowhere").exists());
        fs::remove_file(&link)?;
    }
    assert_eq!(fs::read(&dst)?, b"keep me");
    assert_eq!(
        leftovers(dir.path(), &[&src, &dst]),
        Vec::<std::path::PathBuf>::new()
    );
    Ok(())
}

/// A file that appears at the destination while the job runs is kept, and
/// the verified output is discarded.
#[test]
fn a_destination_that_appears_during_the_job_is_kept() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    Clip::new(16, 8, "420", 3).write(&src)?;
    let dst = dir.path().join("out.obu");
    let racer = dst.clone();
    let (_, end, working) =
        run_announced(spec(&src, &dst, OutputKind::Av2AnnexB), move |event, _| {
            if matches!(
                event,
                Event::Stage {
                    stage: Stage::Publishing
                }
            ) {
                let _ = fs::write(&racer, "someone else's");
            }
        });
    assert_eq!(code(&end), Some(FailureCode::DestinationExists), "{end:?}");
    assert!(
        working.is_some(),
        "Failed after the file was made, without Working"
    );
    assert_eq!(fs::read(&dst)?, b"someone else's");
    assert_eq!(
        leftovers(dir.path(), &[&src, &dst]),
        Vec::<std::path::PathBuf>::new()
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn an_unwritable_destination_directory_is_an_output_failure() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    Clip::new(16, 8, "420", 1).write(&src)?;
    let locked = dir.path().join("locked");
    fs::create_dir(&locked)?;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555))?;
    if fs::write(locked.join("probe"), "").is_ok() {
        // Running as root: permissions do not apply, nothing to test.
        return Ok(());
    }
    let (_, end) = run_job(
        spec(&src, &locked.join("out.ivf"), OutputKind::Ivf),
        |_, _| {},
    );
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))?;
    assert_eq!(code(&end), Some(FailureCode::Output), "{end:?}");
    assert_eq!(leftovers(&locked, &[]), Vec::<std::path::PathBuf>::new());
    Ok(())
}

/// Cancelling while encoding, and while verifying, publishes nothing and
/// removes the temporary file.
#[test]
fn cancel_mid_encode_and_mid_verify_leaves_nothing() -> TestResult {
    for at_stage in [Stage::Encoding, Stage::Verifying] {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("in.y4m");
        Clip::new(16, 8, "420", 12).write(&src)?;
        let dst = dir.path().join("out.ivf");
        let (events, end, working) =
            run_announced(spec(&src, &dst, OutputKind::Ivf), move |event, cancel| {
                let hit = match (at_stage, event) {
                    (Stage::Encoding, Event::Progress { progress }) => progress.frames_read == 3,
                    (stage, Event::Stage { stage: now }) => {
                        stage == *now && stage != Stage::Encoding
                    }
                    _ => false,
                };
                if hit {
                    cancel.store(true, Ordering::Relaxed);
                }
            });
        assert!(matches!(end, Event::Cancelled), "{at_stage:?}: {end:?}");
        assert!(
            working.is_some(),
            "{at_stage:?}: Cancelled after the file was made"
        );
        if at_stage == Stage::Encoding {
            let read = events.iter().filter_map(|e| match e {
                Event::Progress { progress } => Some(progress.frames_read),
                _ => None,
            });
            assert_eq!(read.max(), Some(3), "stopped at the next frame boundary");
        }
        assert_eq!(
            leftovers(dir.path(), &[&src]),
            Vec::<std::path::PathBuf>::new()
        );
    }
    Ok(())
}

#[test]
fn a_foreign_or_missing_source_is_invalid() -> TestResult {
    let dir = tempfile::tempdir()?;
    let missing = dir.path().join("missing.y4m");
    let (_, end) = run_job(
        spec(&missing, &dir.path().join("o"), OutputKind::Ivf),
        |_, _| {},
    );
    assert_eq!(code(&end), Some(FailureCode::InvalidSource), "{end:?}");
    let (_, end) = run_job(
        spec(dir.path(), &dir.path().join("o"), OutputKind::Ivf),
        |_, _| {},
    );
    assert_eq!(code(&end), Some(FailureCode::InvalidSource), "{end:?}");
    assert!(matches!(
        source::probe(Path::new("/nonexistent/x.y4m"), None),
        Err(Stop::Failed(_))
    ));
    Ok(())
}

/// Verification fails on damaged files and on a count that does not match.
#[test]
fn verification_catches_damaged_output() -> TestResult {
    use std::sync::atomic::AtomicBool;
    use studio_engine::verify::{Expected, run};

    let clip = Clip::new(16, 8, "420", 3);
    for kind in [OutputKind::Ivf, OutputKind::Av2AnnexB] {
        let (dir, _) = encode_ok(&clip, kind)?;
        let header = source::open(&dir.path().join("in.y4m"), source::Selection::default())
            .map_err(|e| e.to_string())?
            .header;
        let config =
            studio_engine::job::config(&header, header.content_description(), settings(kind))
                .map_err(|e| e.to_string())?;
        let good = fs::read(dir.path().join(format!("out.{}", kind.extension())))?;
        let damaged = dir.path().join("damaged");
        let outcomes =
            |bytes: &[u8], frames: u64| -> Result<Vec<CheckOutcome>, Box<dyn std::error::Error>> {
                fs::write(&damaged, bytes)?;
                let file = fs::File::open(&damaged)?;
                let config = config.clone();
                let checks = studio_engine::job::spawn(move || {
                    let expected = Expected {
                        config: &config,
                        kind,
                        frames,
                    };
                    run(&file, expected, &AtomicBool::new(false))
                })?
                .join()
                .map_err(|_| "verification panicked")?
                .map_err(|e| e.to_string())?;
                Ok(checks.into_iter().map(|c| c.outcome).collect())
            };
        let passed = |o: &CheckOutcome| *o == CheckOutcome::Passed;
        let fails = |o: &CheckOutcome| matches!(o, CheckOutcome::Failed { .. });
        let intact = outcomes(&good, 3)?;
        assert!(
            passed(&intact[0]) && passed(&intact[1]),
            "{kind:?}: {intact:?}"
        );
        let miscounted = outcomes(&good, 4)?;
        assert!(
            fails(&miscounted[0]) && fails(&miscounted[1]),
            "{kind:?}: {miscounted:?}"
        );
        // Inside the last OBU, and (for these clips) the whole last OBU.
        for cut in [1, 3] {
            let truncated = outcomes(&good[..good.len() - cut], 3)?;
            assert!(fails(&truncated[0]), "{kind:?} cut {cut}: {truncated:?}");
        }
        let other = if kind == OutputKind::Ivf {
            b"not an ivf".as_slice()
        } else {
            b"DKIF".as_slice()
        };
        assert!(fails(&outcomes(other, 3)?[0]), "{kind:?}");
    }
    Ok(())
}

/// A job cancelled before its temporary file exists announces none and
/// leaves none.
#[test]
fn cancel_before_the_file_exists_announces_nothing() -> TestResult {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.y4m");
    Clip::new(16, 8, "420", 3).write(&src)?;
    let dst = dir.path().join("out.ivf");
    let (events, end, working) =
        run_announced(spec(&src, &dst, OutputKind::Ivf), |event, cancel| {
            if matches!(
                event,
                Event::Stage {
                    stage: Stage::Encoding
                }
            ) {
                cancel.store(true, Ordering::Relaxed);
            }
        });
    assert!(matches!(end, Event::Cancelled), "{end:?}");
    assert!(working.is_none(), "{events:?}");
    assert_eq!(
        leftovers(dir.path(), &[&src]),
        Vec::<std::path::PathBuf>::new()
    );
    Ok(())
}
