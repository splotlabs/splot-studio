// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Export the selected native decoder outputs through the Jobs publication service.

mod artifact;
mod input;
mod samples;
mod timing;
mod writers;

use studio_domain::export::{ExportProgress, ExportRange, ExportRequest, ExportStage};
use studio_domain::job::FailureCode;

use self::input::Source;
use self::samples::unsupported;
use self::timing::{Evidence, Timing};
use self::writers::Writers;
use crate::job::Stop;
use crate::output::ExportControl;
use crate::stream::MAX_DOCUMENT_UNITS;

/// Prepares, writes, and verifies a batch before claiming publication.
pub(crate) fn run(request: &ExportRequest, control: &dyn ExportControl) -> Result<u64, Stop> {
    check_cancelled(control)?;
    request
        .validate()
        .map_err(|e| Stop::failed(FailureCode::InvalidSettings, e.to_string()))?;
    let (first, last) = match request.range {
        ExportRange::All => (0, None),
        ExportRange::Inclusive { first, last } => {
            if last >= MAX_DOCUMENT_UNITS as u64 {
                return Err(Stop::failed(
                    FailureCode::TooLarge,
                    format!("Export can decode at most {MAX_DOCUMENT_UNITS} output pictures."),
                ));
            }
            (first, Some(last))
        }
    };

    control.stage(ExportStage::Checking);
    let mut source = Source::open(&request.source)?;
    let mut writers = Writers::prepare(request, &source.id, control)?;
    control.await_prepared()?;
    control.stage(ExportStage::Decoding);
    let mut progress = ExportProgress::default();
    let mut timing = Timing::default();
    let mut selected_count = 0;
    let decoded = source.decode(request, control, |output| {
        let ordinal = output.ordinal;
        let selected = ordinal >= first
            && request
                .layer
                .is_none_or(|layer| layer == samples::layer(&output.picture));
        let evidence = Evidence::of(&output);
        timing.observe(output.unit.offset, evidence, selected);
        if selected {
            samples::check_format(&output.picture, request.expected)?;
            writers.push(
                output,
                evidence,
                request,
                timing.status(request.frame_rate),
                control,
            )?;
            selected_count += 1;
        }
        progress.decoded = ordinal + 1;
        progress.exported = writers.frames_written();
        progress.bytes_written = writers.bytes_written();
        control.progress(progress);
        Ok(last == Some(ordinal))
    })?;
    if last.is_some_and(|last| last >= decoded) {
        return Err(unsupported(
            "The source ends before the last requested output.",
        ));
    }
    if selected_count == 0 {
        return Err(unsupported("No decoded outputs match the selection."));
    }

    let verified = writers.finish(timing.status(request.frame_rate), control)?;
    progress.exported = selected_count;
    progress.bytes_written = verified.iter().map(|artifact| artifact.bytes).sum();
    control.progress(progress);
    source.check()?;
    check_cancelled(control)?;
    control.claim_publish(progress.exported)?;
    for artifact in verified {
        artifact.publish(control)?;
    }
    Ok(progress.exported)
}

fn check_cancelled(control: &dyn ExportControl) -> Result<(), Stop> {
    if control.cancelled() {
        Err(Stop::Cancelled)
    } else {
        Ok(())
    }
}
