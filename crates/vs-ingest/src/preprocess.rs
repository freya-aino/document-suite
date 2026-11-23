//! Image conditioning, as DAG steps.
//!
//! These exist as steps rather than a fixed pass so `vs-adapt` can measure whether each
//! one actually helps a given page class. That is not a formality: binarisation helps some
//! recognition models and hurts others, and nothing in this repo yet knows which.
//!
//! Every step is `page_image → page_image`, so they compose and a DAG can apply none, one
//! or several. Two consequences worth knowing before wiring one into a DAG:
//!
//! - **Failure passes the page through unchanged.** `Artifact::empty` has no empty form
//!   for a page image, so these cannot be `optional` in the usual sense; instead a decode
//!   or encode failure logs and forwards the input. A step that cannot improve a page must
//!   not be able to destroy it.
//! - **`page.binarize` produces greyscale.** Feeding its output to `figure.describe` would
//!   describe a black-and-white figure. Put conditioning on the recognition branch of a
//!   DAG, not ahead of the figure branch.
//!
//! `page.orient` is the one step here whose two halves differ in how much they can be
//! trusted, and the difference is stated in its own documentation rather than hidden:
//! which *axis* the text lines run along is a strong measurement, which *end* is up is a
//! weak one. Dewarping is still absent — estimating a page-curl warp needs the page
//! boundary, not ink statistics — and remains an open item in SPEC.md.

use async_trait::async_trait;
use image::{DynamicImage, GrayImage, RgbImage};
use serde::Deserialize;

use vs_dag::{Artifact, ArtifactKind, PageImage, Signature, Step, StepContext, StepError, StepOutput};

/// Re-encodes as PNG **preserving the colour type**.
///
/// Deliberately not `raster::encode_png`, which converts to RGB: that would undo
/// `page.binarize`, tripling the size of an image whose whole point is to be one channel.
fn encode_preserving(img: &DynamicImage) -> Result<(Vec<u8>, u32, u32), String> {
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok((out, img.width(), img.height()))
}

/// Long side used for the *analysis* copy. Deskew and margin detection only need the
/// layout of ink, not its detail, and measuring on a downscaled copy keeps a 200 dpi page
/// well under a second. The transform itself is always applied to the full-size image.
const ANALYSIS_LONG_SIDE: u32 = 1000;

// ---------------------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------------------

/// Runs `op` on the decoded page and re-encodes the result, or returns the page unchanged.
///
/// `op` returning `None` means "nothing to do" — a page already straight, or already
/// cropped — and costs no re-encode, so a step that finds nothing to fix is nearly free.
async fn map_page(
    page: &PageImage,
    step: &'static str,
    op: impl FnOnce(DynamicImage) -> Option<DynamicImage> + Send + 'static,
) -> Result<StepOutput, StepError> {
    let bytes = page.bytes.clone();
    let dpi = page.dpi;
    // Decoding and re-encoding a full page is CPU-bound; keep it off the async executor.
    let result = tokio::task::spawn_blocking(move || {
        let img = image::load_from_memory(&bytes).map_err(|e| format!("decode: {e}"))?;
        match op(img) {
            Some(out) => encode_preserving(&out).map(Some).map_err(|e| format!("encode: {e}")),
            None => Ok(None),
        }
    })
    .await
    .map_err(|e| StepError::Io(format!("{step} task failed: {e}")))?;

    let artifact = match result {
        Ok(Some((bytes, width_px, height_px))) => Artifact::PageImage(PageImage {
            // Distinct from the input so provenance shows the page a model actually saw
            // was conditioned, and by which step.
            uri: format!("{}#{step}", page.uri),
            bytes: std::sync::Arc::from(bytes),
            width_px,
            height_px,
            dpi,
        }),
        Ok(None) => Artifact::PageImage(page.clone()),
        // The page is the one artifact a step must never lose: a model can still read an
        // unconditioned page, but it cannot read nothing.
        Err(reason) => {
            tracing::warn!(step, %reason, "pre-processing skipped; page passed through unchanged");
            Artifact::PageImage(page.clone())
        }
    };
    Ok(StepOutput::plain(artifact))
}

fn page_signature() -> Signature {
    Signature { inputs: vec![ArtifactKind::PageImage], output: ArtifactKind::PageImage }
}

fn page_input(inputs: &[&Artifact]) -> Result<PageImage, StepError> {
    match inputs.first() {
        Some(Artifact::PageImage(p)) => Ok(p.clone()),
        _ => Err(StepError::InvalidParams("expected a page image".into())),
    }
}

fn params_of<T: for<'de> Deserialize<'de> + Default>(params: &serde_json::Value) -> Result<T, StepError> {
    if params.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(params.clone()).map_err(|e| StepError::InvalidParams(e.to_string()))
}

