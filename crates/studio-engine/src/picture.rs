// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! The one display transform: planar 8-bit or 10-bit limited-range YCbCr
//! (4:2:0, 4:2:2, 4:4:4 or monochrome) to a small 8-bit RGB presentation
//! copy. Source, result and live pictures all go through [`render`].
//!
//! Samples stay at their own precision until the last step: the area
//! filter averages code values as `f32`, the matrix runs on those, and only
//! the final value is rounded to 8 bits. Chroma is taken at its own
//! resolution and mapped to luma positions by plain subsampling (no siting
//! correction, nearest sample when chroma is coarser than the picture).
//! Values outside the nominal range are clipped. This is an SDR
//! approximation for comparing pictures, not colour management.
//!
//! The matrix is the one the source states. When it states none, the rule
//! of [`basis`] picks one and the picture says it was assumed.

use studio_domain::preview::{ColorBasis, ColorMatrix, PictureBox, PictureView, SamplePlane};

/// One plane's samples, row by row.
#[derive(Debug, Clone, Copy)]
pub enum Samples<'a> {
    /// One byte per sample (8-bit).
    Narrow(&'a [u8]),
    /// One `u16` per sample, value in the low bits (8-bit or 10-bit).
    Wide(&'a [u16]),
}

/// A plane: `width` x `height` samples, row `y` starting at
/// `offset + y * stride`.
#[derive(Debug, Clone, Copy)]
pub struct Plane<'a> {
    samples: Samples<'a>,
    offset: usize,
    stride: usize,
    width: usize,
    height: usize,
}

impl<'a> Plane<'a> {
    /// A plane over `samples`, or `None` when the geometry does not fit in
    /// them or is empty.
    #[must_use]
    pub fn new(
        samples: Samples<'a>,
        offset: usize,
        stride: usize,
        width: usize,
        height: usize,
    ) -> Option<Self> {
        let len = match samples {
            Samples::Narrow(s) => s.len(),
            Samples::Wide(s) => s.len(),
        };
        let last = (height.checked_sub(1)?)
            .checked_mul(stride)?
            .checked_add(offset)?
            .checked_add(width)?;
        (width > 0 && stride >= width && last <= len).then_some(Self {
            samples,
            offset,
            stride,
            width,
            height,
        })
    }

    /// A tightly packed plane.
    #[must_use]
    pub fn packed(samples: &'a [u16], width: usize, height: usize) -> Option<Self> {
        Self::new(Samples::Wide(samples), 0, width, width, height)
    }

    /// The visible plane size.
    #[must_use]
    pub const fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Reads one sample at a visible coordinate.
    #[must_use]
    pub fn sample(&self, x: u32, y: u32) -> Option<u16> {
        let (x, y) = (usize::try_from(x).ok()?, usize::try_from(y).ok()?);
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = self.offset + y * self.stride + x;
        match self.samples {
            Samples::Narrow(samples) => samples.get(index).copied().map(u16::from),
            Samples::Wide(samples) => samples.get(index).copied(),
        }
    }

    /// Adds `weight` times the samples `lo..hi` of row `y` to `acc[0..]`.
    fn add_row(&self, y: usize, lo: usize, hi: usize, weight: f32, acc: &mut [f32]) {
        let start = self.offset + y * self.stride;
        match self.samples {
            Samples::Narrow(s) => {
                if let Some(row) = s.get(start + lo..start + hi) {
                    for (a, &v) in acc.iter_mut().zip(row) {
                        *a += weight * f32::from(v);
                    }
                }
            }
            Samples::Wide(s) => {
                if let Some(row) = s.get(start + lo..start + hi) {
                    for (a, &v) in acc.iter_mut().zip(row) {
                        *a += weight * f32::from(v);
                    }
                }
            }
        }
    }
}

/// A frame to show: luma, and chroma unless monochrome.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    /// The luma plane; its size is the frame size.
    pub luma: Plane<'a>,
    /// The Cb and Cr planes, `None` for monochrome.
    pub chroma: Option<[Plane<'a>; 2]>,
    /// 8 or 10.
    pub bit_depth: u8,
}

