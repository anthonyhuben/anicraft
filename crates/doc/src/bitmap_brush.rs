//! Editable bitmap-tip strokes. The document stores a Bézier centerline, per-anchor
//! channels, a color, a brush reference, and a stable seed.

use crate::{Rgba, bitmap::base64_bytes};
use anicraft_geom::{Path, PathCmd, Vec2, lerp, walk::CurveWalk};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

mod color;

pub const MAX_TIP_EDGE: u32 = 256;
pub const MAX_TIPS: usize = 32;
pub const MAX_BRUSHES: usize = 64;
pub const MAX_POINTS: usize = 4096;
pub const MAX_STAMPS: usize = 16_384;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BrushId(pub u64);

/// An R8 tip. A texel of 255 is opaque; zero is transparent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "TipFile")]
pub struct TipImage {
    width: u32,
    height: u32,
    #[serde(with = "base64_bytes")]
    mask: Arc<Vec<u8>>,
}

#[derive(Deserialize)]
struct TipFile {
    width: u32,
    height: u32,
    #[serde(with = "base64_bytes")]
    mask: Arc<Vec<u8>>,
}

impl TryFrom<TipFile> for TipImage {
    type Error = &'static str;

    fn try_from(t: TipFile) -> Result<Self, Self::Error> {
        if !(1..=MAX_TIP_EDGE).contains(&t.width) || !(1..=MAX_TIP_EDGE).contains(&t.height) || t.mask.len() != (t.width * t.height) as usize {
            return Err("brush tip dimensions or mask length are invalid");
        }
        Ok(Self { width: t.width, height: t.height, mask: t.mask })
    }
}

impl TipImage {
    pub fn new(width: u32, height: u32, mask: Vec<u8>) -> Option<Self> {
        Self::try_from(TipFile { width, height, mask: Arc::new(mask) }).ok()
    }

