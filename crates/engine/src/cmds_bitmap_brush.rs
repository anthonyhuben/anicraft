//! Document-local tip assets and commands for editable bitmap-tip strokes.

use crate::cmds_brush::{SIZE_RANGE, bounded_samples, brush_param, ranged};
use crate::cmds_draw::MAX_EXTENT;
use crate::params::*;
use crate::{CommandSpec, EngineError, Result, Session};
use anicraft_doc::bitmap_brush::{BrushDef, BrushId, BrushStroke, MAX_BRUSHES, MAX_POINTS, PointAttributes, Stroke, TipImage};
use anicraft_doc::{Element, ElementId, ElementKind, Shape};
use anicraft_geom::{Affine, Path, PathCmd, lerp, walk::CurveWalk};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn specs() -> Vec<CommandSpec> {
    let c = |id, label, params, run| CommandSpec { id, label, params, menu: "", mutates: true, shortcut: None, run };
    vec![
        c(
            "file.import_brush_tip",
            "Import Bitmap Brush Tip…",
            "{path (grayscale PNG up to 256×256), settings? (BrushDef fields, including name)}",
            import_tip as crate::RunFn,
        ),
        c("brush.bitmap_add", "Add Bitmap Brush", "{definition: BrushDef (1–32 R8 tips)}", add_brush),
        c(
            "draw.bitmap_brush",
            "Draw Bitmap Brush Stroke",
            "{brush, anchors?: [StrokePoint (absolute coordinates), …], points?: pointer samples as in draw.brush; closed?, color?, seed?, size?, curve?, pressure_size?, pressure_opacity?, smoothing?, unit?}",
            draw,
        ),
        c(
            "stroke.bitmap_channels",
            "Edit Bitmap Brush Point",
            "{element, point (zero-based anchor index), layer?, width?, opacity?, drift?, color?} edits the current keyframe. Classic tweens interpolate matching brush anchors.",
            set_channels,
        ),
    ]
}

fn decode<T: for<'a> Deserialize<'a>>(params: &Value, key: &str) -> Result<T> {
    T::deserialize(params.get(key).unwrap_or(&Value::Null)).map_err(|e| EngineError::Param(format!("invalid '{key}': {e}")))
}

fn import_tip(s: &mut Session, p: &Value) -> Result<Value> {
    let path = std::path::Path::new(str_req(p, "path")?);
    if crate::vfs::len(path)? > 2 * 1024 * 1024 {
        return Err(EngineError::Param("the brush tip PNG exceeds 2 MiB".into()));
    }
    let tip =
        TipImage::from_png(&crate::vfs::read(path)?).ok_or_else(|| EngineError::Param("choose a readable PNG no larger than 256×256".into()))?;
    let mut def = if p.get("settings").is_some() { decode(p, "settings")? } else { BrushDef::default() };
    if def.name.trim().is_empty() {
        def.name = path.file_stem().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    }
    def.tips = vec![tip];
    insert_brush(s, def)
}

fn add_brush(s: &mut Session, p: &Value) -> Result<Value> {
    if p.get("definition")
        .and_then(|d| d.get("tips"))
        .and_then(Value::as_array)
        .is_none_or(|t| t.is_empty() || t.len() > anicraft_doc::bitmap_brush::MAX_TIPS)
    {
        return Err(EngineError::Param("a bitmap brush needs 1–32 tips".into()));
    }
    insert_brush(s, decode(p, "definition")?)
}

fn insert_brush(s: &mut Session, mut def: BrushDef) -> Result<Value> {
    if !def.valid_tips() {
        return Err(EngineError::Param("invalid tip sequence for this brush's tips".into()));
    }
    if s.doc.bitmap_brushes.len() >= MAX_BRUSHES {
        return Err(EngineError::State("too many bitmap brushes in this document".into()));
    }
    def.name = def.name.trim().chars().take(80).collect();
    if def.name.is_empty() {
        return Err(EngineError::Param("a bitmap brush needs a name".into()));
    }
    let id = s.doc.alloc_id();
    if s.doc.bitmap_brush(BrushId(id)).is_some() {
        return Err(EngineError::State("no free brush id".into()));
    }
    def.id = BrushId(id);
    s.doc.bitmap_brushes.push(Arc::new(def));
    Ok(json!({ "brush": id }))
}

