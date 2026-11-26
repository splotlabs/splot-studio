// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Export options captured before choosing a destination.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use slint::ComponentHandle;
use studio_domain::export::{
    ExportFormat, ExportLayer, ExportRange, ExportRequest, ExportSampleFormat,
};
use studio_domain::job::{ChromaLayout, Rational, WirePath};
use studio_domain::layers::LayerDecodeSelection;
use studio_domain::preview::GrainPolicy;

use crate::export_jobs::{ExportFrameSource, ExportSource};
use crate::ui::{AppWindow, DecodeExport, ExportForm, ExportPurpose, ExportSourceRow};

/// The sources and the displayed picture, as they were when the form opened.
pub struct Draft {
    pub(crate) ticket: std::rc::Rc<()>,
    purpose: ExportPurpose,
    grain: GrainPolicy,
    frame: Option<SelectedFrame>,
    sources: Vec<Source>,
}

#[derive(Clone, Copy)]
struct SelectedFrame {
    ordinal: u64,
    layer: ExportLayer,
    selection: LayerDecodeSelection,
}

struct Source {
    snapshot: ExportSource,
    path: PathBuf,
    selected: bool,
    depth: i32,
    chroma: i32,
}

/// A complete set of choices that only needs a file or folder path.
pub struct Plan {
    purpose: ExportPurpose,
    format: Format,
    range: ExportRange,
    layer: Option<ExportLayer>,
    grain: GrainPolicy,
    selection: LayerDecodeSelection,
    frame_rate: Option<Rational>,
    sources: Vec<PlannedSource>,
}

struct PlannedSource {
    source: studio_domain::export::ExportSource,
    expected: ExportSampleFormat,
    path: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Y4m,
    Raw,
    Hashes,
}

const DEPTHS: [u8; 2] = [8, 10];
const CHROMA: [ChromaLayout; 4] = [
    ChromaLayout::Monochrome,
    ChromaLayout::Yuv420,
    ChromaLayout::Yuv422,
    ChromaLayout::Yuv444,
];

impl Draft {
    /// Captures the available sources without reading files on the UI thread.
    ///
    /// # Errors
    /// Returns an error if the selection has no eligible source.
    pub fn new(
        purpose: ExportPurpose,
        sources: Vec<ExportSource>,
        current: Option<u64>,
        frame_source: Option<&ExportFrameSource>,
    ) -> Result<Self, String> {
        let grain = frame_source.map_or(GrainPolicy::Apply, |frame| frame.grain);
        let frame = if purpose == ExportPurpose::Frame {
            Some(
                frame_source
                    .map(|frame| SelectedFrame {
                        ordinal: frame.ordinal,
                        layer: frame.layer,
                        selection: frame.selection,
                    })
                    .ok_or("Pause on an output picture to export it.")?,
            )
        } else {
            None
        };

        let mut sources = if frame.is_some() {
            frame_source.map_or_else(Vec::new, |frame| vec![frame.source.clone()])
        } else {
            sources
        }
        .into_iter()
        .filter_map(|snapshot| {
            let path = snapshot.source.path.to_path_buf().ok()?;
            let format = frame_source
                .filter(|frame| frame.source.document == snapshot.document)
                .map(|frame| frame.expected);
            let selected = frame.is_some() || Some(snapshot.document) == current;
            Some(Source {
                snapshot,
                path,
                selected,
                depth: format.map_or(0, |f| index(&DEPTHS, f.bit_depth)),
                chroma: format.map_or(0, |f| index(&CHROMA, f.chroma)),
            })
        })
        .collect::<Vec<_>>();
        if let Some(first) = sources.first_mut() {
            if current.is_none() {
                first.selected = true;
            }
        } else {
            return Err("Read a complete AV2 stream in Analyze before exporting.".into());
        }

        Ok(Self {
            ticket: std::rc::Rc::new(()),
            purpose,
            grain,
            frame,
            sources,
        })
    }

    pub(crate) fn select_source(&mut self, index: i32, selected: bool) {
        if self.frame.is_none()
            && let Some(source) = entry(&mut self.sources, index)
        {
            source.selected = selected;
        }
    }

    pub(crate) fn choose_depth(&mut self, index: i32, depth: i32) {
        if self.frame.is_none()
            && let Some(source) = entry(&mut self.sources, index)
        {
            source.depth = depth;
        }
    }

    pub(crate) fn choose_chroma(&mut self, index: i32, chroma: i32) {
        if self.frame.is_none()
            && let Some(source) = entry(&mut self.sources, index)
        {
            source.chroma = chroma;
        }
    }

    /// Sets the form's frozen selection, as the app does when it opens the dialog.
    pub fn render(&self, window: &AppWindow) {
        let export = window.global::<DecodeExport>();
        export.set_purpose(self.purpose);
        export.set_initial_grain(self.grain == GrainPolicy::Apply);
        export.set_frame_details(self.frame_details().into());
        self.render_sources(&export);
    }

