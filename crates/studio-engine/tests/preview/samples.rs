// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Native sample and display checks against AVM output.

use std::fs;
use std::path::Path;

use studio_domain::job::ChromaLayout;
use studio_domain::preview::{
    GrainPolicy, PictureBox, PictureColor, PictureFailure, PictureFrame, PictureHeader,
    PictureOptions, PictureSurface, PictureTarget, PictureView, SamplePlane, SamplePoint,
    SampleRect,
};
use studio_domain::protocol::{Command, Event};

use super::{BIG, Fallible, IVF_10, Preview, TestResult, target};

const ODD_422: &[u8] =
    include_bytes!("fixtures/syn-3frame-padded-filters-422-134x69-10bit-q150.ivf");

fn fixture(dir: &Path, document: u64, name: &str, bytes: &[u8]) -> Fallible<PictureTarget> {
    let path = dir.join(name);
    fs::write(&path, bytes)?;
    target(&path, document)
}

struct Viewer {
    worker: Preview,
    generation: u64,
}

impl Viewer {
    fn open() -> Fallible<Self> {
        Ok(Self {
            worker: Preview::spawn()?,
            generation: 0,
        })
    }

    fn ask(&mut self, command: impl FnOnce(u64) -> Command) -> Fallible<(Event, Option<Vec<u8>>)> {
        self.generation += 1;
        self.worker
            .send(self.generation, command(self.generation))?;
        let (answer, pixels) = self.worker.picture_answer(self.generation)?;
        assert_eq!(answer.request_id, self.generation);
        Ok((answer.body, pixels))
    }

    fn sample(
        &mut self,
        target: &PictureTarget,
        ordinal: u64,
        grain: GrainPolicy,
        point: SamplePoint,
    ) -> Fallible<Result<u16, PictureFailure>> {
        let (event, pixels) = self.ask(|generation| Command::PictureSample {
            generation,
            target: target.clone(),
            ordinal,
            grain,
            point,
        })?;
        assert!(pixels.is_none());
        match event {
            Event::PictureSample {
                generation,
                ordinal: answer_ordinal,
                grain: answer_grain,
                point: answer_point,
                value,
            } => {
                assert_eq!(
                    (generation, answer_ordinal, answer_grain, answer_point),
                    (self.generation, ordinal, grain, point)
                );
                Ok(Ok(value))
            }
            Event::PictureUnavailable {
                generation, code, ..
            } => {
                assert_eq!(generation, self.generation);
                Ok(Err(code))
            }
            other => Err(format!("unexpected sample answer {other:?}").into()),
        }
    }

    fn picture(
        &mut self,
        target: &PictureTarget,
        ordinal: u64,
        surface: PictureSurface,
        grain: GrainPolicy,
        view: PictureView,
        size: PictureBox,
    ) -> Fallible<Result<(PictureHeader, Vec<u8>), PictureFailure>> {
        let options = PictureOptions { surface, grain };
        let (event, pixels) = self.ask(|generation| Command::Picture {
            generation,
            target: target.clone(),
            frame: PictureFrame::Index { index: ordinal },
            view,
            options,
            size,
        })?;
        match event {
            Event::Picture { picture } => {
                assert_eq!(
                    (picture.generation, picture.frame, picture.options),
                    (self.generation, ordinal, options)
                );
                assert_eq!(
                    picture.output.ok_or("missing output identity")?.ordinal,
                    ordinal
                );
                Ok(Ok((picture, pixels.ok_or("missing pixels")?)))
            }
            Event::PictureUnavailable {
                generation, code, ..
            } => {
                assert_eq!(generation, self.generation);
                assert!(pixels.is_none());
                Ok(Err(code))
            }
            other => Err(format!("unexpected picture answer {other:?}").into()),
        }
    }
}

const fn point(plane: SamplePlane, x: u32, y: u32) -> SamplePoint {
    SamplePoint { plane, x, y }
}