fn req_id(p: &Value, key: &str) -> Result<u64> {
    opt_u64(p, key)?.ok_or_else(|| EngineError::Param(format!("missing '{key}'")))
}

fn draw(s: &mut Session, p: &Value) -> Result<Value> {
    let brush = BrushId(req_id(p, "brush")?);
    let definition = s.doc.bitmap_brush(brush).ok_or_else(|| EngineError::Param("no such bitmap brush in this document".into()))?;
    let color = opt_color(p, "color")?.unwrap_or(s.style.stroke);
    let id = s.doc.next_id.max(1);
    let seed = opt_u64(p, "seed")?.unwrap_or(id);
    let closed = opt_bool(p, "closed")?.unwrap_or(false);
    let (mut path, style) = if let Some(anchors) = p.get("anchors") {
        if anchors.as_array().is_none_or(|a| !(1..=MAX_POINTS).contains(&a.len())) {
            return Err(EngineError::Param("a bitmap brush stroke needs 1–4096 anchors".into()));
        }
        let stroke = Stroke { points: decode(p, "anchors")?, closed, brush, color, seed };
        let bounded = |v: anicraft_geom::Vec2| v.x.abs().max(v.y.abs()) <= MAX_EXTENT;
        if stroke.points.iter().any(|a| !a.is_valid() || !bounded(a.pos) || !bounded(a.in_handle) || !bounded(a.out_handle) || a.width > 4000.0) {
            return Err(EngineError::Param("invalid bitmap brush anchor or channel values".into()));
        }
        (stroke.path(), stroke.style())
    } else {
        let (mut path, points) = pointer_path(s, p, definition.pressure_to_opacity)?;
        if closed {
            path.close();
        }
        (path, BrushStroke { brush, color, seed, points })
    };
    let origin = path.bounds().center();
    path = path.transformed(&Affine::translate(-origin.x, -origin.y));
    let el = Element {
        id: ElementId(s.doc.alloc_id()),
        name: String::new(),
        transform: Affine::translate(origin.x, origin.y),
        color: Default::default(),
        kind: ElementKind::Shape(Shape {
            path: Arc::new(path),
            fill: None,
            stroke: None,
            fill_rule: Default::default(),
            merge: false,
            warp: None,
            bitmap_brush: Some(Arc::new(style)),
        }),
    };
    let kf = s.editable_keyframe()?;
    if kf.elements.len() >= 100_000 {
        return Err(EngineError::State("too many elements on this keyframe".into()));
    }
    kf.elements.push(Arc::new(el));
    s.select_only([ElementId(id)]);
    Ok(json!({ "element": id, "kind": "bitmap_brush" }))
}