/// Greyscale copy with its long side at most `long_side`, for measurement only.
fn analysis_gray(img: &DynamicImage, long_side: u32) -> GrayImage {
    let (w, h) = (img.width(), img.height());
    let scaled = if w.max(h) > long_side {
        let scale = long_side as f32 / w.max(h) as f32;
        let (nw, nh) = ((w as f32 * scale).max(1.0) as u32, (h as f32 * scale).max(1.0) as u32);
        img.resize_exact(nw, nh, image::imageops::FilterType::Triangle)
    } else {
        img.clone()
    };
    scaled.to_luma8()
}

// ---------------------------------------------------------------------------------------
// Measurements
// ---------------------------------------------------------------------------------------

/// Fraction of pixels darker than the Otsu threshold: how much ink is on the page.
///
/// A real signal for a raster input, which otherwise reaches the classifier with nothing
/// but a thumbnail: it separates a blank or near-blank scan (< 0.005) from a dense one.
pub fn ink_coverage(gray: &GrayImage) -> f32 {
    let total = (gray.width() as usize) * (gray.height() as usize);
    if total == 0 {
        return 0.0;
    }
    let level = imageproc::contrast::otsu_level(gray);
    let dark = gray.pixels().filter(|p| p.0[0] <= level).count();
    dark as f32 / total as f32
}

/// How many long straight ruled lines the page has, horizontal and vertical together.
///
/// The signal that tells a **form or a table** from prose on a page nobody has read yet.
/// Tier 0 could measure only how much ink a scan had, which separates a blank page from a
/// dense one and nothing else — so a form, a photograph and a page of text all arrived at
/// the vision tier indistinguishable, and `has_tables` was false for every rules-only
/// page whatever was on it.
///
/// A ruled line is a row (or column) with one **contiguous** dark run across most of the
/// page. Contiguity is what does the work: a line of type also puts a lot of ink in its
/// row, but in short runs with white between the words, so its longest unbroken run is a
/// word wide. A rule's is the whole table. Measuring the run rather than the coverage is
/// therefore the difference between finding rules and finding text.
///
/// Adjacent dark rows are counted once, because a rule two pixels thick is one rule.
pub fn ruling_lines(gray: &GrayImage) -> u16 {
    /// Fraction of the page's width (or height) one unbroken dark run must cover.
    const MIN_RUN: f32 = 0.7;
    /// Pale pixels this many in a row do not break a rule: a scanned line drops out for a
    /// pixel or two, and a rule broken by a speck is still a rule.
    const TOLERANCE: u32 = 2;

    let (w, h) = (gray.width(), gray.height());
    if w < 20 || h < 20 {
        return 0;
    }
    let level = imageproc::contrast::otsu_level(gray);
    // Longest tolerant dark run along a line of pixels, as a fraction of its length.
    let longest_run = |dark: &dyn Fn(u32) -> bool, len: u32| -> f32 {
        let (mut best, mut run, mut gap) = (0u32, 0u32, 0u32);
        for i in 0..len {
            if dark(i) {
                run += 1 + std::mem::take(&mut gap);
                best = best.max(run);
            } else if run > 0 && gap < TOLERANCE {
                gap += 1;
            } else {
                run = 0;
                gap = 0;
            }
        }
        best as f32 / len as f32
    };
    let mut count: u16 = 0;
    // Rows, then columns; a run of adjacent ruled lines counts once.
    let mut in_run = false;
    for y in 0..h {
        let ruled = longest_run(&|x| gray.get_pixel(x, y).0[0] <= level, w) >= MIN_RUN;
        if ruled && !in_run {
            count = count.saturating_add(1);
        }
        in_run = ruled;
    }
    in_run = false;
    for x in 0..w {
        let ruled = longest_run(&|y| gray.get_pixel(x, y).0[0] <= level, h) >= MIN_RUN;
        if ruled && !in_run {
            count = count.saturating_add(1);
        }
        in_run = ruled;
    }
    count
}

/// Fraction of pixels in the mid-grey band: how **continuous-tone** the page is.
///
/// What separates a photograph from a document. A scanned or born-digital page of text is
/// bimodal — nearly white paper, nearly black ink, very little between — while a
/// photograph has its tones spread through the middle. Measured as the fraction of pixels
/// in the central half of the range, which needs one histogram and no model.
///
/// Known limits, which is why it is a fraction and not a verdict: a heavily shadowed scan
/// (a phone photo of a page) reads as continuous-tone, and so does a page printed on grey
/// stock. Both are cases where treating the page as a photograph — handing it to a vision
/// model rather than a text parser — is the safer error anyway.
pub fn tone_continuity(gray: &GrayImage) -> f32 {
    let total = (gray.width() as usize) * (gray.height() as usize);
    if total == 0 {
        return 0.0;
    }
    let mid = gray.pixels().filter(|p| (64..=191).contains(&p.0[0])).count();
    mid as f32 / total as f32
}