impl<'a> Frame<'a> {
    /// The planes the source reader returns: packed `u16`, the chroma
    /// planes empty for monochrome. `None` when a plane's length does not
    /// match its size.
    #[must_use]
    pub fn packed(
        planes: &'a [Vec<u16>; 3],
        sizes: [(usize, usize); 3],
        bit_depth: u8,
    ) -> Option<Self> {
        let plane = |i: usize| {
            let (w, h) = sizes[i];
            (planes[i].len() == w * h)
                .then(|| Plane::packed(&planes[i], w, h))
                .flatten()
        };
        let chroma = if sizes[1] == (0, 0) {
            None
        } else {
            Some([plane(1)?, plane(2)?])
        };
        Some(Self {
            luma: plane(0)?,
            chroma,
            bit_depth,
        })
    }

    /// The selected visible plane, if it exists.
    #[must_use]
    pub fn plane(&self, plane: SamplePlane) -> Option<&Plane<'a>> {
        match plane {
            SamplePlane::Y => Some(&self.luma),
            SamplePlane::U => self.chroma.as_ref().map(|planes| &planes[0]),
            SamplePlane::V => self.chroma.as_ref().map(|planes| &planes[1]),
        }
    }

    /// The frame size in luma samples.
    #[must_use]
    pub const fn size(&self) -> (usize, usize) {
        (self.luma.width, self.luma.height)
    }
}

/// The matrix of a picture of a `width` x `height` frame whose source
/// states `stated` (`None`: unstated). The rule for an unstated matrix:
/// BT.709 above standard definition (wider than 1024 or taller than 576),
/// else BT.601, marked as assumed. Monochrome needs no matrix.
#[must_use]
pub fn basis(stated: Option<ColorMatrix>, monochrome: bool, width: u32, height: u32) -> ColorBasis {
    if monochrome {
        return ColorBasis {
            matrix: ColorMatrix::Monochrome,
            assumed: false,
        };
    }
    match stated {
        Some(matrix) => ColorBasis {
            matrix,
            assumed: false,
        },
        None => ColorBasis {
            matrix: if width > 1024 || height > 576 {
                ColorMatrix::Bt709
            } else {
                ColorMatrix::Bt601
            },
            assumed: true,
        },
    }
}

/// The matrix `FFmpeg` names `color_space`, when the transform implements
/// it. Any other stated matrix is treated as unstated (and so assumed).
#[must_use]
pub fn ffmpeg_matrix(color_space: &str) -> Option<ColorMatrix> {
    match color_space {
        "bt709" => Some(ColorMatrix::Bt709),
        "smpte170m" | "bt470bg" => Some(ColorMatrix::Bt601),
        "bt2020nc" => Some(ColorMatrix::Bt2020),
        _ => None,
    }
}

/// A presentation picture: `width * height * 3` bytes of RGB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// Rows top to bottom, R, G, B, no padding.
    pub pixels: Vec<u8>,
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// The view produced, with the origin used.
    pub view: PictureView,
}

/// The picture size and luma origin a `view` of a `width` x `height`
/// surface gets in `size`. Fit scales down and preserves aspect ratio.
/// Native and Zoom crop at the sample grid with the origin moved inside the surface.
#[must_use]
pub fn layout(
    width: usize,
    height: usize,
    view: PictureView,
    size: PictureBox,
) -> (usize, usize, PictureView) {
    let (bw, bh) = (usize::from(size.width), usize::from(size.height));
    match view {
        PictureView::Fit => {
            if width <= bw && height <= bh {
                (width, height, view)
            } else if width * bh >= height * bw {
                let h = ((height * bw * 2 + width) / (width * 2)).clamp(1, bh);
                (bw, h, view)
            } else {
                let w = ((width * bh * 2 + height) / (height * 2)).clamp(1, bw);
                (w, bh, view)
            }
        }
        PictureView::Native { x, y } | PictureView::Zoom { x, y, .. } => {
            let factor = match view {
                PictureView::Zoom { factor, .. } => usize::from(factor.max(1)),
                _ => 1,
            };
            let (w, h) = (
                width.min((bw / factor).max(1)),
                height.min((bh / factor).max(1)),
            );
            let x = usize::try_from(x).unwrap_or(usize::MAX).min(width - w);
            let y = usize::try_from(y).unwrap_or(usize::MAX).min(height - h);
            let (x, y) = (
                u32::try_from(x).unwrap_or(u32::MAX),
                u32::try_from(y).unwrap_or(u32::MAX),
            );
            let used = match view {
                PictureView::Zoom { factor, .. } => PictureView::Zoom { x, y, factor },
                _ => PictureView::Native { x, y },
            };
            (w, h, used)
        }
    }
}