fn pointer_path(s: &Session, p: &Value, pressure_to_opacity: bool) -> Result<(Path, Vec<PointAttributes>)> {
    use anicraft_geom::brush::{Sample, centre_stroke, size_factor};
    let mut samples = bounded_samples(p)?;
    // Keep both ends; bound the fit on very long pointer recordings.
    if samples.len() > MAX_POINTS {
        samples = (0..MAX_POINTS).filter_map(|i| samples.get(i * (samples.len() - 1) / (MAX_POINTS - 1)).copied()).collect();
    }
    let b = brush_param(s, p)?;
    let size = ranged(p, "size", SIZE_RANGE, b.size)?;
    let unit = ranged(p, "unit", 0.001..=1000.0, 1.0)?;
    let shaping = b.shaping();
    let shaped: Vec<Sample> = samples.iter().map(|x| Sample { pressure: b.size_pressure(x.pressure), ..*x }).collect();
    let (mut path, widths) = centre_stroke(&shaped, &shaping, unit * (0.3 + b.smoothing * 0.06), (unit * 0.09).min(size * 0.0025), 512);
    let Some(first) = samples.first() else { return Err(EngineError::Param("the stroke needs a point".into())) };
    let opacity = |p| if pressure_to_opacity && b.pressure_opacity { b.curve.map(p) } else { 1.0 };
    let Some(walk) = CurveWalk::adaptive(&path, 0.025).filter(|_| path.cmds.len() <= MAX_POINTS) else {
        return Ok((
            Path { cmds: vec![PathCmd::MoveTo(first.at)] },
            vec![PointAttributes { width: size * size_factor(b.size_pressure(first.pressure)), opacity: opacity(first.pressure), drift: 1.0 }],
        ));
    };
    let mut along = vec![(0.0, opacity(first.pressure))];
    let mut length = 0.0;
    for pair in samples.windows(2) {
        if let [a, b] = pair {
            length += a.at.distance(b.at);
            along.push((length, opacity(b.pressure)));
        }
    }
    let at = |table: &[(f32, f32)], s: f32| {
        let i = table.partition_point(|p| p.0 < s);
        let a = table.get(i.saturating_sub(1)).copied().unwrap_or((0.0, 1.0));
        let b = table.get(i).copied().unwrap_or(a);
        let t = if b.0 > a.0 { ((s - a.0) / (b.0 - a.0)).clamp(0.0, 1.0) } else { 0.0 };
        lerp(a.1, b.1, t)
    };
    // Preserve interior pressure changes even when the geometry fit needs only one cubic.
    let width_knots = crate::cmds_brush::simplify_profile(&widths, 0.025, 64);
    let opacity_samples: Vec<_> = (0..=128)
        .map(|i| {
            let u = i as f32 / 128.0;
            (u, at(&along, u * length))
        })
        .collect();
    let opacity_knots = crate::cmds_brush::simplify_profile(&opacity_samples, 0.025, 64);
    let mut knots: Vec<_> = width_knots.into_iter().chain(opacity_knots).map(|(u, _)| u).filter(|&u| u > 0.0 && u < 1.0).collect();
    knots.sort_by(f32::total_cmp);
    knots.dedup();
    let mut previous = (usize::MAX, 1.0);
    for u in knots.into_iter().rev() {
        let (cmd, t) = walk.locate(u * walk.length());
        let end = if previous.0 == cmd { previous.1 } else { 1.0 };
        if (0.005..0.995).contains(&(t / end)) && path.cmds.len() < MAX_POINTS && path.split_segment(cmd, t / end) {
            previous = (cmd, t);
        }
    }
    let measured = CurveWalk::adaptive(&path, 0.025).unwrap_or(walk);
    let points = path
        .cmds
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let t = measured.anchor_distance(i).unwrap_or(0.0) / measured.length();
            PointAttributes { width: size * at(&widths, t), opacity: at(&along, t * length), drift: 1.0 }
        })
        .collect();
    Ok((path, points))
}

fn set_channels(s: &mut Session, p: &Value) -> Result<Value> {
    let layer = opt_u64(p, "layer")?.map_or(s.layer, anicraft_doc::LayerId);
    s.with_layer(layer, |s| set_channels_on_layer(s, p))
}