/// The rotation, in degrees, that would straighten the page — i.e. the value to hand
/// straight to a rotate, not the page's own tilt, which has the opposite sign.
///
/// Projection profile: a straight page of text has rows that alternate between dense ink
/// and empty space, so the horizontal ink histogram has high variance. Shearing the ink
/// by a candidate angle and scoring the histogram finds the angle that maximises it.
///
/// The shear is computed by projecting each dark pixel to `y + x·tan θ` rather than by
/// rotating the image once per candidate, so the whole search is a handful of passes over
/// the dark pixels instead of 40 image rotations.
pub fn skew_angle(gray: &GrayImage, max_deg: f32, step_deg: f32) -> f32 {
    let (w, h) = (gray.width() as i64, gray.height() as i64);
    if w == 0 || h == 0 {
        return 0.0;
    }
    let level = imageproc::contrast::otsu_level(gray);
    // Collect dark pixels once; the search then only iterates over ink.
    let ink: Vec<(i64, i64)> = gray
        .enumerate_pixels()
        .filter(|(_, _, p)| p.0[0] <= level)
        .map(|(x, y, _)| (x as i64, y as i64))
        .collect();
    // Too little ink to measure anything: a blank or near-blank page has no skew to find.
    if ink.len() < 200 {
        return 0.0;
    }

    let max_deg = max_deg.abs().clamp(0.0, 20.0);
    let step_deg = if step_deg > 0.0 { step_deg } else { 0.25 };
    let mut best = (0.0f32, f32::MIN);
    let mut angle = -max_deg;
    while angle <= max_deg + f32::EPSILON {
        let tan = angle.to_radians().tan();
        // Room for the shear at both ends, so no ink falls outside the histogram.
        let margin = (w as f32 * tan.abs()).ceil() as i64 + 1;
        let bins = (h + 2 * margin) as usize;
        let mut hist = vec![0u32; bins];
        for (x, y) in &ink {
            let row = *y + (*x as f32 * tan) as i64 + margin;
            if row >= 0 && (row as usize) < bins {
                hist[row as usize] += 1;
            }
        }
        // Sum of squares: maximal when ink is concentrated into few rows, i.e. when the
        // text lines are horizontal. Equivalent to maximising variance here, because the
        // total ink is the same for every angle.
        let score: f64 = hist.iter().map(|c| (*c as f64) * (*c as f64)).sum();
        if score as f32 > best.1 {
            best = (angle, score as f32);
        }
        angle += step_deg;
    }
    best.0
}

/// Fraction of dark pixels per row, and per column.
fn ink_profiles(gray: &GrayImage, level: u8) -> (Vec<f32>, Vec<f32>) {
    let (w, h) = (gray.width() as usize, gray.height() as usize);
    let mut rows = vec![0f32; h];
    let mut cols = vec![0f32; w];
    for (x, y, p) in gray.enumerate_pixels() {
        if p.0[0] <= level {
            rows[y as usize] += 1.0;
            cols[x as usize] += 1.0;
        }
    }
    for r in rows.iter_mut() {
        *r /= w.max(1) as f32;
    }
    for c in cols.iter_mut() {
        *c /= h.max(1) as f32;
    }
    (rows, cols)
}

/// How strongly a profile alternates between ink and space, relative to its own mean.
///
/// Text lines make the profile across them spiky and the profile along them flat, so this
/// is larger on the axis that crosses the lines. Normalising by the mean is what makes
/// the two axes of one page comparable: a page has the same total ink whichever way it is
/// held, but not the same number of bins.
fn line_score(profile: &[f32]) -> f32 {
    let n = profile.len() as f32;
    if n == 0.0 {
        return 0.0;
    }
    let mean = profile.iter().sum::<f32>() / n;
    if mean <= f32::EPSILON {
        return 0.0;
    }
    let var = profile.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / n;
    var / (mean * mean)
}

/// Contiguous runs of rows carrying more than `cutoff` ink: the text lines.
fn bands(profile: &[f32], cutoff: f32) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, v) in profile.iter().enumerate() {
        match (*v > cutoff, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, profile.len()));
    }
    out
}

