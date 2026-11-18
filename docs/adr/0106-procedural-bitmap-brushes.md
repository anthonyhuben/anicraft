# ADR 0106 — Procedural bitmap-tip brushes

* Status: proposed (2026-11-17)
* Relates to M7, the Paint Brush (ADR 0068), shape armatures (ADR 0071) and Asset Warp (ADR 0079)

## Decision

A bitmap-tip stroke is a drawing object backed by the existing Bézier shape path.
A parallel array stores width, opacity and colour-drift values at each distinct anchor.
The stroke carries a colour, a stable seed and a document-local brush ID. Each brush
definition owns its original R8 tip masks and static placement and blending settings.
The native document stores no stamps, mipmaps, rendered stroke surfaces or caches.
Documents that contain a bitmap brush use envelope version 5. Existing documents retain
the version they would have used before.

Classic tweens interpolate the channels when their brush and anchor counts match.
Corresponding Bézier controls interpolate before rendering. Bone skinning carries the
anchors and their handles, and Asset Warp maps those controls through its mesh. The
stamp generator measures that evaluated path. Asset Warp preserves the brush's anchor
topology so the channel indices follow the controls.

The generator walks a bounded adaptive arc-length table with zoom-independent spacing.
It includes the end of an open stroke, uses the incoming tangent at corners, and treats a
zero-length centreline as a dab. A per-index hash drives tip choice and other variation.
Seeded noise varies hue, chroma and lightness in OKLCH. The runtime caches stamps by
copy-on-write geometry, channel and brush identities and by zoom bucket. A stroke can
emit up to 16,384 stamps. Very long strokes increase the minimum spacing to fit.

The CPU oracle and the GPU stage share a mipmapped R8 tip array and the same stamp
transforms and colours. The GPU draws one set of instanced quads per visible stroke per
tile. Accumulate uses premultiplied source-over. Max Per Stroke uses coverage alpha as a
depth value and retains the premultiplied colour of the strongest sample, taking the
later stamp on a tie. Both composite the result once through the stage's existing masks,
groups and filters. The CPU oracle resolves the same operations in bands.

## User workflow

Choose **Paint Brush** and **Import PNG Tip…** in Properties. Tip PNGs may be up to
256 × 256 pixels. White deposits pigment and black is transparent. Select the stroke to
edit its colour or any anchor's channels. Select **View › Show Bitmap Brush Texture** to
toggle the editor between texture and a plain vector preview. Test Movie and raster
exports regenerate full texture at their output resolution. Vector publishing formats
carry brush centreline artwork to the extent supported by their existing outline paths.

The Paint Brush uses a vector guide during a drag and shows the committed texture when
the pointer lifts. Native playback and raster rendering support bitmap tips. Texture
embedding in SVG and bitmap-tip playback in HTML or Lottie are future work.

## Example

Run the example with a grayscale PNG and an output directory.

```sh
cargo run -p anicraft-engine --example bitmap_brush -- tip.png output
```

It writes a native document and renders frames 0, 10 and 20 at 2×. The middle anchor's
width, opacity and colour drift change across the classic tween.