/// Converts `frame` to RGB with `basis`, as `view` in at most `size`.
#[must_use]
pub fn render(
    frame: &Frame<'_>,
    basis: ColorBasis,
    view: PictureView,
    size: PictureBox,
) -> Rendered {
    let (width, height) = frame.size();
    let (ow, oh, used) = layout(width, height, view, size);
    let origin = crop_origin(used);
    let resample = |plane: &Plane<'_>| {
        // Chroma is subsampled by 2 exactly when it is narrower (shorter).
        let sub_x = if plane.width < width { 2 } else { 1 };
        let sub_y = if plane.height < height { 2 } else { 1 };
        let ax = axis(width, ow, sub_x, plane.width, origin.map(|o| o.0));
        let ay = axis(height, oh, sub_y, plane.height, origin.map(|o| o.1));
        resample(plane, &ax, &ay, ow)
    };
    let luma = resample(&frame.luma);
    let chroma = frame
        .chroma
        .as_ref()
        .map(|[cb, cr]| (resample(cb), resample(cr)));
    let mut pixels = Vec::with_capacity(ow * oh * 3);
    let scale = f32::from(1_u16 << frame.bit_depth.saturating_sub(8).min(8));
    let black = 16.0 * scale;
    let luma_range = 219.0 * scale;
    let mid = 128.0 * scale;
    let chroma_range = 224.0 * scale;
    let (kr, kb) = match basis.matrix {
        ColorMatrix::Bt601 => (0.299, 0.114),
        ColorMatrix::Bt709 => (0.2126, 0.0722),
        ColorMatrix::Bt2020 => (0.2627, 0.0593),
        ColorMatrix::Monochrome => (0.0, 0.0),
    };
    let kg = 1.0 - kr - kb;
    for (i, &y) in luma.iter().enumerate() {
        let y = (y - black) / luma_range;
        let rgb = match &chroma {
            Some((cb, cr)) if basis.matrix != ColorMatrix::Monochrome => {
                let cb = (cb[i] - mid) / chroma_range;
                let cr = (cr[i] - mid) / chroma_range;
                // G from Cb and Cr directly, so neutral chroma gives
                // exactly R = G = B = Y.
                let r = y + 2.0 * (1.0 - kr) * cr;
                let b = y + 2.0 * (1.0 - kb) * cb;
                let g = y - (2.0 * kb * (1.0 - kb) / kg) * cb - (2.0 * kr * (1.0 - kr) / kg) * cr;
                [r, g, b]
            }
            _ => [y, y, y],
        };
        pixels.extend(rgb.map(to_byte));
    }
    rendered(pixels, ow, oh, used)
}

/// Maps native code values to diagnostic grayscale over the bit-depth range.
///
/// Fit uses the same area filter as RGB. The integer views return an unscaled crop.
#[must_use]
pub fn render_plane(
    plane: &Plane<'_>,
    bit_depth: u8,
    view: PictureView,
    size: PictureBox,
) -> Rendered {
    let (width, height) = plane.size();
    let (ow, oh, used) = layout(width, height, view, size);
    let origin = crop_origin(used);
    let x = axis(width, ow, 1, width, origin.map(|xy| xy.0));
    let y = axis(height, oh, 1, height, origin.map(|xy| xy.1));
    let max = f32::from((1_u16 << bit_depth.clamp(1, 15)) - 1);
    let mut pixels = Vec::with_capacity(ow * oh * 3);
    for value in resample(plane, &x, &y, ow) {
        pixels.extend([to_byte(value / max); 3]);
    }
    rendered(pixels, ow, oh, used)
}

fn crop_origin(view: PictureView) -> Option<(usize, usize)> {
    match view {
        PictureView::Native { x, y } | PictureView::Zoom { x, y, .. } => {
            Some((x as usize, y as usize))
        }
        PictureView::Fit => None,
    }
}