/// Where the ink sits inside its text lines: positive when it is below the middle of the
/// line, negative when above. Zero when there are no lines to measure.
///
/// Upright text is positive, and the reason is typographic rather than statistical. Most
/// of a line's ink is its x-height band, which sits **against the baseline** — at the
/// bottom of the line. Above it there is only the sparse ascender zone; below it only
/// descenders, which are rare enough that the band this finds usually runs from the
/// ascender line down to the baseline. So the centre of mass of an upright line of text
/// sits below the middle of its own extent.
///
/// **It is still the weaker of the two measurements.** The effect is a few percent of
/// the line height and it assumes a script with an x-height: a page of all-caps text or
/// of bare figures has little asymmetry to find, and a script without ascenders and
/// descenders may show none. That is why a 180° turn is applied only when this clears a
/// margin, and the page is otherwise left as it came.
///
/// (An earlier version of this had the sign the other way round, reasoning from
/// ascenders being more frequent than descenders. They are, but the x-height block
/// dominates both, so the conclusion was simply backwards — and every page would have
/// been flipped. The round-trip test in `tests/preprocess.rs` is what pins the
/// direction.)
fn baseline_heaviness(profile: &[f32]) -> f32 {
    let n = profile.len();
    if n == 0 {
        return 0.0;
    }
    let mean = profile.iter().sum::<f32>() / n as f32;
    if mean <= f32::EPSILON {
        return 0.0;
    }
    let mut weighted = 0.0;
    let mut total = 0.0;
    for (from, to) in bands(profile, mean * 0.5) {
        let height = (to - from) as f32;
        // One- and two-row bands are noise, not lines, and have no inside to speak of.
        if height < 3.0 {
            continue;
        }
        let ink: f32 = profile[from..to].iter().sum();
        if ink <= f32::EPSILON {
            continue;
        }
        let centroid: f32 = profile[from..to].iter().enumerate().map(|(i, v)| (i as f32 + 0.5) * v).sum::<f32>() / ink;
        // Fraction of the line height by which the ink's centre of mass sits below the
        // line's geometric centre. Rows run downwards, so "below" is the larger index.
        weighted += (centroid - height / 2.0) / height * ink;
        total += ink;
    }
    if total <= f32::EPSILON {
        0.0
    } else {
        weighted / total
    }
}

/// The rotation in degrees (0, 90, 180 or 270) that would stand the page upright, where
/// 90 means a quarter turn clockwise.
///
/// Two separate decisions, with very different confidence:
///
/// 1. **Which axis the text lines run along** — strong. A page of text has a spiky ink
///    profile across its lines and a flat one along them, so comparing the two axes says
///    whether the page is sideways. `axis_margin` is how much spikier the other axis must
///    be before the page is turned.
/// 2. **Which end is up** — weak; see [`baseline_heaviness`]. Applied only when the
///    measurement clears `min_asymmetry`. A sideways page is turned either way, because
///    it is unreadable either way and clockwise is as good a guess as any when the
///    measurement is not convincing — that guess is logged. An upright page, by
///    contrast, is left alone unless the measurement is convincing: it is already
///    readable, so a wrong flip would be a loss rather than a coin toss.
///
/// Returns 0 for a page with too little ink to measure, which is the honest answer: an
/// unrotated page and an unmeasurable one are treated the same.
pub fn orientation(gray: &GrayImage, axis_margin: f32, min_asymmetry: f32) -> u16 {
    let (w, h) = (gray.width(), gray.height());
    if w == 0 || h == 0 {
        return 0;
    }
    let level = imageproc::contrast::otsu_level(gray);
    let dark = gray.pixels().filter(|p| p.0[0] <= level).count();
    if dark < 200 {
        return 0;
    }
    let (rows, cols) = ink_profiles(gray, level);
    let (row_score, col_score) = (line_score(&rows), line_score(&cols));

    // Reversing a profile reflects every band about its own centre, so it negates the
    // asymmetry exactly. One measurement therefore decides both of the remaining cases:
    // its sign is which end is up, and its size is how much that is worth believing.
    let min_asymmetry = min_asymmetry.max(0.0);

    if col_score > row_score * axis_margin.max(1.0) {
        // Sideways. A clockwise quarter turn takes the left edge to the top, so the
        // turned page's rows are the columns in the order they already are.
        let a = baseline_heaviness(&cols);
        if a.abs() < min_asymmetry {
            tracing::debug!(asymmetry = a, "page is sideways; which way up is a guess");
            return 90;
        }
        return if a > 0.0 { 90 } else { 270 };
    }

    // Upright axis: only the flip is left.
    if baseline_heaviness(&rows) < -min_asymmetry {
        return 180;
    }
    0
}