    pub(crate) fn render_sources(&self, export: &DecodeExport<'_>) {
        let rows = self
            .sources
            .iter()
            .map(|source| ExportSourceRow {
                name: crate::present::file_name(&source.path).into(),
                path: source.path.display().to_string().into(),
                selected: source.selected,
                depth: source.depth,
                chroma: source.chroma,
            })
            .collect();
        if let Some(model) = crate::render::refill(&export.get_sources(), rows) {
            export.set_sources(model);
        }
        let count = self.sources.iter().filter(|source| source.selected).count();
        export.set_selected_count(i32::try_from(count).unwrap_or(i32::MAX));
    }

    fn frame_details(&self) -> String {
        let Some(frame) = self.frame else {
            return String::new();
        };
        let Some(source) = self.sources.first() else {
            return String::new();
        };
        let Some(format) = source.sample_format() else {
            return String::new();
        };
        let ExportLayer {
            extended,
            embedded,
            temporal,
        } = frame.layer;
        let grain = match self.grain {
            GrainPolicy::Apply => "Apply film grain when signaled.",
            GrainPolicy::Skip => "Skip film grain.",
        };
        format!(
            "{} · Output {} · Extended {extended} · Embedded {embedded} · Temporal {temporal}\n{}-bit · {}. {grain}",
            crate::present::file_name(&source.path),
            frame.ordinal.saturating_add(1),
            format.bit_depth,
            crate::present::chroma(format.chroma),
        )
    }

