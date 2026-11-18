//! Run with a grayscale PNG (white deposits pigment, black is transparent):
//! cargo run -p anicraft-engine --example bitmap_brush -- tip.png output-directory

use anicraft_doc::bitmap_brush::{PointAttributes, StrokePoint};
use anicraft_engine::Session;
use anicraft_geom::Vec2;
use serde_json::json;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let tip = args.next().ok_or("usage: bitmap_brush TIP.png OUTPUT_DIRECTORY")?;
    let out = std::path::PathBuf::from(args.next().ok_or("missing output directory")?);
    std::fs::create_dir_all(&out)?;

    let mut session = Session::new(Default::default());
    session.doc.stage.width = 800;
    session.doc.stage.height = 320;
    session.doc.stage.background = anicraft_doc::Rgba::hex(0xf5efe5);
    let brush = session.execute(
        "file.import_brush_tip",
        &json!({
            "path": tip.to_string_lossy(),
            "settings": {
                "name": "Dry pigment", "spacing": 0.19, "flow": 0.55,
                "aspect": 1.2, "scatter": 0.11, "size_jitter": 0.19, "angle_jitter": 0.4,
                "color_drift": {"lightness": 0.09, "chroma": 0.025, "hue": 0.4, "frequency": 0.03}
            }
        }),
    )?;
    let mut a = StrokePoint::at(Vec2::new(100.0, 232.0), PointAttributes { width: 34.0, ..Default::default() });
    a.out_handle = Vec2::new(215.0, 18.0);
    let mut b = StrokePoint::at(Vec2::new(390.0, 142.0), PointAttributes { width: 52.0, ..Default::default() });
    b.in_handle = Vec2::new(300.0, 59.0);
    b.out_handle = Vec2::new(495.0, 234.0);
    let mut c = StrokePoint::at(Vec2::new(700.0, 92.0), PointAttributes { width: 18.0, ..Default::default() });
    c.in_handle = Vec2::new(588.0, 273.0);
    let result = session.execute("draw.bitmap_brush", &json!({"brush": brush["brush"], "anchors": [a, b, c], "color": "#8c3d3c", "seed": 7361}))?;

    session.execute("timeline.insert_keyframe", &json!({"frame": 20}))?;
    session.set_frame(20);
    session.execute("stroke.bitmap_channels", &json!({"element": result["element"], "point": 1, "width": 128, "opacity": 0.55, "drift": 1.6}))?;
    session.set_frame(0);
    session.execute("timeline.create_tween", &json!({"easing": "linear"}))?;

    anicraft_engine::format::save(&session.doc, &out.join("bitmap-brush.anicraft"))?;
    for frame in [0, 10, 20] {
        let picture = anicraft_render::render_frame(&session.doc, frame, 2.0)?;
        std::fs::write(out.join(format!("frame-{frame:02}.png")), picture.encode_png()?)?;
    }
    Ok(())
}