/// Ink bounding box in fractions of the page, or `None` when there is no ink to bound.
pub fn ink_bounds(gray: &GrayImage, min_ink_per_line: f32) -> Option<(f32, f32, f32, f32)> {
    let (w, h) = (gray.width(), gray.height());
    if w == 0 || h == 0 {
        return None;
    }
    let level = imageproc::contrast::otsu_level(gray);
    let mut rows = vec![0u32; h as usize];
    let mut cols = vec![0u32; w as usize];
    for (x, y, p) in gray.enumerate_pixels() {
        if p.0[0] <= level {
            rows[y as usize] += 1;
            cols[x as usize] += 1;
        }
    }
    // A per-line floor keeps a speck of scanner noise in the margin from defeating the
    // crop, which is the usual way a naive ink-bounds crop does nothing at all.
    let row_floor = (min_ink_per_line * w as f32).max(1.0) as u32;
    let col_floor = (min_ink_per_line * h as f32).max(1.0) as u32;
    let first = |v: &[u32], floor: u32| v.iter().position(|c| *c >= floor);
    let last = |v: &[u32], floor: u32| v.iter().rposition(|c| *c >= floor);
    let (y0, y1) = (first(&rows, row_floor)?, last(&rows, row_floor)?);
    let (x0, x1) = (first(&cols, col_floor)?, last(&cols, col_floor)?);
    Some((x0 as f32 / w as f32, y0 as f32 / h as f32, (x1 + 1) as f32 / w as f32, (y1 + 1) as f32 / h as f32))
}

/// Percentile bounds of the luma histogram, used as the contrast-stretch input range.
fn luma_percentiles(gray: &GrayImage, low: f32, high: f32) -> (u8, u8) {
    let mut hist = [0u32; 256];
    for p in gray.pixels() {
        hist[p.0[0] as usize] += 1;
    }
    let total: u32 = hist.iter().sum();
    if total == 0 {
        return (0, 255);
    }
    let pick = |frac: f32| -> u8 {
        let target = (total as f32 * frac.clamp(0.0, 1.0)) as u32;
        let mut acc = 0u32;
        for (v, c) in hist.iter().enumerate() {
            acc += c;
            if acc >= target {
                return v as u8;
            }
        }
        255
    };
    let (lo, hi) = (pick(low), pick(high));
    if lo >= hi { (0, 255) } else { (lo, hi) }
}

// ---------------------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct OrientParams {
    /// How much spikier the other axis's ink profile must be before the page is treated
    /// as sideways. 1.0 would turn a page on the slightest difference.
    pub axis_margin: f32,
    /// Smallest ink asymmetry that is allowed to decide which end is up. Below it the
    /// page keeps the orientation it came with (or, when sideways, takes the documented
    /// guess), because the measurement is weak — see `baseline_heaviness`.
    pub min_asymmetry: f32,
}

impl Default for OrientParams {
    fn default() -> Self {
        Self { axis_margin: 1.3, min_asymmetry: 0.02 }
    }
}

/// Turns a page that was scanned or photographed sideways or upside down.
///
/// A quarter turn is lossless and the measurement behind it is strong. Which *end* is up
/// is a weaker judgement, and deliberately conservative: see [`orientation`].
pub struct Orient;

