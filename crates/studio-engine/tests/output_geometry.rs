// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Checks coded and visible sizes for resized and cropped outputs.

mod common;

use std::fs;
use std::sync::atomic::AtomicBool;

use splot_encode::{BitDepth, Config, CroppingWindow, Encoder, Preset, SourceFrame, ThreadCount};
use studio_domain::job::{CheckKind, CheckOutcome};
use studio_domain::settings::OutputKind;
use studio_engine::output::PacketSink;
use studio_engine::verify::{Expected, run};

use common::{Clip, TestResult};

#[test]
fn verifies_resized_and_cropped_ivf_and_obu() -> TestResult {
    studio_engine::job::spawn(|| -> Result<(), String> {
        for (depth, chroma, sample_bytes) in
            [(BitDepth::Eight, "420", 1), (BitDepth::Ten, "420p10", 2)]
        {
            check_output(depth, chroma, sample_bytes).map_err(|error| error.to_string())?;
        }
        Ok(())
    })?
    .join()
    .map_err(|_| "Output verification panicked.")??;
    Ok(())
}

fn check_output(depth: BitDepth, chroma: &'static str, sample_bytes: usize) -> TestResult {
    let clip = Clip::new(67, 49, chroma, 3);
    let mut config = Config::new(67, 49);
    config.bit_depth = depth;
    config.preset = Preset::new(10)?;
    config.threads = ThreadCount::from(1_usize);
    config.resize_denominator = Some(16);
    config.crop = Some(CroppingWindow {
        left: 2,
        right: 1,
        top: 2,
        bottom: 0,
    });
    let mut encoder = Encoder::new(config.clone())?;
    let mut packets = Vec::new();
    for index in 0..clip.frames {
        let mut planes = clip.samples(index).into_iter();
        let samples = std::array::from_fn(|_| planes.next().expect("fixture has three planes"));
        encoder.send_frame(SourceFrame::new(&config, index as u64, samples)?)?;
        while let Some(packet) = encoder.receive_packet()? {
            packets.push(packet);
        }
    }
    encoder.flush()?;
    while let Some(packet) = encoder.receive_packet()? {
        packets.push(packet);
    }
    assert_eq!(packets.len(), clip.frames);

    let dir = tempfile::tempdir()?;
    let mut decoded = Vec::new();
    for kind in OutputKind::ALL {
        let mut sink = PacketSink::new(kind, Vec::new(), &config)?;
        for packet in &packets {
            sink.write(packet)?;
        }
        let bytes = sink.finish()?;
        if kind == OutputKind::Ivf {
            let header = splot_core::ivf::parse_ivf_header(&bytes)?;
            assert_eq!((header.width, header.height), (34, 25));
        }

        let path = dir.path().join(format!("out.{}", kind.extension()));
        fs::write(&path, bytes)?;
        let checks = run(
            &fs::File::open(&path)?,
            Expected {
                config: &config,
                kind,
                frames: clip.frames as u64,
            },
            &AtomicBool::new(false),
        )?;
        for check in checks {
            if matches!(check.kind, CheckKind::Structure | CheckKind::SplotDecode) {
                assert_eq!(check.outcome, CheckOutcome::Passed, "{kind:?} {depth:?}");
            }
        }
        let raw = common::decode(&path)?;
        // The crop leaves 31 × 23 luma samples and two 16 × 12 chroma planes.
        assert_eq!(
            raw.len(),
            clip.frames * (31 * 23 + 2 * 16 * 12) * sample_bytes
        );
        decoded.push(raw);
    }
    assert_eq!(decoded[0], decoded[1]);
    Ok(())
}
