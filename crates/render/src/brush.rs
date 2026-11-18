//! Transient stamp preparation and the CPU reference for bitmap-tip strokes.

use crate::paint::{Premul, premultiply};
use crate::raster::IRect;
use anicraft_doc::bitmap_brush::{BrushDef, BrushStroke, EvaluatedBrush, Stamp, StampBlend, StampContext, TipImage, generate_for_path};
use anicraft_doc::{ColorTransform, DisplayItem};
use anicraft_geom::{Affine, Path, Vec2, lerp};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// A backend draws one prepared stroke into its own transparent surface. The caller then
/// composites that surface once, with the item's regular mask, group, and filter semantics.
pub trait StampRenderer<T: ?Sized> {
    type Error;

    fn draw(&mut self, stroke: &BrushLayer, target: &mut T) -> Result<(), Self::Error>;
}

/// R8 texture-array data, indexed by tip then mip. Both backends use the same bytes.
pub struct TipAtlas {
    pub width: u32,
    pub height: u32,
    pub tips: Vec<Vec<Vec<u8>>>,
}

impl TipAtlas {
    fn new(tips: &[TipImage]) -> Self {
        let w = tips.iter().map(|t| t.dimensions().0).max().unwrap_or(1).next_power_of_two();
        let h = tips.iter().map(|t| t.dimensions().1).max().unwrap_or(1).next_power_of_two();
        let tips = tips
            .iter()
            .map(|tip| {
                let (tw, th) = tip.dimensions();
                let mut level = if (w, h) == (tw, th) {
                    tip.mask().to_vec()
                } else {
                    (0..h)
                        .flat_map(|y| {
                            (0..w).map(move |x| {
                                let uv = Vec2::new((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
                                (bilinear(tip.mask(), tw, th, uv) * 255.0).round() as u8
                            })
                        })
                        .collect()
                };
                let (mut pw, mut ph) = (w, h);
                let mut chain = Vec::new();
                while pw > 1 || ph > 1 {
                    let (nw, nh) = ((pw / 2).max(1), (ph / 2).max(1));
                    let mut next = Vec::with_capacity((nw * nh) as usize);
                    for y in 0..nh {
                        for x in 0..nw {
                            let x0 = (x * 2).min(pw - 1);
                            let y0 = (y * 2).min(ph - 1);
                            let x1 = (x0 + 1).min(pw - 1);
                            let y1 = (y0 + 1).min(ph - 1);
                            let get = |x, y| u32::from(level.get((y * pw + x) as usize).copied().unwrap_or(0));
                            next.push(((get(x0, y0) + get(x1, y0) + get(x0, y1) + get(x1, y1) + 2) / 4) as u8);
                        }
                    }
                    chain.push(level);
                    level = next;
                    (pw, ph) = (nw, nh);
                }
                chain.push(level);
                chain
            })
            .collect();
        Self { width: w, height: h, tips }
    }

    pub fn level_count(&self) -> u32 {
        self.width.max(self.height).ilog2() + 1
    }

    pub fn level_size(&self, level: u32) -> (u32, u32) {
        (self.width.checked_shr(level).unwrap_or(0).max(1), self.height.checked_shr(level).unwrap_or(0).max(1))
    }

    fn sample(&self, tip: u16, lod: f32, uv: Vec2) -> f32 {
        let Some(tip) = self.tips.get(usize::from(tip)) else { return 0.0 };
        let i = lod.floor() as u32;
        let j = (i + 1).min(self.level_count() - 1);
        let sample = |n| {
            let (w, h) = self.level_size(n);
            tip.get(n as usize).map_or(0.0, |data| bilinear(data, w, h, uv))
        };
        let a = sample(i);
        if i == j || lod.fract() == 0.0 { a } else { lerp(a, sample(j), lod.fract()) }
    }
}

fn bilinear(data: &[u8], w: u32, h: u32, uv: Vec2) -> f32 {
    let x = uv.x * w as f32 - 0.5;
    let y = uv.y * h as f32 - 0.5;
    let (ix, iy) = (x.floor() as i32, y.floor() as i32);
    let get = |x: i32, y: i32| {
        let x = x.clamp(0, w as i32 - 1) as usize;
        let y = y.clamp(0, h as i32 - 1) as usize;
        f32::from(data.get(y * w as usize + x).copied().unwrap_or(0)) / 255.0
    };
    let fx = x - ix as f32;
    let fy = y - iy as f32;
    lerp(lerp(get(ix, iy), get(ix + 1, iy), fx), lerp(get(ix, iy + 1), get(ix + 1, iy + 1), fx), fy)
}

struct StampEntry {
    path: Weak<Path>,
    style: Weak<BrushStroke>,
    brush: Weak<BrushDef>,
    zoom: i8,
    stamps: Arc<OnceLock<Vec<Stamp>>>,
}

static STAMPS: Mutex<VecDeque<StampEntry>> = Mutex::new(VecDeque::new());
static ATLASES: Mutex<VecDeque<(Weak<BrushDef>, Arc<TipAtlas>)>> = Mutex::new(VecDeque::new());

fn atlas(brush: &Arc<BrushDef>) -> Arc<TipAtlas> {
    let key = Arc::downgrade(brush);
    let mut cache = ATLASES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(i) = cache.iter().position(|(b, _)| b.ptr_eq(&key))
        && let Some(entry) = cache.remove(i)
    {
        let atlas = entry.1.clone();
        cache.push_front(entry);
        return atlas;
    }
    let atlas = Arc::new(TipAtlas::new(&brush.tips));
    cache.truncate(7);
    cache.push_front((key, atlas.clone()));
    atlas
}

fn stamps(path: &Arc<Path>, brush: &EvaluatedBrush, ctx: StampContext) -> Arc<OnceLock<Vec<Stamp>>> {
    // Copy-on-write path/style/definition identities are their versions. Evaluated poses
    // or interpolated attributes get new identities. A weak reference prevents key reuse.
    let (path, style, definition) = (Arc::downgrade(path), Arc::downgrade(&brush.style), Arc::downgrade(&brush.definition));
    let zoom = StampContext::zoom_bucket(ctx.zoom);
    let mut cache = STAMPS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(i) = cache.iter().position(|e| e.zoom == zoom && e.path.ptr_eq(&path) && e.style.ptr_eq(&style) && e.brush.ptr_eq(&definition))
        && let Some(entry) = cache.remove(i)
    {
        let stamps = entry.stamps.clone();
        cache.push_front(entry);
        return stamps;
    }
    let stamps = Arc::new(OnceLock::new());
    cache.truncate(63);
    cache.push_front(StampEntry { path, style, brush: definition, zoom, stamps: stamps.clone() });
    stamps
}

/// One instance in surface space. The transform maps the unit quad (UV 0..1) to pixels.
pub struct BrushInstance {
    pub transform: Affine,
    pub color: Premul,
    pub tip: u16,
    pub lod: f32,
    inverse: Affine,
    bounds: IRect,
}

/// A stroke's visible instances and its clipped surface bounds.
pub struct BrushLayer {
    pub atlas: Arc<TipAtlas>,
    pub blend: StampBlend,
    pub bounds: IRect,
    pub instances: Vec<BrushInstance>,
}

impl BrushLayer {
    pub(crate) fn from_item(item: &DisplayItem, m: &Affine, surface: IRect) -> Option<Self> {
        let brush = item.bitmap_brush.as_ref()?;
        let ctx = StampContext {
            zoom: m.apply_vector(Vec2::new(1.0, 0.0)).length().max(m.apply_vector(Vec2::new(0.0, 1.0)).length()),
            ..Default::default()
        };
        let cached = stamps(&item.path, brush, ctx);
        let stamps = cached.get_or_init(|| generate_for_path(&item.path, &brush.style, &brush.definition, &ctx));
        Self::new(stamps, &brush.definition, m, item.color, surface)
    }

    /// Prepares a standalone list for either backend.
    pub fn new(stamps: &[Stamp], brush: &Arc<BrushDef>, m: &Affine, effect: ColorTransform, surface: IRect) -> Option<Self> {
        if !brush.valid_tips() || !m.is_finite() {
            return None;
        }
        let atlas = atlas(brush);
        let mut bounds = crate::NO_PIXELS;
        let mut instances = Vec::new();
        for s in stamps.iter().take(anicraft_doc::bitmap_brush::MAX_STAMPS) {
            if usize::from(s.tip) >= atlas.tips.len() || !s.alpha.is_finite() {
                continue;
            }
            let transform = m
                .mul(&Affine::translate(s.pos.x, s.pos.y))
                .mul(&Affine::rotate(s.rotation))
                .mul(&Affine::scale(s.scale.x, s.scale.y))
                .mul(&Affine::translate(-0.5, -0.5));
            if !transform.is_finite() {
                continue;
            }
            let Some(inverse) = transform.inverse() else { continue };
            let r = anicraft_geom::Rect::new(0.0, 0.0, 1.0, 1.0).transformed(&transform);
            let b = IRect { x0: r.min.x.floor() as i32, y0: r.min.y.floor() as i32, x1: r.max.x.ceil() as i32, y1: r.max.y.ceil() as i32 }
                .intersect(&surface);
            if b.is_empty() {
                continue;
            }
            let [a, b0, c, d, _, _] = inverse.m;
            let (tw, th) = (atlas.width as f32, atlas.height as f32);
            let lod = ((a * tw).hypot(b0 * th).max((c * tw).hypot(d * th))).log2().clamp(0.0, (atlas.level_count() - 1) as f32);
            let mut color = effect.apply(s.color);
            color.a *= s.alpha.clamp(0.0, 1.0);
            if color.a > 0.0 {
                bounds = bounds.union(&b);
                instances.push(BrushInstance { transform, color: premultiply(color), tip: s.tip, lod, inverse, bounds: b });
            }
        }
        (!instances.is_empty()).then_some(Self { atlas, blend: brush.blend, bounds, instances })
    }
}

/// The rectangle covered by the supplied slice, in surface coordinates.
pub struct CpuStampRenderer(pub IRect);

impl StampRenderer<[Premul]> for CpuStampRenderer {
    type Error = std::convert::Infallible;

    fn draw(&mut self, stroke: &BrushLayer, target: &mut [Premul]) -> Result<(), Self::Error> {
        let dst = self.0;
        let w = dst.width();
        for s in &stroke.instances {
            let b = dst.intersect(&s.bounds);
            for y in b.y0..b.y1 {
                let start = (y - dst.y0) as usize * w + (b.x0 - dst.x0) as usize;
                let Some(row) = target.get_mut(start..start + b.width()) else { continue };
                for (x, pixel) in (b.x0..b.x1).zip(row) {
                    let uv = s.inverse.apply(Vec2::new(x as f32 + 0.5, y as f32 + 0.5));
                    if !(0.0..1.0).contains(&uv.x) || !(0.0..1.0).contains(&uv.y) {
                        continue;
                    }
                    let k = stroke.atlas.sample(s.tip, s.lod, uv);
                    if k <= 0.0 {
                        continue;
                    }
                    let src = s.color.map(|c| c * k);
                    match stroke.blend {
                        StampBlend::Accumulate => over(pixel, src),
                        StampBlend::MaxPerStroke if src[3] >= pixel[3] => *pixel = src,
                        StampBlend::MaxPerStroke => {}
                    }
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn over(dst: &mut Premul, src: Premul) {
    let inv = 1.0 - src[3];
    for (d, s) in dst.iter_mut().zip(src) {
        *d = s + *d * inv;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anicraft_doc::{Rgba, bitmap_brush::TipImage};

    #[test]
    fn strongest_stamp_keeps_its_color_while_over_builds_density() {
        let b = IRect { x0: 0, y0: 0, x1: 4, y1: 4 };
        let base = Stamp { pos: Vec2::new(2.0, 2.0), rotation: 0.0, scale: Vec2::new(4.0, 4.0), tip: 0, alpha: 0.5, color: Rgba::hex(0xff0000) };
        let stamps = [base, Stamp { alpha: 0.25, color: Rgba::hex(0x0000ff), ..base }];
        for (blend, expected) in [(StampBlend::Accumulate, [0.375, 0.0, 0.25, 0.625]), (StampBlend::MaxPerStroke, [0.5, 0.0, 0.0, 0.5])] {
            let brush = Arc::new(BrushDef { tips: vec![TipImage::new(1, 1, vec![255]).unwrap()], blend, ..Default::default() });
            let layer = BrushLayer::new(&stamps, &brush, &Affine::IDENTITY, ColorTransform::IDENTITY, b).unwrap();
            let mut out = [[0.0; 4]; 16];
            CpuStampRenderer(b).draw(&layer, &mut out).unwrap();
            assert!(out.iter().all(|&p| p == expected));
        }
    }

    #[test]
    fn minified_tips_use_a_shared_mipmap() {
        let atlas = TipAtlas::new(&[TipImage::new(4, 4, [0, 255, 255, 0].repeat(4)).unwrap()]);
        assert_eq!(atlas.level_count(), 3);
        assert_eq!(atlas.tips[0][2], [128]);
        assert_eq!(atlas.sample(0, 2.0, Vec2::ZERO), 128.0 / 255.0);
    }
}