#[async_trait]
impl Step for Orient {
    fn kind(&self) -> &'static str {
        "page.orient"
    }
    fn signature(&self) -> Signature {
        page_signature()
    }
    fn default_timeout_ms(&self) -> u64 {
        30_000
    }
    async fn run(&self, _ctx: &StepContext, inputs: &[&Artifact], params: &serde_json::Value) -> Result<StepOutput, StepError> {
        let page = page_input(inputs)?;
        let p: OrientParams = params_of(params)?;
        map_page(&page, "page.orient", move |img| {
            let gray = analysis_gray(&img, ANALYSIS_LONG_SIDE);
            let deg = orientation(&gray, p.axis_margin, p.min_asymmetry);
            if deg == 0 {
                return None;
            }
            tracing::debug!(deg, "turning page upright");
            // Quarter turns and flips move whole pixels, so unlike deskew this costs no
            // resampling and no detail.
            Some(match deg {
                90 => img.rotate90(),
                180 => img.rotate180(),
                _ => img.rotate270(),
            })
        })
        .await
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct DeskewParams {
    /// Largest skew searched, in degrees either way.
    pub max_deg: f32,
    /// Search granularity, in degrees.
    pub step_deg: f32,
    /// Angles below this are left alone: rotating costs a resample, which is a real loss
    /// of detail for a correction nobody can see.
    pub min_apply_deg: f32,
}

impl Default for DeskewParams {
    fn default() -> Self {
        Self { max_deg: 5.0, step_deg: 0.25, min_apply_deg: 0.2 }
    }
}

/// Straightens a scan whose text lines are not horizontal.
pub struct Deskew;

#[async_trait]
impl Step for Deskew {
    fn kind(&self) -> &'static str {
        "page.deskew"
    }
    fn signature(&self) -> Signature {
        page_signature()
    }
    fn default_timeout_ms(&self) -> u64 {
        30_000
    }
    async fn run(&self, _ctx: &StepContext, inputs: &[&Artifact], params: &serde_json::Value) -> Result<StepOutput, StepError> {
        let page = page_input(inputs)?;
        let p: DeskewParams = params_of(params)?;
        map_page(&page, "page.deskew", move |img| {
            let gray = analysis_gray(&img, ANALYSIS_LONG_SIDE);
            let angle = skew_angle(&gray, p.max_deg, p.step_deg);
            if angle.abs() < p.min_apply_deg.max(0.0) {
                return None;
            }
            tracing::debug!(angle, "deskewing page");
            let rgb = img.to_rgb8();
            // White border: a document's background. Black would be read as ink by every
            // downstream measurement, including this step's own on a second pass.
            let rotated = imageproc::geometric_transformations::rotate_about_center(
                &rgb,
                // `skew_angle` returns the correction, so it is applied as-is. Negating
                // here doubles the skew instead of removing it; the round-trip test in
                // this module is what pins the direction.
                angle.to_radians(),
                imageproc::geometric_transformations::Interpolation::Bilinear,
                imageproc::geometric_transformations::Border::Constant(image::Rgb([255u8, 255, 255])),
            );
            Some(DynamicImage::ImageRgb8(rotated))
        })
        .await
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct DenoiseParams {
    /// Median window radius in pixels. 1 (a 3×3 window) clears scanner speckle; larger
    /// starts eating thin strokes and punctuation.
    pub radius: u32,
}

impl Default for DenoiseParams {
    fn default() -> Self {
        Self { radius: 1 }
    }
}

/// Median filter, for speckled scans and photos.
pub struct Denoise;

#[async_trait]
impl Step for Denoise {
    fn kind(&self) -> &'static str {
        "page.denoise"
    }
    fn signature(&self) -> Signature {
        page_signature()
    }
    fn default_timeout_ms(&self) -> u64 {
        60_000
    }
    async fn run(&self, _ctx: &StepContext, inputs: &[&Artifact], params: &serde_json::Value) -> Result<StepOutput, StepError> {
        let page = page_input(inputs)?;
        let p: DenoiseParams = params_of(params)?;
        let radius = p.radius.clamp(1, 4);
        map_page(&page, "page.denoise", move |img| {
            // Per channel, so colour survives: a denoise that greyscaled the page would
            // also degrade any figure on it.
            let rgb: RgbImage = img.to_rgb8();
            Some(DynamicImage::ImageRgb8(imageproc::filter::median_filter(&rgb, radius, radius)))
        })
        .await
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct NormalizeParams {
    /// Luma percentiles mapped to black and white.
    pub low_percentile: f32,
    pub high_percentile: f32,
}

impl Default for NormalizeParams {
    fn default() -> Self {
        Self { low_percentile: 0.02, high_percentile: 0.98 }
    }
}

/// Contrast normalisation for faint or unevenly lit pages.
pub struct Normalize;

#[async_trait]
impl Step for Normalize {
    fn kind(&self) -> &'static str {
        "page.normalize"
    }
    fn signature(&self) -> Signature {
        page_signature()
    }
    fn default_timeout_ms(&self) -> u64 {
        30_000
    }
    async fn run(&self, _ctx: &StepContext, inputs: &[&Artifact], params: &serde_json::Value) -> Result<StepOutput, StepError> {
        let page = page_input(inputs)?;
        let p: NormalizeParams = params_of(params)?;
        map_page(&page, "page.normalize", move |img| {
            // Bounds come from luma and are applied to every channel equally, so a faint
            // page is brightened without shifting its colour balance — which per-channel
            // stretching would do, turning a yellowed scan blue.
            let gray = img.to_luma8();
            let (lo, hi) = luma_percentiles(&gray, p.low_percentile, p.high_percentile);
            if (lo, hi) == (0, 255) {
                return None;
            }
            let rgb = img.to_rgb8();
            Some(DynamicImage::ImageRgb8(imageproc::contrast::stretch_contrast(&rgb, lo, hi, 0, 255)))
        })
        .await
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct BinarizeParams {
    /// Fixed threshold instead of Otsu's, for a corpus where Otsu is known to misbehave.
    pub threshold: Option<u8>,
}

/// Otsu binarisation.
///
/// Helps some recognition models and hurts others, which is the clearest case in the
/// catalogue for being a scored step rather than an assumption. **Outputs greyscale**, so
/// it must not feed a figure or chart branch.
pub struct Binarize;

#[async_trait]
impl Step for Binarize {
    fn kind(&self) -> &'static str {
        "page.binarize"
    }
    fn signature(&self) -> Signature {
        page_signature()
    }
    fn default_timeout_ms(&self) -> u64 {
        30_000
    }
    async fn run(&self, _ctx: &StepContext, inputs: &[&Artifact], params: &serde_json::Value) -> Result<StepOutput, StepError> {
        let page = page_input(inputs)?;
        let p: BinarizeParams = params_of(params)?;
        map_page(&page, "page.binarize", move |img| {
            let gray = img.to_luma8();
            let level = p.threshold.unwrap_or_else(|| imageproc::contrast::otsu_level(&gray));
            let out = imageproc::contrast::threshold(&gray, level, imageproc::contrast::ThresholdType::Binary);
            Some(DynamicImage::ImageLuma8(out))
        })
        .await
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct CropMarginsParams {
    /// Margin kept around the ink, as a fraction of the page.
    pub padding: f32,
    /// Ink a row or column needs, as a fraction of its length, to count as content.
    pub min_ink_per_line: f32,
    /// Refuse a crop that would keep less than this fraction of the page area. A crop
    /// that aggressive almost always means the ink measurement was wrong, not that the
    /// page is 90 % margin.
    pub min_kept_area: f32,
}

impl Default for CropMarginsParams {
    fn default() -> Self {
        Self { padding: 0.01, min_ink_per_line: 0.005, min_kept_area: 0.2 }
    }
}

/// Trims scanner borders and blank margins.
pub struct CropMargins;

#[async_trait]
impl Step for CropMargins {
    fn kind(&self) -> &'static str {
        "page.crop_margins"
    }
    fn signature(&self) -> Signature {
        page_signature()
    }
    fn default_timeout_ms(&self) -> u64 {
        30_000
    }
    async fn run(&self, _ctx: &StepContext, inputs: &[&Artifact], params: &serde_json::Value) -> Result<StepOutput, StepError> {
        let page = page_input(inputs)?;
        let p: CropMarginsParams = params_of(params)?;
        map_page(&page, "page.crop_margins", move |img| {
            let gray = analysis_gray(&img, ANALYSIS_LONG_SIDE);
            let (x0, y0, x1, y1) = ink_bounds(&gray, p.min_ink_per_line)?;
            let pad = p.padding.clamp(0.0, 0.2);
            let (x0, y0) = ((x0 - pad).max(0.0), (y0 - pad).max(0.0));
            let (x1, y1) = ((x1 + pad).min(1.0), (y1 + pad).min(1.0));
            let kept = (x1 - x0) * (y1 - y0);
            if kept < p.min_kept_area.clamp(0.0, 1.0) || kept >= 0.999 {
                return None;
            }
            let (w, h) = (img.width() as f32, img.height() as f32);
            let (px, py) = ((x0 * w).round() as u32, (y0 * h).round() as u32);
            let (pw, ph) = (((x1 - x0) * w).round().max(1.0) as u32, ((y1 - y0) * h).round().max(1.0) as u32);
            tracing::debug!(kept, "cropping page margins");
            Some(img.crop_imm(px, py, pw, ph))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;

    /// A page of horizontal "text lines": dark bands separated by white.
    fn lined_page(w: u32, h: u32) -> GrayImage {
        GrayImage::from_fn(w, h, |x, y| {
            let in_margin = x < w / 10 || x > w * 9 / 10;
            let on_line = (y / 4) % 3 == 0 && y > h / 10 && y < h * 9 / 10;
            if on_line && !in_margin { Luma([20]) } else { Luma([240]) }
        })
    }

    fn blank(w: u32, h: u32) -> GrayImage {
        GrayImage::from_pixel(w, h, Luma([250]))
    }

    /// Rotates a page the way a crooked scan is crooked, using the same primitive the
    /// step uses to correct it.
    fn rotated(src: &GrayImage, deg: f32) -> GrayImage {
        imageproc::geometric_transformations::rotate_about_center(
            src,
            deg.to_radians(),
            imageproc::geometric_transformations::Interpolation::Bilinear,
            imageproc::geometric_transformations::Border::Constant(Luma([240])),
        )
    }

    /// A ruled table: full-width horizontal rules and full-height vertical ones. Text
    /// never makes a row that dark across the page, which is what the measurement rests
    /// on — so a page of type must come back with no rules at all.
    #[test]
    fn ruling_lines_find_a_table_and_not_a_page_of_text() {
        let table = GrayImage::from_fn(300, 300, |x, y| {
            let on_rule = y % 50 == 0 || x % 60 == 0;
            if on_rule { Luma([20]) } else { Luma([245]) }
        });
        let found = ruling_lines(&table);
        assert!(found >= 8, "a ruled grid should show its rules, found {found}");

        // Text: short dark runs with white between the words. Its rows carry plenty of
        // ink — which is why coverage alone could not tell them from rules — but no long
        // unbroken run.
        let text = GrayImage::from_fn(300, 300, |x, y| {
            let on_line = (y / 4) % 3 == 0 && (10..290).contains(&y);
            let in_word = (x / 9) % 2 == 0 && (20..280).contains(&x);
            if on_line && in_word { Luma([20]) } else { Luma([245]) }
        });
        assert_eq!(ruling_lines(&text), 0, "lines of type are not ruled lines");
        assert_eq!(ruling_lines(&blank(300, 300)), 0);
        assert_eq!(ruling_lines(&GrayImage::new(4, 4)), 0, "too small to measure");

        // A rule several pixels thick is one rule, not one per pixel row.
        let thick = GrayImage::from_fn(200, 200, |_, y| if (100..104).contains(&y) { Luma([20]) } else { Luma([245]) });
        assert_eq!(ruling_lines(&thick), 1);
    }

    #[test]
    fn tone_continuity_separates_a_photograph_from_a_document() {
        // A document: paper and ink, nothing in between.
        let document = lined_page(200, 200);
        assert!(tone_continuity(&document) < 0.05, "got {}", tone_continuity(&document));

        // A photograph: a smooth gradient through the mid tones.
        let photo = GrayImage::from_fn(200, 200, |x, y| Luma([((x + y) * 255 / 398) as u8]));
        assert!(tone_continuity(&photo) > 0.4, "got {}", tone_continuity(&photo));
        assert_eq!(tone_continuity(&GrayImage::new(0, 0)), 0.0);
    }

    #[test]
    fn ink_coverage_separates_blank_from_dense() {
        assert!(ink_coverage(&blank(200, 200)) < 0.01);
        let dense = ink_coverage(&lined_page(200, 200));
        assert!((0.05..0.5).contains(&dense), "lined page coverage was {dense}");
        assert_eq!(ink_coverage(&GrayImage::new(0, 0)), 0.0);
    }

    #[test]
    fn a_straight_page_reads_as_straight() {
        assert!(skew_angle(&lined_page(400, 400), 5.0, 0.25).abs() < 0.3);
    }

    /// The property that matters, and the one that pins the sign convention: applying the
    /// correction the step applies must leave the page measurably straighter than it was.
    /// Asserting the returned angle against a "true" angle instead would only test my
    /// arithmetic for generating the test image.
    #[test]
    fn the_correction_straightens_a_crooked_page() {
        let page = lined_page(400, 400);
        for truth in [-3.0f32, -1.5, 1.0, 2.5] {
            let crooked = rotated(&page, truth);
            let found = skew_angle(&crooked, 5.0, 0.25);
            assert!(found.abs() > 0.5, "rotated by {truth}° but measured {found}° — no skew detected");

            // Exactly what `Deskew` does with the measurement.
            let corrected = rotated(&crooked, found);
            let residual = skew_angle(&corrected, 5.0, 0.25);
            assert!(
                residual.abs() < found.abs() / 2.0,
                "rotated {truth}°, measured {found}°, residual after correction {residual}°"
            );
            assert!(residual.abs() < 0.8, "residual {residual}° after correcting {truth}°");
        }
    }

    /// Nothing to measure must read as no skew, not as a random angle.
    #[test]
    fn blank_and_tiny_pages_have_no_skew() {
        assert_eq!(skew_angle(&blank(300, 300), 5.0, 0.25), 0.0);
        assert_eq!(skew_angle(&GrayImage::new(0, 0), 5.0, 0.25), 0.0);
        assert_eq!(skew_angle(&lined_page(4, 4), 5.0, 0.25), 0.0);
    }

    #[test]
    fn ink_bounds_find_the_content_block() {
        let page = lined_page(400, 400);
        let (x0, y0, x1, y1) = ink_bounds(&page, 0.005).unwrap();
        // The generator leaves a tenth of the page blank on every side.
        assert!((0.05..0.15).contains(&x0), "x0 = {x0}");
        assert!((0.05..0.15).contains(&y0), "y0 = {y0}");
        assert!((0.85..0.96).contains(&x1), "x1 = {x1}");
        assert!((0.85..0.96).contains(&y1), "y1 = {y1}");
        assert_eq!(ink_bounds(&blank(100, 100), 0.005), None, "a blank page has no bounds");
    }

    /// A speck of noise in the margin must not defeat the crop.
    #[test]
    fn ink_bounds_ignore_isolated_specks() {
        let mut page = lined_page(400, 400);
        page.put_pixel(2, 2, Luma([0]));
        page.put_pixel(397, 397, Luma([0]));
        let (x0, _, x1, _) = ink_bounds(&page, 0.005).unwrap();
        assert!(x0 > 0.05, "a corner speck should not widen the bounds to the page edge");
        assert!(x1 < 0.96);
    }

    #[test]
    fn percentiles_bracket_the_histogram() {
        let (lo, hi) = luma_percentiles(&lined_page(200, 200), 0.02, 0.98);
        assert!(lo < hi);
        assert!(hi >= 200, "the paper background should land near white, got {hi}");
        // A flat image has nothing to stretch, which the caller reads as "do nothing".
        assert_eq!(luma_percentiles(&blank(50, 50), 0.02, 0.98), (0, 255));
        assert_eq!(luma_percentiles(&GrayImage::new(0, 0), 0.02, 0.98), (0, 255));
    }
}