fn rendered(pixels: Vec<u8>, width: usize, height: usize, view: PictureView) -> Rendered {
    Rendered {
        pixels,
        width: u16::try_from(width).unwrap_or(u16::MAX),
        height: u16::try_from(height).unwrap_or(u16::MAX),
        view,
    }
}

fn to_byte(value: f32) -> u8 {
    // In 0..=255 after the clamp, so the cast cannot truncate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let byte = (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    byte
}

/// For each output position, the plane samples it covers and their weights.
#[derive(Debug)]
struct Axis {
    /// `taps[starts[i]..starts[i + 1]]` belong to output `i`.
    starts: Vec<usize>,
    taps: Vec<(usize, f32)>,
}

impl Axis {
    fn taps(&self, i: usize) -> &[(usize, f32)] {
        self.taps
            .get(self.starts[i]..self.starts[i + 1])
            .unwrap_or_default()
    }
}

/// The taps along one axis: `out` outputs over `full` luma samples, on a
/// plane of `len` samples subsampled by `sub`. With `origin`, one luma
/// sample per output from there (the native view); otherwise each output
/// averages its share of the frame (an area filter).
fn axis(full: usize, out: usize, sub: usize, len: usize, origin: Option<usize>) -> Axis {
    let mut starts = Vec::with_capacity(out + 1);
    let mut taps = Vec::with_capacity(out * 2);
    let last = len.saturating_sub(1);
    for i in 0..out {
        starts.push(taps.len());
        if let Some(origin) = origin {
            taps.push((((origin + i) / sub).min(last), 1.0));
            continue;
        }
        // Luma span of output i, in plane samples. Sizes are far below
        // 2^52, so they are exact as `f64`.
        #[allow(clippy::cast_precision_loss)]
        let (a, b) = {
            let scale = full as f64 / out as f64 / sub as f64;
            (i as f64 * scale, (i + 1) as f64 * scale)
        };
        let total = b - a;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (first, end) = (
            a.floor() as usize,
            (b.ceil() as usize).max(a.floor() as usize + 1),
        );
        for s in first..end {
            #[allow(clippy::cast_precision_loss)]
            let covered = (b.min((s + 1) as f64) - a.max(s as f64)).max(0.0);
            if covered <= 0.0 {
                continue;
            }
            #[allow(clippy::cast_possible_truncation)]
            let weight = (covered / total) as f32;
            let s = s.min(last);
            let own = taps.len() > starts[i];
            match taps.last_mut() {
                // A span past the plane's end folds onto its last sample.
                Some((prev, w)) if own && *prev == s => *w += weight,
                _ => taps.push((s, weight)),
            }
        }
    }
    starts.push(taps.len());
    Axis { starts, taps }
}