fn set_channels_on_layer(s: &mut Session, p: &Value) -> Result<Value> {
    let id = ElementId(req_id(p, "element")?);
    let point = usize::try_from(req_id(p, "point")?).map_err(|_| EngineError::Param("point index is too large".into()))?;
    let kf = if s.active_layer().is_some_and(|l| l.kind == anicraft_doc::LayerKind::Armature) {
        s.editable_keyframe_in(crate::KeyEdit::Pose)?
    } else {
        s.editable_keyframe()?
    };
    let entry = kf.elements.iter_mut().find(|e| e.id == id).ok_or_else(|| EngineError::State("no such element on the active keyframe".into()))?;
    let ElementKind::Shape(shape) = &mut Arc::make_mut(entry).kind else {
        return Err(EngineError::State("the element is not a bitmap brush stroke".into()));
    };
    let style = shape.bitmap_brush.as_mut().ok_or_else(|| EngineError::State("the element is not a bitmap brush stroke".into()))?;
    let style = Arc::make_mut(style);
    if let Some(color) = opt_color(p, "color")? {
        style.color = color;
    }
    let a = style.points.get_mut(point).ok_or_else(|| EngineError::Param("no such brush anchor".into()))?;
    *a = PointAttributes {
        width: ranged(p, "width", 0.0..=4000.0, a.width)?,
        opacity: ranged(p, "opacity", 0.0..=1.0, a.opacity)?,
        drift: ranged(p, "drift", -16.0..=16.0, a.drift)?,
    };
    Ok(json!({ "element": id.0, "point": point, "channels": *a }))
}

/// Collects only the tip definitions reachable from the copied artwork and its symbols.
pub(crate) fn clipboard_brushes(s: &Session, elements: &[Element], symbols: &[Arc<anicraft_doc::Symbol>]) -> Vec<Arc<BrushDef>> {
    fn collect(e: &Element, ids: &mut std::collections::HashSet<BrushId>, depth: usize) {
        if depth > anicraft_doc::MAX_GROUP_DEPTH {
            return;
        }
        match &e.kind {
            ElementKind::Shape(sh) => {
                if let Some(brush) = &sh.bitmap_brush {
                    ids.insert(brush.brush);
                }
            }
            ElementKind::Group { .. } => {
                for child in e.kind.group_members() {
                    collect(&child, ids, depth + 1);
                }
            }
            _ => {}
        }
    }
    let mut ids = std::collections::HashSet::new();
    for element in elements.iter().chain(
        symbols.iter().flat_map(|sym| sym.timeline.layers.iter().flat_map(|l| l.keyframes.iter().flat_map(|k| k.elements.iter().map(Arc::as_ref)))),
    ) {
        collect(element, &mut ids, 0);
    }
    s.doc.bitmap_brushes.iter().filter(|b| ids.contains(&b.id)).cloned().collect()
}

pub(crate) type BrushRemap = std::collections::HashMap<BrushId, BrushId>;

pub(crate) fn import_clipboard_brushes(s: &mut Session, brushes: &[Arc<BrushDef>]) -> Result<BrushRemap> {
    let mut ids = BrushRemap::new();
    for b in brushes {
        if s.doc.bitmap_brush(b.id).is_some_and(|here| Arc::ptr_eq(here, b) || **here == **b) {
            continue;
        }
        // A second paste should share the first paste's definition, even after an id collision.
        let mut copy = (**b).clone();
        if let Some(here) = s.doc.bitmap_brushes.iter().find(|here| {
            copy.id = here.id;
            here.as_ref() == &copy
        }) {
            ids.insert(b.id, here.id);
            continue;
        }
        let v = insert_brush(s, copy)?;
        let id = BrushId(req_id(&v, "brush")?);
        ids.insert(b.id, id);
    }
    Ok(ids)
}

pub(crate) fn remap_element(e: &mut Element, ids: &BrushRemap, depth: usize) {
    if ids.is_empty() || depth > anicraft_doc::MAX_GROUP_DEPTH {
        return;
    }
    match &mut e.kind {
        ElementKind::Shape(sh) => {
            if let Some(brush) = &mut sh.bitmap_brush
                && let Some(id) = ids.get(&brush.brush)
            {
                Arc::make_mut(brush).brush = *id;
            }
        }
        ElementKind::Group { members } => remap_timeline(members, ids, depth + 1),
        _ => {}
    }
}