    /// Captures strong domain values from the form before opening a native file dialog.
    ///
    /// # Errors
    /// Returns the field or source that needs a valid choice.
    pub fn plan(&self, form: &ExportForm) -> Result<Plan, String> {
        let format = if self.purpose == ExportPurpose::Hashes {
            Format::Hashes
        } else if form.raw {
            Format::Raw
        } else {
            Format::Y4m
        };
        let (range, layer, grain) = match self.frame {
            Some(frame) => (
                ExportRange::Inclusive {
                    first: frame.ordinal,
                    last: frame.ordinal,
                },
                Some(frame.layer),
                self.grain,
            ),
            None => (
                output_range(form)?,
                output_layer(form)?,
                if format == Format::Hashes || !form.apply_grain {
                    GrainPolicy::Skip
                } else {
                    GrainPolicy::Apply
                },
            ),
        };
        let frame_rate = if form.use_rate && format != Format::Hashes {
            Some(Rational {
                num: positive(&form.numerator).ok_or("Enter a positive frame-rate numerator.")?,
                den: positive(&form.denominator)
                    .ok_or("Enter a positive frame-rate denominator.")?,
            })
        } else {
            None
        };
        let sources = self
            .sources
            .iter()
            .filter(|source| source.selected)
            .map(|source| {
                let expected = source.sample_format().ok_or_else(|| {
                    format!(
                        "Select the decoded bit depth and chroma layout for {}.",
                        crate::present::file_name(&source.path)
                    )
                })?;
                Ok(PlannedSource {
                    source: source.snapshot.source.clone(),
                    expected,
                    path: source.path.clone(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        if sources.is_empty() {
            return Err("Select at least one source to export.".into());
        }
        Ok(Plan {
            purpose: self.purpose,
            format,
            range,
            layer,
            grain,
            selection: self
                .frame
                .map_or(LayerDecodeSelection::Default, |frame| frame.selection),
            frame_rate,
            sources,
        })
    }
}

impl Source {
    fn sample_format(&self) -> Option<ExportSampleFormat> {
        Some(ExportSampleFormat {
            bit_depth: *selected(&DEPTHS, self.depth)?,
            chroma: *selected(&CHROMA, self.chroma)?,
        })
    }
}

impl Plan {
    /// More than one source requires a folder rather than a file path.
    #[must_use]
    pub fn is_batch(&self) -> bool {
        self.sources.len() > 1
    }

    pub(crate) const fn extension(&self) -> &'static str {
        match self.format {
            Format::Y4m => "y4m",
            Format::Raw => "yuv",
            Format::Hashes => "json",
        }
    }

    pub(crate) const fn format_label(&self) -> &'static str {
        match self.format {
            Format::Y4m => "Y4M video",
            Format::Raw => "Raw samples",
            Format::Hashes => "Frame hashes",
        }
    }

    pub(crate) fn suggested_file(&self) -> Option<(&Path, OsString)> {
        let source = self.sources.first()?;
        Some((source.path.parent()?, self.name(source, 1)))
    }

    fn name(&self, source: &PlannedSource, duplicate: u32) -> OsString {
        let mut name = source
            .path
            .file_stem()
            .unwrap_or(OsStr::new("stream"))
            .to_os_string();
        match (self.purpose, self.range) {
            (ExportPurpose::Frame, ExportRange::Inclusive { first, .. }) => {
                name.push(format!(" - frame {}", first.saturating_add(1)));
            }
            (ExportPurpose::Hashes, _) => name.push(" - frame hashes"),
            _ => name.push(" - decoded"),
        }
        if duplicate > 1 {
            name.push(format!(" ({duplicate})"));
        }
        name.push(".");
        name.push(self.extension());
        name
    }

    /// Uses the chosen path to create immutable job requests. A batch uses unique names inside its folder.
    #[must_use]
    pub fn requests(self, chosen: &Path) -> Vec<ExportRequest> {
        let mut used = HashSet::new();
        let batch = self.is_batch();
        self.sources
            .iter()
            .map(|source| {
                let destination = if batch {
                    let mut duplicate = 1;
                    loop {
                        let name = self.name(source, duplicate);
                        if used.insert(name.to_string_lossy().to_lowercase()) {
                            break chosen.join(name);
                        }
                        duplicate += 1;
                    }
                } else {
                    chosen.to_path_buf()
                };
                let path = WirePath::from_path(&destination);
                let format = match self.format {
                    Format::Y4m => ExportFormat::Y4m { path },
                    Format::Hashes => ExportFormat::Hashes { path },
                    Format::Raw => {
                        let mut layout = destination.into_os_string();
                        layout.push(".layout.json");
                        ExportFormat::Raw {
                            samples: path,
                            layout: WirePath::from_path(Path::new(&layout)),
                        }
                    }
                };
                ExportRequest {
                    source: source.source.clone(),
                    range: self.range,
                    layer: self.layer,
                    grain: self.grain,
                    selection: self.selection,
                    frame_rate: self.frame_rate,
                    expected: source.expected,
                    format,
                }
            })
            .collect()
    }
}

fn positive(text: &str) -> Option<u32> {
    text.trim().parse().ok().filter(|&value| value > 0)
}

fn output_range(form: &ExportForm) -> Result<ExportRange, String> {
    if !form.use_range {
        return Ok(ExportRange::All);
    }
    let ordinal = |text: &str| {
        text.trim()
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_sub(1))
    };
    let first = ordinal(&form.first).ok_or("Enter a positive first output number.")?;
    let last = ordinal(&form.last).ok_or("Enter a positive last output number.")?;
    if first > last {
        return Err("Set the last output at or after the first output.".into());
    }
    Ok(ExportRange::Inclusive { first, last })
}

fn output_layer(form: &ExportForm) -> Result<Option<ExportLayer>, String> {
    if !form.use_layer {
        return Ok(None);
    }
    let layer = |text: &str, max| text.trim().parse::<u8>().ok().filter(|&id| id <= max);
    Ok(Some(ExportLayer {
        extended: layer(&form.extended, 30).ok_or("Set the extended layer ID from 0 to 30.")?,
        embedded: layer(&form.embedded, 7).ok_or("Set the embedded layer ID from 0 to 7.")?,
        temporal: layer(&form.temporal, 3).ok_or("Set the temporal layer ID from 0 to 3.")?,
    }))
}

fn index<T: Copy + PartialEq>(values: &[T], value: T) -> i32 {
    values
        .iter()
        .position(|choice| *choice == value)
        .and_then(|index| i32::try_from(index + 1).ok())
        .unwrap_or(0)
}

fn selected<T>(values: &[T], index: i32) -> Option<&T> {
    values.get(usize::try_from(index).ok()?.checked_sub(1)?)
}

fn entry(sources: &mut [Source], index: i32) -> Option<&mut Source> {
    sources.get_mut(usize::try_from(index).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_domain::analysis::{Framing, SourceDigest};
    use studio_domain::job::SourceFingerprint;

    fn source(path: &str, document: u64) -> ExportSource {
        ExportSource {
            document,
            source: studio_domain::export::ExportSource {
                path: WirePath::from_path(&crate::testing::abs(path)),
                fingerprint: SourceFingerprint {
                    len: 7,
                    modified: None,
                },
                digest: SourceDigest {
                    bytes: 7,
                    sha256: "a".repeat(64),
                },
                framing: Framing::AnnexB,
            },
            ivf_header: None,
        }
    }

    fn frame(source: ExportSource) -> ExportFrameSource {
        ExportFrameSource {
            source,
            generation: 13,
            ordinal: 5,
            layer: ExportLayer {
                extended: 2,
                embedded: 1,
                temporal: 0,
            },
            expected: ExportSampleFormat {
                bit_depth: 10,
                chroma: ChromaLayout::Yuv422,
            },
            grain: GrainPolicy::Skip,
            selection: LayerDecodeSelection::OperatingPoint {
                scope: studio_domain::layers::LayerScope::Global,
                set: 1,
                point: 0,
            },
        }
    }

    #[test]
    fn frame_export_freezes_the_displayed_ordinal_layer_format_and_grain() -> Result<(), String> {
        let source = source("/one/test.obu", 8);
        let displayed = frame(source.clone());
        let draft = Draft::new(ExportPurpose::Frame, vec![], Some(8), Some(&displayed))?;
        let form = ExportForm {
            raw: true,
            use_range: true,
            first: "bad".into(),
            use_layer: true,
            extended: "bad".into(),
            apply_grain: true,
            ..ExportForm::default()
        };
        let chosen = crate::testing::abs("/out/frame.yuv");
        let requests = draft.plan(&form)?.requests(&chosen);
        let [request] = requests.as_slice() else {
            unreachable!("{requests:?}")
        };
        assert_eq!(request.source, source.source);
        assert_eq!(request.range, ExportRange::Inclusive { first: 5, last: 5 });
        assert_eq!(request.layer, Some(displayed.layer));
        assert_eq!(request.expected, displayed.expected);
        assert_eq!(request.grain, GrainPolicy::Skip);
        assert_eq!(request.selection, displayed.selection);
        assert_eq!(
            request.format,
            ExportFormat::Raw {
                samples: WirePath::from_path(&chosen),
                layout: WirePath::from_path(&crate::testing::abs("/out/frame.yuv.layout.json")),
            }
        );
        Ok(())
    }

    #[test]
    fn hash_batch_keeps_per_source_formats_and_disambiguates_names() -> Result<(), String> {
        let a = source("/one/test.obu", 8);
        let b = source("/two/test.ivf", 9);
        let mut draft = Draft::new(
            ExportPurpose::Hashes,
            vec![a.clone(), b.clone()],
            Some(8),
            Some(&frame(a)),
        )?;
        draft.select_source(1, true);
        let form = ExportForm {
            use_range: true,
            first: "2".into(),
            last: "3".into(),
            apply_grain: true,
            use_rate: true,
            ..ExportForm::default()
        };
        assert!(draft.plan(&form).is_err());
        draft.choose_depth(1, 1);
        draft.choose_chroma(1, 1);
        let plan = draft.plan(&form)?;
        assert!(plan.is_batch());
        let requests = plan.requests(&crate::testing::abs("/out"));
        let [a, b] = requests.as_slice() else {
            unreachable!("{requests:?}")
        };
        for request in &requests {
            assert_eq!(request.range, ExportRange::Inclusive { first: 1, last: 2 });
            assert_eq!(request.grain, GrainPolicy::Skip);
            assert_eq!(request.frame_rate, None);
            assert!(request.validate().is_ok());
        }
        assert_ne!(a.format, b.format);
        assert_eq!(
            b.expected,
            ExportSampleFormat {
                bit_depth: 8,
                chroma: ChromaLayout::Monochrome
            }
        );
        assert_eq!(
            b.format,
            ExportFormat::Hashes {
                path: WirePath::from_path(&crate::testing::abs(
                    "/out/test - frame hashes (2).json"
                )),
            }
        );
        Ok(())
    }

    #[test]
    fn range_layer_and_timing_choices_are_checked_before_the_picker() -> Result<(), String> {
        let source = source("/test.obu", 8);
        let draft = Draft::new(
            ExportPurpose::Video,
            vec![source.clone()],
            Some(8),
            Some(&frame(source)),
        )?;
        let mut form = ExportForm {
            use_range: true,
            first: "1".into(),
            last: "12".into(),
            use_layer: true,
            extended: "2".into(),
            embedded: "1".into(),
            temporal: "4".into(),
            use_rate: true,
            numerator: "24000".into(),
            denominator: "0".into(),
            ..ExportForm::default()
        };
        assert_eq!(
            draft.plan(&form).err().as_deref(),
            Some("Set the temporal layer ID from 0 to 3.")
        );
        form.temporal = "2".into();
        assert_eq!(
            draft.plan(&form).err().as_deref(),
            Some("Enter a positive frame-rate denominator.")
        );
        form.denominator = "1001".into();
        let requests = draft
            .plan(&form)?
            .requests(&crate::testing::abs("/chosen.y4m"));
        let [request] = requests.as_slice() else {
            unreachable!("{requests:?}")
        };
        assert_eq!(
            request.frame_rate,
            Some(Rational {
                num: 24000,
                den: 1001
            })
        );
        assert_eq!(request.range, ExportRange::Inclusive { first: 0, last: 11 });
        assert_eq!(
            request.layer,
            Some(ExportLayer {
                extended: 2,
                embedded: 1,
                temporal: 2
            })
        );
        Ok(())
    }
}