/// The plane resampled to `ax` x `ay`, as code values.
fn resample(plane: &Plane<'_>, ax: &Axis, ay: &Axis, out_width: usize) -> Vec<f32> {
    let lo = ax.taps.iter().map(|t| t.0).min().unwrap_or(0);
    let hi = ax
        .taps
        .iter()
        .map(|t| t.0 + 1)
        .max()
        .unwrap_or(0)
        .min(plane.width);
    let mut acc = vec![0.0_f32; hi.saturating_sub(lo)];
    let rows = ay.starts.len().saturating_sub(1);
    let mut out = Vec::with_capacity(out_width * rows);
    for oy in 0..rows {
        acc.fill(0.0);
        for &(y, weight) in ay.taps(oy) {
            plane.add_row(y.min(plane.height - 1), lo, hi, weight, &mut acc);
        }
        for ox in 0..out_width {
            let value: f32 = ax
                .taps(ox)
                .iter()
                .map(|&(x, w)| w * acc.get(x - lo).copied().unwrap_or(0.0))
                .sum();
            out.push(value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOX: PictureBox = PictureBox {
        width: 1280,
        height: 720,
    };

    fn basis_of(matrix: ColorMatrix) -> ColorBasis {
        ColorBasis {
            matrix,
            assumed: false,
        }
    }

    /// One colour over a whole `w` x `h` 4:2:0 frame.
    fn flat(w: usize, h: usize, yuv: [u16; 3]) -> [Vec<u16>; 3] {
        let c = w.div_ceil(2) * h.div_ceil(2);
        [vec![yuv[0]; w * h], vec![yuv[1]; c], vec![yuv[2]; c]]
    }

    fn sizes420(w: usize, h: usize) -> [(usize, usize); 3] {
        let c = (w.div_ceil(2), h.div_ceil(2));
        [(w, h), c, c]
    }

    fn rgb_of(
        planes: &[Vec<u16>; 3],
        w: usize,
        h: usize,
        depth: u8,
        matrix: ColorMatrix,
    ) -> [u8; 3] {
        let frame = Frame::packed(planes, sizes420(w, h), depth);
        let Some(frame) = frame else {
            unreachable!("valid planes")
        };
        let out = render(&frame, basis_of(matrix), PictureView::Fit, BOX);
        [out.pixels[0], out.pixels[1], out.pixels[2]]
    }

    fn near(got: [u8; 3], want: [u8; 3], tolerance: u8) -> bool {
        got.iter()
            .zip(want)
            .all(|(&g, w)| g.abs_diff(w) <= tolerance)
    }

    /// Limited-range grey ramps at both depths: black to white, and every
    /// step against `(Y - black) / range` computed here independently.
    #[test]
    fn grey_ramps_map_to_full_rgb() {
        for depth in [8_u8, 10] {
            let shift = u32::from(depth - 8);
            let (black, white) = (16_u16 << shift, 235_u16 << shift);
            for y in black..=white {
                let got = rgb_of(
                    &flat(2, 2, [y, 128 << shift, 128 << shift]),
                    2,
                    2,
                    depth,
                    ColorMatrix::Bt709,
                );
                let want = (f64::from(y - black) * 255.0 / f64::from(white - black)).round();
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let want = want as u8;
                assert_eq!(got, [want; 3], "depth {depth}, Y {y}");
            }
            // Below black and above white clip.
            let low = rgb_of(
                &flat(2, 2, [0, 128 << shift, 128 << shift]),
                2,
                2,
                depth,
                ColorMatrix::Bt601,
            );
            let high = rgb_of(
                &flat(2, 2, [(1 << depth) - 1, 128 << shift, 128 << shift]),
                2,
                2,
                depth,
                ColorMatrix::Bt601,
            );
            assert_eq!((low, high), ([0; 3], [255; 3]));
        }
    }

    /// 100% primaries from the published BT.601 and BT.709 limited-range
    /// code values (ITU-R BT.601-7, BT.709-6 tables; 10-bit for BT.709).
    /// The published codes are themselves rounded, so one step of tolerance.
    #[test]
    fn published_primaries_come_back() {
        let cases: [(ColorMatrix, u8, [u16; 3], [u8; 3]); 10] = [
            (ColorMatrix::Bt709, 8, [63, 102, 240], [255, 0, 0]),
            (ColorMatrix::Bt709, 8, [173, 42, 26], [0, 255, 0]),
            (ColorMatrix::Bt709, 8, [32, 240, 118], [0, 0, 255]),
            (ColorMatrix::Bt709, 10, [250, 409, 960], [255, 0, 0]),
            (ColorMatrix::Bt709, 10, [691, 167, 105], [0, 255, 0]),
            (ColorMatrix::Bt709, 10, [127, 960, 471], [0, 0, 255]),
            (ColorMatrix::Bt601, 8, [81, 90, 240], [255, 0, 0]),
            (ColorMatrix::Bt601, 8, [145, 54, 34], [0, 255, 0]),
            (ColorMatrix::Bt601, 8, [41, 240, 110], [0, 0, 255]),
            (ColorMatrix::Bt709, 8, [235, 128, 128], [255, 255, 255]),
        ];
        for (matrix, depth, yuv, want) in cases {
            let got = rgb_of(&flat(4, 4, yuv), 4, 4, depth, matrix);
            assert!(
                near(got, want, 2),
                "{matrix:?} {depth}-bit {yuv:?}: {got:?}"
            );
        }
        // BT.2020 red (Kr 0.2627, Kb 0.0593), 10-bit, computed by hand:
        // Y = 64 + 876 * 0.2627, Cb = 512 - 448 * 0.2627 / 0.9407,
        // Cr = 512 + 448.
        let got = rgb_of(&flat(2, 2, [294, 387, 960]), 2, 2, 10, ColorMatrix::Bt2020);
        assert!(near(got, [255, 0, 0], 2), "{got:?}");
    }

    /// The same colour at 8 and 10 bits gives the same RGB.
    #[test]
    fn both_depths_agree() {
        for yuv in [[63_u16, 102, 240], [100, 150, 90], [200, 60, 180]] {
            let eight = rgb_of(&flat(2, 2, yuv), 2, 2, 8, ColorMatrix::Bt709);
            let ten = rgb_of(
                &flat(2, 2, yuv.map(|v| v << 2)),
                2,
                2,
                10,
                ColorMatrix::Bt709,
            );
            assert_eq!(eight, ten);
        }
    }

    #[test]
    fn monochrome_is_grey() {
        let planes = [vec![126_u16; 4], Vec::new(), Vec::new()];
        let frame = Frame::packed(&planes, [(2, 2), (0, 0), (0, 0)], 8);
        let Some(frame) = frame else { unreachable!() };
        let out = render(&frame, basis(None, true, 2, 2), PictureView::Fit, BOX);
        assert_eq!(out.pixels, [128; 12]);
    }

    #[test]
    fn samples_and_diagnostic_crop_use_the_visible_plane_grid() -> Result<(), &'static str> {
        // Three visible samples per row. Padding and crop offsets contain different values.
        let storage = [
            16_u16, 16, 16, 16, 16, 17, 513, 1023, 16, 16, 300, 700, 0, 16, 16,
        ];
        let plane = Plane::new(Samples::Wide(&storage), 5, 5, 3, 2).ok_or("Invalid test plane.")?;
        assert_eq!(plane.sample(0, 0), Some(17));
        assert_eq!(plane.sample(1, 0), Some(513));
        assert_eq!(plane.sample(2, 1), Some(0));
        assert_eq!(plane.sample(3, 0), None);
        assert_eq!(plane.sample(0, 2), None);
        let output = render_plane(
            &plane,
            10,
            PictureView::Zoom {
                x: u32::MAX,
                y: u32::MAX,
                factor: 3,
            },
            PictureBox {
                width: 8,
                height: 6,
            },
        );
        assert_eq!(
            output.view,
            PictureView::Zoom {
                x: 1,
                y: 0,
                factor: 3
            }
        );
        assert_eq!((output.width, output.height), (2, 2));
        assert_eq!(
            output.pixels,
            [128, 128, 128, 255, 255, 255, 174, 174, 174, 0, 0, 0],
        );
        Ok(())
    }

    /// The area filter averages each output's share; it never picks one
    /// sample (nearest) and never scales up.
    #[test]
    fn fit_averages_and_never_upscales() {
        // 4x2 luma: left half black, right half white; to 2x1.
        let y = vec![16, 16, 235, 235, 16, 16, 235, 235];
        let planes = [y, vec![128; 2], vec![128; 2]];
        let frame = Frame::packed(&planes, [(4, 2), (2, 1), (2, 1)], 8);
        let Some(frame) = frame else { unreachable!() };
        let small = PictureBox {
            width: 2,
            height: 2,
        };
        let out = render(
            &frame,
            basis_of(ColorMatrix::Bt709),
            PictureView::Fit,
            small,
        );
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(out.pixels, [0, 0, 0, 255, 255, 255]);
        // A 3:1 reduction of 0, 1, 2 steps over three samples averages them.
        let y: Vec<u16> = vec![16, 16 + 73, 16 + 146, 235, 235, 235];
        let planes = [y, vec![128; 3], vec![128; 3]];
        let frame = Frame::packed(&planes, [(6, 1), (3, 1), (3, 1)], 8);
        let Some(frame) = frame else { unreachable!() };
        let out = render(
            &frame,
            basis_of(ColorMatrix::Bt709),
            PictureView::Fit,
            PictureBox {
                width: 2,
                height: 1,
            },
        );
        assert_eq!(out.pixels, [85, 85, 85, 255, 255, 255]);
        // Never up.
        let planes = flat(4, 2, [16, 128, 128]);
        let frame = Frame::packed(&planes, sizes420(4, 2), 8);
        let Some(frame) = frame else { unreachable!() };
        let out = render(&frame, basis_of(ColorMatrix::Bt709), PictureView::Fit, BOX);
        assert_eq!((out.width, out.height), (4, 2));
    }

    #[test]
    fn fit_keeps_the_aspect() {
        for ((w, h), (bw, bh), want) in [
            ((1920, 1080), (1280, 720), (1280, 720)),
            ((1920, 1080), (640, 640), (640, 360)),
            ((1080, 1920), (1280, 720), (405, 720)),
            ((4000, 1), (100, 100), (100, 1)),
            ((64, 48), (1280, 720), (64, 48)),
        ] {
            let size = PictureBox {
                width: bw,
                height: bh,
            };
            let (ow, oh, _) = layout(w, h, PictureView::Fit, size);
            assert_eq!((ow, oh), want, "{w}x{h} in {bw}x{bh}");
        }
    }

    /// Native is one to one, the origin is moved inside the frame, and
    /// chroma follows luma positions.
    #[test]
    fn native_crops_and_clamps() {
        let (w, h) = (8, 4);
        let y: Vec<u16> = (0..w * h)
            .map(|i| 16 + u16::try_from(i).unwrap_or(0) * 6)
            .collect();
        let planes = [y, vec![128; 8], vec![128; 8]];
        let frame = Frame::packed(&planes, sizes420(w, h), 8);
        let Some(frame) = frame else { unreachable!() };
        let size = PictureBox {
            width: 3,
            height: 2,
        };
        let out = render(
            &frame,
            basis_of(ColorMatrix::Bt709),
            PictureView::Native { x: 100, y: 1 },
            size,
        );
        assert_eq!(out.view, PictureView::Native { x: 5, y: 1 });
        assert_eq!((out.width, out.height), (3, 2));
        let grey = |i: usize| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = (f64::from(u16::try_from(i * 6).unwrap_or(0)) * 255.0 / 219.0).round() as u8;
            v
        };
        let want: Vec<u8> = [13, 14, 15, 21, 22, 23]
            .iter()
            .flat_map(|&i| [grey(i); 3])
            .collect();
        assert_eq!(out.pixels, want);
    }

    /// Chroma of a 4:2:0 frame lands on its luma positions.
    #[test]
    fn subsampled_chroma_follows_luma() {
        // 4x2: left chroma sample red-ish, right one neutral.
        let planes = [
            vec![81, 81, 126, 126, 81, 81, 126, 126],
            vec![90, 128],
            vec![240, 128],
        ];
        let frame = Frame::packed(&planes, sizes420(4, 2), 8);
        let Some(frame) = frame else { unreachable!() };
        let out = render(
            &frame,
            basis_of(ColorMatrix::Bt601),
            PictureView::Native { x: 0, y: 0 },
            BOX,
        );
        let px = |i: usize| {
            [
                out.pixels[i * 3],
                out.pixels[i * 3 + 1],
                out.pixels[i * 3 + 2],
            ]
        };
        assert!(
            near(px(0), [255, 0, 0], 2) && near(px(1), [255, 0, 0], 2),
            "{:?}",
            px(0)
        );
        assert_eq!(px(2), [128; 3]);
    }

    #[test]
    fn the_unstated_matrix_follows_the_size() {
        assert_eq!(basis(None, false, 720, 576).matrix, ColorMatrix::Bt601);
        assert_eq!(basis(None, false, 1024, 576).matrix, ColorMatrix::Bt601);
        assert_eq!(basis(None, false, 1280, 720).matrix, ColorMatrix::Bt709);
        assert!(basis(None, false, 1280, 720).assumed);
        let stated = basis(Some(ColorMatrix::Bt601), false, 1920, 1080);
        assert_eq!((stated.matrix, stated.assumed), (ColorMatrix::Bt601, false));
        assert_eq!(ffmpeg_matrix("bt470bg"), Some(ColorMatrix::Bt601));
        assert_eq!(ffmpeg_matrix("fcc"), None);
    }

    #[test]
    fn bad_geometry_is_refused() {
        let samples = [0_u16; 10];
        assert!(Plane::packed(&samples, 4, 3).is_none());
        assert!(Plane::packed(&samples, 0, 1).is_none());
        assert!(Plane::new(Samples::Wide(&samples), 0, 2, 3, 1).is_none());
        assert!(Plane::packed(&samples, 5, 2).is_some());
    }
}
