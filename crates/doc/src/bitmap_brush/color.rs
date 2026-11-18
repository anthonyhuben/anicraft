//! OKLCH variation using Björn Ottosson's published Oklab matrices.

use super::{ColorDrift, finite, hash, signed, unit};
use crate::Rgba;
use anicraft_geom::lerp;

pub(super) struct Drift {
    original: Rgba,
    lch: [f32; 3],
    amounts: ColorDrift,
}

impl Drift {
    pub(super) fn new(original: Rgba, amounts: ColorDrift) -> Self {
        let linear = |v: f32| {
            let v = unit(v);
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        let rgb = [linear(original.r), linear(original.g), linear(original.b)];
        let lms = matrix(
            rgb,
            [[0.412_221_46, 0.536_332_55, 0.051_445_995], [0.211_903_5, 0.680_699_5, 0.107_396_96], [0.088_302_46, 0.281_718_85, 0.629_978_7]],
        )
        .map(f32::cbrt);
        let [l, a, b] = matrix(
            lms,
            [[0.210_454_26, 0.793_617_8, -0.004_072_047], [1.977_998_5, -2.428_592_2, 0.450_593_7], [0.025_904_037, 0.782_771_77, -0.808_675_77]],
        );
        Self { original, lch: [l, a.hypot(b), b.atan2(a)], amounts }
    }

    pub(super) fn at(&self, seed: u64, s: f32, multiplier: f32) -> Rgba {
        let d = self.amounts;
        if multiplier == 0.0 || d.frequency <= 0.0 || !d.frequency.is_finite() || (d.hue == 0.0 && d.chroma == 0.0 && d.lightness == 0.0) {
            return self.original;
        }
        let x = (f64::from(s) * f64::from(d.frequency)).min((1u64 << 50) as f64);
        let cell = x.floor() as u64;
        let f = x.fract() as f32;
        let fade = f * f * (3.0 - 2.0 * f);
        let noise = |channel| {
            let at = |i: u64| signed(hash(seed ^ hash(i.wrapping_mul(3).wrapping_add(channel))));
            lerp(at(cell), at(cell + 1), fade) * finite(multiplier, 0.0).clamp(-16.0, 16.0)
        };
        let [l, c, h] = self.lch;
        let h = h + finite(d.hue, 0.0).clamp(-32.0, 32.0) * noise(2);
        let c = (c + finite(d.chroma, 0.0) * noise(1)).clamp(0.0, 0.5);
        let l = unit(l + finite(d.lightness, 0.0) * noise(0));
        let lab = [l, c * h.cos(), c * h.sin()];
        let lms = matrix(lab, [[1.0, 0.396_337_78, 0.215_803_76], [1.0, -0.105_561_346, -0.063_854_17], [1.0, -0.089_484_18, -1.291_485_5]])
            .map(|v| v * v * v);
        let [r, g, b] = matrix(
            lms,
            [[4.076_741_7, -3.307_711_6, 0.230_969_94], [-1.268_438, 2.609_757_4, -0.341_319_38], [-0.004_196_086_3, -0.703_418_6, 1.707_614_7]],
        )
        .map(|v| {
            let v = unit(v);
            if v <= 0.003_130_8 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
        });
        Rgba { r, g, b, a: self.original.a }
    }
}

fn matrix(v: [f32; 3], m: [[f32; 3]; 3]) -> [f32; 3] {
    let [x, y, z] = v;
    m.map(|[a, b, c]| a * x + b * y + c * z)
}