    /// Reads grayscale intensity times PNG alpha using the shared bounded PNG decoder.
    pub fn from_png(png: &[u8]) -> Option<Self> {
        let mut opts = craft_image_codecs::DecodeOptions::default();
        opts.limits.max_width = MAX_TIP_EDGE;
        opts.limits.max_height = MAX_TIP_EDGE;
        let img = craft_image_codecs::decode_as_with(craft_image_codecs::Format::Png, png, &opts).ok()?;
        let (w, h) = img.dimensions();
        let mask = img
            .to_rgba8()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&[r, g, b, a]| {
                let gray = (54 * u32::from(r) + 183 * u32::from(g) + 19 * u32::from(b) + 128) / 256;
                ((gray * u32::from(a) + 127) / 255) as u8
            })
            .collect();
        Self::new(w, h, mask)
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn mask(&self) -> &[u8] {
        &self.mask
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TipSequence {
    Fixed(u16),
    #[default]
    InOrder,
    Random,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AngleMode {
    #[default]
    FollowPath,
    Fixed(f32),
    Random,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StampBlend {
    #[default]
    Accumulate,
    MaxPerStroke,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorDrift {
    /// OKLCH hue variation in radians.
    pub hue: f32,
    pub chroma: f32,
    pub lightness: f32,
    /// Noise cells per path unit.
    pub frequency: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrushDef {
    pub id: BrushId,
    pub name: String,
    pub tips: Vec<TipImage>,
    pub sequence: TipSequence,
    pub spacing: f32,
    pub min_spacing_world: f32,
    pub size_scale: f32,
    pub aspect: f32,
    pub angle_mode: AngleMode,
    pub angle_offset: f32,
    pub angle_jitter: f32,
    pub scatter: f32,
    pub size_jitter: f32,
    pub opacity_jitter: f32,
    pub flow: f32,
    pub blend: StampBlend,
    pub color_drift: ColorDrift,
    /// Whether freehand input maps pen pressure into the per-point opacity channel.
    pub pressure_to_opacity: bool,
}

impl Default for BrushDef {
    fn default() -> Self {
        Self {
            id: BrushId(0),
            name: String::new(),
            tips: Vec::new(),
            sequence: TipSequence::InOrder,
            spacing: 0.2,
            min_spacing_world: 0.25,
            size_scale: 1.0,
            aspect: 1.0,
            angle_mode: AngleMode::FollowPath,
            angle_offset: 0.0,
            angle_jitter: 0.0,
            scatter: 0.0,
            size_jitter: 0.0,
            opacity_jitter: 0.0,
            flow: 1.0,
            blend: StampBlend::Accumulate,
            color_drift: ColorDrift::default(),
            pressure_to_opacity: true,
        }
    }
}

impl BrushDef {
    pub fn valid_tips(&self) -> bool {
        (1..=MAX_TIPS).contains(&self.tips.len())
            && match self.sequence {
                TipSequence::Fixed(i) => usize::from(i) < self.tips.len(),
                _ => true,
            }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PointAttributes {
    pub width: f32,
    pub opacity: f32,
    pub drift: f32,
}

impl Default for PointAttributes {
    fn default() -> Self {
        Self { width: 1.0, opacity: 1.0, drift: 1.0 }
    }
}

impl PointAttributes {
    fn interpolate(self, other: Self, t: f32, width_t: f32) -> Self {
        Self {
            width: lerp(nonnegative(self.width), nonnegative(other.width), width_t),
            opacity: lerp(unit(self.opacity), unit(other.opacity), t),
            drift: lerp(finite(self.drift, 0.0), finite(other.drift, 0.0), t),
        }
    }
}

/// Handles are absolute coordinates in the same local space as the anchor.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct StrokePoint {
    pub pos: Vec2,
    pub in_handle: Vec2,
    pub out_handle: Vec2,
    pub width: f32,
    pub opacity: f32,
    pub drift: f32,
}

impl StrokePoint {
    pub fn at(pos: Vec2, attrs: PointAttributes) -> Self {
        Self { pos, in_handle: pos, out_handle: pos, width: attrs.width, opacity: attrs.opacity, drift: attrs.drift }
    }

    pub fn attributes(self) -> PointAttributes {
        PointAttributes { width: self.width, opacity: self.opacity, drift: self.drift }
    }

    pub fn is_valid(self) -> bool {
        self.pos.is_finite()
            && self.in_handle.is_finite()
            && self.out_handle.is_finite()
            && self.width.is_finite()
            && self.width >= 0.0
            && (0.0..=1.0).contains(&self.opacity)
            && self.drift.is_finite()
    }
}

/// Standalone stroke input. AniCraft shapes keep geometry in the shared path table and
/// serialize their other channels through [BrushStroke].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    pub points: Vec<StrokePoint>,
    pub closed: bool,
    pub brush: BrushId,
    pub color: Rgba,
    pub seed: u64,
}

impl Stroke {
    pub fn path(&self) -> Path {
        let Some(first) = self.points.first() else { return Path::new() };
        let mut path = Path::new();
        path.move_to(first.pos);
        let mut edge = |a: &StrokePoint, b: &StrokePoint| {
            if a.out_handle == a.pos && b.in_handle == b.pos {
                path.line_to(b.pos);
            } else {
                path.cubic_to(a.out_handle, b.in_handle, b.pos);
            }
        };
        for pair in self.points.windows(2) {
            if let [a, b] = pair {
                edge(a, b);
            }
        }
        if self.closed {
            if let Some(last) = self.points.last().filter(|p| p.out_handle != p.pos || first.in_handle != first.pos) {
                edge(last, first);
            }
            path.close();
        }
        path
    }

    pub fn style(&self) -> BrushStroke {
        BrushStroke { brush: self.brush, color: self.color, seed: self.seed, points: self.points.iter().map(|p| p.attributes()).collect() }
    }
}

/// Serialized with a shape. One entry per Bézier anchor, in path order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrushStroke {
    pub brush: BrushId,
    pub color: Rgba,
    pub seed: u64,
    pub points: Vec<PointAttributes>,
}

impl BrushStroke {
    pub fn tween(&self, other: &Self, t: f32) -> Option<Self> {
        if self.brush != other.brush || self.points.len() != other.points.len() {
            return None;
        }
        Some(Self {
            color: self.color.lerp(other.color, t),
            points: self.points.iter().zip(&other.points).map(|(&a, &b)| a.interpolate(b, t, t)).collect(),
            ..self.clone()
        })
    }

    /// Channels on an edge ending at command index `cmd`. An explicit closing
    /// edge shares its end with anchor zero.
    pub fn sample_channels(&self, cmd: usize, t: f32, closing: bool) -> PointAttributes {
        let get = |i| self.points.get(i).or_else(|| self.points.last()).copied().unwrap_or_default();
        let a = get(cmd.saturating_sub(1));
        let b = get(if closing { 0 } else { cmd });
        a.interpolate(b, t, t * t * (3.0 - 2.0 * t))
    }

    fn max_width(&self) -> f32 {
        self.points.iter().map(|p| nonnegative(p.width)).fold(0.0, f32::max)
    }

    pub fn reach(&self, brush: &BrushDef) -> f32 {
        let width = self.max_width();
        let diameter = width * positive(brush.size_scale) * (1.0 + unit(brush.size_jitter));
        let aspect = positive(brush.aspect).sqrt();
        diameter * 0.5 * aspect.hypot(aspect.recip()) + width * nonnegative(brush.scatter).min(8.0)
    }

    /// Vector outline used for draft previews and centerline picking.
    pub fn outline(&self, path: &Path, brush: &BrushDef) -> Option<crate::Stroke> {
        use crate::width::{WidthPoint, WidthProfile};
        let width = self.max_width();
        if width <= 0.0 {
            return None;
        }
        let profile = self.points.iter().any(|p| p.width != width).then(|| CurveWalk::adaptive(path, 0.025)).flatten().and_then(|walk| {
            let points = self
                .points
                .iter()
                .enumerate()
                .filter_map(|(cmd, p)| {
                    let factor = nonnegative(p.width) / width;
                    Some(WidthPoint::new(walk.anchor_distance(cmd)? / walk.length(), factor, factor))
                })
                .collect();
            WidthProfile::new(points)
        });
        Some(crate::Stroke::new(self.color, width * positive(brush.size_scale)).with_profile(profile))
    }
}

/// Static tip definition and evaluated per-point channels passed to a renderer.
#[derive(Clone, Debug)]
pub struct EvaluatedBrush {
    pub style: Arc<BrushStroke>,
    pub definition: Arc<BrushDef>,
}

/// Derived render data. Always regenerate stamps after evaluating the path and channels.
/// Never serialize this list or its composited pixels with the document.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stamp {
    pub pos: Vec2,
    pub rotation: f32,
    /// Full width and height of the local quad.
    pub scale: Vec2,
    pub tip: u16,
    pub alpha: f32,
    pub color: Rgba,
}

#[derive(Clone, Copy, Debug)]
pub struct StampContext {
    pub zoom: f32,
    /// Frame time. Channels and deformation must already have been evaluated at this time.
    /// Random variation never depends on time.
    pub time: f32,
    /// Target deviation in screen pixels.
    pub flattening_tolerance: f32,
}

impl Default for StampContext {
    fn default() -> Self {
        Self { zoom: 1.0, time: 0.0, flattening_tolerance: 0.1 }
    }
}

impl StampContext {
    /// Quantizes zoom in powers of two so adjacent tiles and similar zoom levels share stamps.
    pub fn zoom_bucket(zoom: f32) -> i8 {
        positive(zoom).log2().ceil().clamp(-12.0, 12.0) as i8
    }

    fn tolerance(self) -> f32 {
        (positive(self.flattening_tolerance) / 2.0f32.powi(i32::from(Self::zoom_bucket(self.zoom)))).clamp(0.0001, 0.025)
    }
}

/// Derives a bounded stamp list from evaluated geometry and channels.
pub fn generate_stamps(stroke: &Stroke, brush: &BrushDef, ctx: &StampContext) -> Vec<Stamp> {
    if stroke.points.len() > MAX_POINTS || !stroke.points.iter().all(|p| p.is_valid()) {
        return Vec::new();
    }
    generate_for_path(&stroke.path(), &stroke.style(), brush, ctx)
}

/// Native shape entry point. The path has already been tweened and deformed.
pub fn generate_for_path(path: &Path, style: &BrushStroke, brush: &BrushDef, ctx: &StampContext) -> Vec<Stamp> {
    let Some(&PathCmd::MoveTo(start)) = path.cmds.first() else { return Vec::new() };
    if path.cmds.len() > MAX_POINTS + 2
        || style.points.is_empty()
        || style.brush != brush.id
        || !brush.valid_tips()
        || !start.is_finite()
        || path.cmds.iter().skip(1).any(|c| matches!(c, PathCmd::MoveTo(_)))
    {
        return Vec::new();
    }
    let first = style.points.first().copied().unwrap_or_default();
    let make = StampMaker { style, brush, color: color::Drift::new(style.color, brush.color_drift) };
    let Some(walk) = CurveWalk::adaptive(path, ctx.tolerance()) else {
        return make.at(0, 0.0, start, Vec2::new(1.0, 0.0), first).into_iter().collect();
    };
    let closed = matches!(path.cmds.last(), Some(PathCmd::Close));
    let length = walk.length();
    let min_step = (length / (MAX_STAMPS - 2) as f32).max(positive(brush.min_spacing_world));
    let spacing = finite(brush.spacing, 0.2).clamp(0.001, 8.0);
    let mut result = Vec::new();
    let mut s = 0.0f32;
    for i in 0..MAX_STAMPS {
        let (pos, tangent, command, t) = walk.sample(if closed && s == 0.0 { length } else { s });
        let a = if s == 0.0 {
            first
        } else if !closed && s == length {
            style.points.last().copied().unwrap_or(first)
        } else {
            let closing = closed && (command >= style.points.len() || path.cmds.get(command) == Some(&PathCmd::Close));
            style.sample_channels(command, t, closing)
        };
        if let Some(stamp) = make.at(i, s, if s == 0.0 { start } else { pos }, tangent, a) {
            result.push(stamp);
        }
        if s >= length {
            break;
        }
        let step = (nonnegative(a.width) * positive(brush.size_scale) * spacing).max(min_step);
        if closed && s + step >= length - (length * f32::EPSILON * 4.0) {
            break;
        }
        s = if !closed && i == MAX_STAMPS - 2 { length } else { (s + step).min(length) };
    }
    result
}

struct StampMaker<'a> {
    style: &'a BrushStroke,
    brush: &'a BrushDef,
    color: color::Drift,
}

impl StampMaker<'_> {
    fn at(&self, index: usize, s: f32, pos: Vec2, tangent: Vec2, a: PointAttributes) -> Option<Stamp> {
        let b = self.brush;
        let width = nonnegative(a.width);
        let d = width * positive(b.size_scale);
        if d <= 0.0 || !d.is_finite() {
            return None;
        }
        let seed = hash(self.style.seed.wrapping_add((index as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)));
        let random = |channel| signed(hash(seed ^ channel));
        let angle = match b.angle_mode {
            AngleMode::FollowPath => tangent.y.atan2(tangent.x),
            AngleMode::Fixed(a) => finite(a, 0.0),
            AngleMode::Random => random(1) * std::f32::consts::PI,
        };
        let rotation = angle + finite(b.angle_offset, 0.0) + nonnegative(b.angle_jitter).min(32.0) * random(2);
        let pos = pos + Vec2::new(-tangent.y, tangent.x) * (random(3) * nonnegative(b.scatter).min(8.0) * width);
        let diameter = d * (1.0 + unit(b.size_jitter) * random(4));
        let aspect = positive(b.aspect).sqrt();
        let scale = Vec2::new(diameter * aspect, diameter / aspect);
        let alpha = unit(unit(b.flow) * unit(a.opacity) * (1.0 + unit(b.opacity_jitter) * random(5)));
        let tip = match b.sequence {
            TipSequence::Fixed(i) => i,
            TipSequence::InOrder => (index % b.tips.len()) as u16,
            TipSequence::Random => (hash(seed ^ 6) % b.tips.len() as u64) as u16,
        };
        (alpha > 0.0 && rotation.is_finite() && scale.is_finite() && scale.x > 0.0 && scale.y > 0.0 && pos.is_finite()).then(|| Stamp {
            pos,
            rotation,
            scale,
            tip,
            alpha,
            color: self.color.at(self.style.seed, s, a.drift),
        })
    }
}

fn hash(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn signed(x: u64) -> f32 {
    (x >> 40) as f32 * (2.0 / 16_777_216.0) - 1.0
}

fn finite(v: f32, fallback: f32) -> f32 {
    if v.is_finite() { v } else { fallback }
}

fn nonnegative(v: f32) -> f32 {
    finite(v, 0.0).max(0.0)
}

fn positive(v: f32) -> f32 {
    finite(v, 1.0).clamp(0.0001, 1000.0)
}

fn unit(v: f32) -> f32 {
    finite(v, 0.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brush() -> BrushDef {
        BrushDef { spacing: 0.5, tips: vec![TipImage::new(1, 1, vec![255]).unwrap()], ..BrushDef::default() }
    }

    fn stroke(points: &[(f32, f32, f32)], closed: bool) -> Stroke {
        Stroke {
            points: points.iter().map(|&(x, y, width)| StrokePoint::at(Vec2::new(x, y), PointAttributes { width, ..Default::default() })).collect(),
            closed,
            brush: BrushId(0),
            color: Rgba::hex(0x8e3432),
            seed: 519,
        }
    }

    #[test]
    fn deterministic_across_time_and_serde_round_trips() {
        let st = stroke(&[(0.0, 0.0, 10.0), (27.0, 20.0, 18.0)], false);
        let mut b = brush();
        b.tips.push(TipImage::new(1, 1, vec![128]).unwrap());
        b.sequence = TipSequence::Random;
        b.angle_mode = AngleMode::Random;
        b.angle_jitter = 0.5;
        b.scatter = 0.2;
        b.size_jitter = 0.3;
        b.opacity_jitter = 0.6;
        b.color_drift = ColorDrift { hue: 0.7, chroma: 0.03, lightness: 0.1, frequency: 0.07 };
        let expected = generate_stamps(&st, &b, &StampContext::default());
        let copy: Stroke = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        let def: BrushDef = serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(expected, generate_stamps(&copy, &def, &StampContext { time: 97.0, ..Default::default() }));
        let shifted = Stroke { seed: st.seed + 1, ..st };
        assert_ne!(expected, generate_stamps(&shifted, &b, &StampContext::default()));
        assert!(expected.iter().any(|s| s.color != shifted.color));
    }

    #[test]
    fn spacing_is_by_arc_length_and_includes_open_endpoint() {
        let st = stroke(&[(0.0, 0.0, 10.0), (23.0, 0.0, 10.0)], false);
        let b = brush();
        let stamps = generate_stamps(&st, &b, &StampContext::default());
        assert_eq!(stamps, generate_stamps(&st, &b, &StampContext { zoom: 16.0, ..Default::default() }));
        assert_eq!(stamps.len(), 6);
        for (s, x) in stamps.iter().zip([0.0, 5.0, 10.0, 15.0, 20.0, 23.0]) {
            assert!((s.pos.x - x).abs() < 0.0001);
        }
    }

    #[test]
    fn width_eases_between_anchors_and_opacity_is_linear() {
        let mut st = stroke(&[(0.0, 0.0, 2.0), (20.0, 0.0, 18.0)], false);
        st.points[1].opacity = 0.4;
        let b = BrushDef { spacing: 0.01, min_spacing_world: 5.0, pressure_to_opacity: false, ..brush() };
        let stamps = generate_stamps(&st, &b, &StampContext::default());
        let expected = [(2.0, 1.0), (4.5, 0.85), (10.0, 0.7), (15.5, 0.55), (18.0, 0.4)];
        assert_eq!(stamps.len(), expected.len());
        for (s, (w, a)) in stamps.iter().zip(expected) {
            assert!((s.scale.x - w).abs() < 0.0001);
            assert!((s.alpha - a).abs() < 0.0001);
        }
    }

    #[test]
    fn closed_seam_uses_incoming_tangent_without_a_duplicate() {
        let st = stroke(&[(0.0, 0.0, 20.0), (10.0, 0.0, 20.0), (10.0, 10.0, 20.0), (0.0, 10.0, 20.0)], true);
        let result = generate_stamps(&st, &brush(), &StampContext::default());
        assert_eq!(result.len(), 4);
        assert_eq!(result[0].pos, Vec2::ZERO);
        assert!((result[0].rotation + std::f32::consts::FRAC_PI_2).abs() < 0.0001);
        assert_eq!(result[1].rotation, 0.0);
        assert_ne!(result[3].pos, result[0].pos);
    }

    #[test]
    fn zero_length_stroke_emits_one_dab_and_zero_width_emits_none() {
        let b = brush();
        let ctx = StampContext::default();
        let dot = stroke(&[(7.0, 3.0, 10.0)], false);
        let repeated = stroke(&[(7.0, 3.0, 10.0), (7.0, 3.0, 10.0)], false);
        for st in [dot, repeated] {
            let result = generate_stamps(&st, &b, &ctx);
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].pos, Vec2::new(7.0, 3.0));
        }
        let taper = stroke(&[(0.0, 0.0, 0.0), (5.0, 0.0, 0.0), (10.0, 0.0, 5.0)], false);
        let stamps = generate_stamps(&taper, &b, &ctx);
        assert!(stamps.first().is_some_and(|s| s.pos.x > 5.0));
        assert!(generate_stamps(&stroke(&[], false), &b, &ctx).is_empty());
    }

    #[test]
    fn long_strokes_widen_spacing_and_reach_the_end() {
        let st = stroke(&[(0.0, 0.0, 1.0), (1_000_000.0, 0.0, 1.0)], false);
        let result = generate_stamps(&st, &brush(), &StampContext::default());
        assert!(result.len() <= MAX_STAMPS);
        assert_eq!(result.last().unwrap().pos, Vec2::new(1_000_000.0, 0.0));
        assert!(result.windows(2).all(|pair| pair[0].pos.x < pair[1].pos.x));
    }
}