const fn rect(x: u32, y: u32, width: u32, height: u32) -> SampleRect {
    SampleRect {
        x,
        y,
        width,
        height,
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn visible_plane_edges_match_avm_at_both_depths() -> TestResult {
    use ChromaLayout::{Monochrome, Yuv420, Yuv422, Yuv444};
    use SamplePlane::{U, V, Y};

    let dir = tempfile::tempdir()?;
    let mono = fixture(
        dir.path(),
        1,
        "mono.ivf",
        include_bytes!("fixtures/syn-luma-palette2-row-mono-16x16-q0.ivf"),
    )?;
    let yuv420 = IVF_10.write(dir.path(), 2)?;
    let yuv422 = fixture(dir.path(), 3, "422.ivf", ODD_422)?;
    let yuv444 = fixture(
        dir.path(),
        4,
        "444.ivf",
        include_bytes!("fixtures/syn-mrl-padded-444-67x67-10bit-q120.ivf"),
    )?;
    let cases = [
        (
            &mono,
            0,
            8,
            Monochrome,
            (16, 16),
            None,
            &[
                (point(Y, 0, 0), 32),
                (point(Y, 3, 5), 224),
                (point(Y, 15, 15), 224),
            ][..],
        ),
        (
            &yuv420,
            1,
            10,
            Yuv420,
            (64, 64),
            Some((32, 32)),
            &[
                (point(Y, 63, 63), 503),
                (point(U, 3, 5), 611),
                (point(U, 31, 31), 585),
                (point(V, 31, 31), 303),
            ][..],
        ),
        (
            &yuv422,
            1,
            10,
            Yuv422,
            (134, 69),
            Some((67, 69)),
            &[
                (point(Y, 133, 68), 299),
                (point(U, 3, 5), 264),
                (point(U, 66, 68), 493),
                (point(V, 66, 68), 554),
            ][..],
        ),
        (
            &yuv444,
            0,
            10,
            Yuv444,
            (67, 67),
            Some((67, 67)),
            &[
                (point(Y, 66, 66), 326),
                (point(U, 3, 5), 545),
                (point(U, 66, 66), 543),
                (point(V, 66, 66), 484),
            ][..],
        ),
    ];
    let mut viewer = Viewer::open()?;
    for (target, ordinal, depth, chroma, (width, height), uv, samples) in cases {
        let (header, _) = viewer
            .picture(
                target,
                ordinal,
                PictureSurface::Y,
                GrainPolicy::Skip,
                PictureView::Fit,
                BIG,
            )?
            .map_err(|code| format!("no {chroma:?} luma surface: {code:?}"))?;
        let info = header.document_info.ok_or("missing plane geometry")?;
        assert_eq!((info.bit_depth, info.chroma), (depth, chroma));
        assert_eq!(info.y, rect(0, 0, width, height));
        assert_eq!(info.u, uv.map(|(w, h)| rect(0, 0, w, h)));
        assert_eq!(info.v, info.u);
        for &(point, expected) in samples {
            assert_eq!(
                viewer.sample(target, ordinal, GrainPolicy::Skip, point)?,
                Ok(expected),
                "{chroma:?} {point:?}"
            );
        }
    }
    assert_eq!(
        viewer.sample(&mono, 0, GrainPolicy::Skip, point(U, 0, 0))?,
        Err(PictureFailure::Unsupported)
    );
    assert_eq!(
        viewer.sample(&yuv420, 1, GrainPolicy::Skip, point(V, 32, 31))?,
        Err(PictureFailure::OutOfRange)
    );
    assert_eq!(
        viewer.sample(&yuv422, 1, GrainPolicy::Skip, point(U, 66, 69))?,
        Err(PictureFailure::OutOfRange)
    );
    viewer.worker.finish()
}

#[test]
fn sequence_crop_and_integer_zoom_keep_plane_coordinates() -> TestResult {
    let dir = tempfile::tempdir()?;
    let crop = fixture(
        dir.path(),
        1,
        "crop.obu",
        include_bytes!("fixtures/syn-2seq-crop-intra-64x64.obu"),
    )?;
    let mut viewer = Viewer::open()?;
    for (ordinal, visible, origin) in [(0, 32, 0), (1, 30, 2)] {
        let (header, pixels) = viewer
            .picture(
                &crop,
                ordinal,
                PictureSurface::U,
                GrainPolicy::Skip,
                PictureView::Zoom {
                    x: 60,
                    y: 60,
                    factor: 2,
                },
                PictureBox {
                    width: 8,
                    height: 6,
                },
            )?
            .map_err(|code| format!("no cropped surface: {code:?}"))?;
        let info = header.document_info.ok_or("missing plane geometry")?;
        assert_eq!((info.coded_width, info.coded_height), (64, 64));
        assert_eq!(info.y, rect(origin, origin, visible * 2, visible * 2));
        assert_eq!(info.u, Some(rect(origin / 2, origin / 2, visible, visible)));
        assert_eq!(info.v, info.u);
        assert_eq!((header.width, header.height), (4, 3));
        assert_eq!(
            header.view,
            PictureView::Zoom {
                x: visible - 4,
                y: visible - 3,
                factor: 2
            }
        );
        assert_eq!(pixels, [128; 4 * 3 * 3]);
        assert_eq!(
            viewer.sample(
                &crop,
                ordinal,
                GrainPolicy::Skip,
                point(SamplePlane::U, visible - 1, visible - 1)
            )?,
            Ok(128)
        );
    }
    let yuv422 = fixture(dir.path(), 2, "edge.ivf", ODD_422)?;
    let expected: Vec<u8> = [86, 117, 112, 94, 121, 123]
        .into_iter()
        .flat_map(|gray| [gray; 3])
        .collect();
    for (view, size) in [
        (
            PictureView::Native { x: 64, y: 67 },
            PictureBox {
                width: 3,
                height: 2,
            },
        ),
        (
            PictureView::Zoom {
                x: 64,
                y: 67,
                factor: 2,
            },
            PictureBox {
                width: 6,
                height: 4,
            },
        ),
    ] {
        let (header, pixels) = viewer
            .picture(&yuv422, 1, PictureSurface::U, GrainPolicy::Skip, view, size)?
            .map_err(|code| format!("no edge surface: {code:?}"))?;
        assert_eq!((header.width, header.height, header.view), (3, 2, view));
        assert_eq!(pixels, expected);
        assert_eq!(
            viewer.sample(&yuv422, 1, GrainPolicy::Skip, point(SamplePlane::U, 66, 68))?,
            Ok(493)
        );
    }
    viewer.worker.finish()
}

#[test]
fn each_grain_policy_uses_its_own_samples_and_display_pixels() -> TestResult {
    let dir = tempfile::tempdir()?;
    let grain = fixture(
        dir.path(),
        1,
        "grain.ivf",
        include_bytes!("fixtures/syn-3frame-filmgrain-inter-64x64-10bit.ivf"),
    )?;
    let mut viewer = Viewer::open()?;
    let mut output = None;
    for (policy, value, gray) in [
        (GrainPolicy::Apply, 110, 27),
        (GrainPolicy::Skip, 120, 30),
        (GrainPolicy::Apply, 110, 27),
    ] {
        let (header, pixels) = viewer
            .picture(
                &grain,
                2,
                PictureSurface::Y,
                policy,
                PictureView::Native { x: 3, y: 5 },
                PictureBox {
                    width: 1,
                    height: 1,
                },
            )?
            .map_err(|code| format!("no grain surface: {code:?}"))?;
        assert!(
            header
                .document_info
                .ok_or("missing grain metadata")?
                .film_grain
        );
        assert_eq!(header.output, *output.get_or_insert(header.output));
        assert_eq!(pixels, [gray; 3]);
        assert_eq!(
            viewer.sample(&grain, 2, policy, point(SamplePlane::Y, 3, 5))?,
            Ok(value)
        );
    }
    viewer.worker.finish()
}

#[test]
fn pq_color_refuses_rgb_and_preserves_native_diagnostics() -> TestResult {
    let dir = tempfile::tempdir()?;
    let pq = fixture(
        dir.path(),
        1,
        "pq.ivf",
        include_bytes!("fixtures/syn-pq-420-10bit-64x64.ivf"),
    )?;
    let mut viewer = Viewer::open()?;
    let box_size = PictureBox {
        width: 1,
        height: 1,
    };
    let view = PictureView::Native { x: 3, y: 5 };
    for (surface, plane, value, gray) in [
        (PictureSurface::Y, SamplePlane::Y, 433, 108),
        (PictureSurface::U, SamplePlane::U, 520, 130),
        (PictureSurface::V, SamplePlane::V, 620, 155),
        (PictureSurface::Y, SamplePlane::Y, 433, 108),
    ] {
        let (header, pixels) = viewer
            .picture(&pq, 0, surface, GrainPolicy::Skip, view, box_size)?
            .map_err(|code| format!("no diagnostic surface: {code:?}"))?;
        let info = header.document_info.ok_or("missing color metadata")?;
        assert_eq!(
            info.color,
            Some(PictureColor {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: false
            })
        );
        assert_eq!(pixels, [gray; 3]);
        assert_eq!(
            viewer.sample(&pq, 0, GrainPolicy::Skip, point(plane, 3, 5))?,
            Ok(value)
        );
        assert_eq!(
            viewer.picture(
                &pq,
                0,
                PictureSurface::Rgb,
                GrainPolicy::Skip,
                view,
                box_size
            )?,
            Err(PictureFailure::Unsupported)
        );
    }
    viewer.worker.finish()
}