pub(crate) fn remap_timeline(tl: &mut anicraft_doc::Timeline, ids: &BrushRemap, depth: usize) {
    if ids.is_empty() || depth > anicraft_doc::MAX_GROUP_DEPTH {
        return;
    }
    for layer in &mut tl.layers {
        for key in &mut Arc::make_mut(layer).keyframes {
            for el in &mut Arc::make_mut(key).elements {
                remap_element(Arc::make_mut(el), ids, depth);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anicraft_doc::{Document, Rgba, evaluate};
    use anicraft_geom::Vec2;

    fn add(s: &mut Session, value: u8) -> BrushId {
        let b = BrushDef { name: "Chalk".into(), tips: vec![TipImage::new(1, 1, vec![value]).unwrap()], ..Default::default() };
        BrushId(s.execute("brush.bitmap_add", &json!({ "definition": b })).unwrap()["brush"].as_u64().unwrap())
    }

    fn line(s: &mut Session, id: BrushId) -> u64 {
        s.execute("draw.bitmap_brush", &json!({ "brush": id.0, "points": [[20, 30], [100, 30]], "size": 10, "color": "#c05040", "seed": 90 }))
            .unwrap()["element"]
            .as_u64()
            .unwrap()
    }

    #[test]
    fn bitmap_brush_tweens_channels_and_round_trips_without_derived_data() {
        let mut s = Session::new(Document::default());
        s.doc.stage.width = 128;
        s.doc.stage.height = 64;
        let brush = add(&mut s, 200);
        let element = line(&mut s, brush);
        s.execute("timeline.insert_keyframe", &json!({"frame": 10})).unwrap();
        s.set_frame(10);
        s.execute("stroke.bitmap_channels", &json!({"element": element, "point": 1, "width": 30, "opacity": 0.4, "drift": 3})).unwrap();
        s.set_frame(0);
        s.execute("timeline.create_tween", &json!({})).unwrap();
        let middle = evaluate(&s.doc, 5);
        let style = &middle.items[0].bitmap_brush.as_ref().unwrap().style;
        assert_eq!(style.seed, 90);
        assert_eq!(style.points[1].width, 20.0);
        assert!((style.points[1].opacity - 0.7).abs() < 1e-6);
        assert_eq!(style.points[1].drift, 2.0);

        let saved = crate::format::to_json(&s.doc).unwrap();
        assert_eq!(crate::format::version_for(&s.doc), 5);
        assert!(!saved.contains("stamps"));
        let loaded = crate::format::from_json(&saved).unwrap();
        for frame in [0, 5, 10] {
            let a = anicraft_render::render_frame(&s.doc, frame, 1.0).unwrap();
            let b = anicraft_render::render_frame(&loaded, frame, 1.0).unwrap();
            assert_eq!(a, b);
        }
        let view = anicraft_render::View::stage(&s.doc, 1.0);
        let full = crate::render_service::make_job(0, &s.doc, 5, view, None);
        let draft = full.clone().draft_brushes(true);
        assert_ne!(full.key, draft.key);
        assert!(full.display_list().items[0].bitmap_brush.is_some());
        assert!(draft.display_list().items[0].bitmap_brush.is_none());
    }

    #[test]
    fn bitmap_brush_copy_remaps_colliding_definition_ids() {
        let mut s = Session::new(Document::default());
        let id = add(&mut s, 222);
        line(&mut s, id);
        s.execute("edit.copy", &json!({})).unwrap();
        s.replace_document(Document::default(), None);
        assert_eq!(id, add(&mut s, 20));
        for _ in 0..2 {
            s.execute("edit.paste_in_place", &json!({})).unwrap();
        }
        assert_eq!(s.doc.bitmap_brushes.len(), 2);
        let dl = evaluate(&s.doc, 0);
        assert_eq!(dl.items.len(), 2);
        for item in dl.items {
            let brush = item.bitmap_brush.unwrap();
            assert_ne!(brush.definition.id, id);
            assert_eq!(brush.definition.tips[0].mask(), [222]);
            assert_eq!(brush.style.seed, 90);
        }
    }

    #[test]
    fn bitmap_brush_keeps_channels_when_its_skinned_centerline_is_edited_and_posed() {
        let mut s = Session::new(Document::default());
        let brush = add(&mut s, 255);
        let element = line(&mut s, brush);
        s.execute("stroke.bitmap_channels", &json!({"element": element, "point": 1, "width": 30})).unwrap();
        let bone = s.execute("bone.add_in_shape", &json!({"shape": element, "head": [22, 30], "tail": [95, 30]})).unwrap()["bone"].clone();
        s.execute("path.add_point", &json!({"id": element, "x": 60, "y": 30})).unwrap();
        let rest = evaluate(&s.doc, 0).items.remove(0);
        let points = &rest.bitmap_brush.as_ref().unwrap().style.points;
        assert_eq!(points.len(), 3);
        assert!((points[1].width - 20.0).abs() < 0.01);
        s.execute("bone.pose", &json!({"bone": bone, "angle": 35})).unwrap();
        let posed = evaluate(&s.doc, 0).items.remove(0);
        assert_ne!(rest.path, posed.path);
        assert_eq!(*points, posed.bitmap_brush.unwrap().style.points);
    }

    #[test]
    fn asset_warp_preserves_brush_anchor_count() {
        use anicraft_doc::bitmap_brush::StrokePoint;
        let mut s = Session::new(Document::default());
        let brush = add(&mut s, 255);
        let point = |x, y| StrokePoint::at(Vec2::new(x, y), PointAttributes { width: 12.0, ..Default::default() });
        let anchors = [point(20.0, 20.0), point(65.0, 60.0), point(100.0, 20.0)];
        let v = s.execute("draw.bitmap_brush", &json!({"brush": brush.0, "anchors": anchors})).unwrap();
        let element = &v["element"];
        let pin = s.execute("warp.pin_add", &json!({"element": element, "x": 65, "y": 55})).unwrap()["pin"].clone();
        s.execute("warp.pin_add", &json!({"element": element, "x": 25, "y": 25})).unwrap();
        let before = evaluate(&s.doc, 0).items.remove(0);
        s.execute("warp.pin_move", &json!({"element": element, "pin": pin, "dx": 0, "dy": 25})).unwrap();
        let after = evaluate(&s.doc, 0).items.remove(0);
        assert_ne!(before.path, after.path);
        assert_eq!(before.path.cmds.len(), after.path.cmds.len());
        assert_eq!(after.bitmap_brush.unwrap().style.points.len(), 3);
    }

    #[test]
    fn bitmap_brush_dab_is_drawn_and_pickable() {
        let mut s = Session::new(Document::default());
        s.doc.stage.width = 60;
        s.doc.stage.height = 60;
        let id = add(&mut s, 255);
        s.execute("draw.bitmap_brush", &json!({"brush": id.0, "points": [[30, 30]], "size": 10, "color": "#ff0000"})).unwrap();
        let picture = anicraft_render::render_frame(&s.doc, 0, 1.0).unwrap();
        assert_eq!(picture.pixel(30, 30), Some([1.0, 0.0, 0.0, 1.0]));
        assert!(s.hit_test(Vec2::new(30.0, 30.0), 0.0).is_some());
        s.doc.stage.background = Rgba::TRANSPARENT;
        let view = anicraft_render::View::stage(&s.doc, 1.0);
        let draft = crate::render_service::make_job(0, &s.doc, 0, view, None).draft_brushes(true);
        let (picture, _) = anicraft_render::render(&draft.display_list(), &view).unwrap();
        assert_eq!(picture.pixel(30, 30), Some([1.0, 0.0, 0.0, 1.0]));
    }
}
