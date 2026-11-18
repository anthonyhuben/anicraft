// A unit quad instanced once per stamp. All tips share one mipmapped R8 array.
struct Varying {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) color: vec4<f32>,
    @location(2) @interpolate(flat) tip: i32,
    @location(3) @interpolate(flat) lod: f32,
}

@group(0) @binding(0) var tips: texture_2d_array<f32>;
@group(0) @binding(1) var tip_sampler: sampler;

@vertex
fn stamp_vertex(
    @builtin(vertex_index) vertex: u32,
    @location(0) axes: vec4<f32>,
    @location(1) origin_tip_lod: vec4<f32>,
    @location(2) color: vec4<f32>,
) -> Varying {
    let uv = vec2<f32>(f32(vertex & 1u), f32(vertex >> 1u));
    let p = origin_tip_lod.xy + axes.xy * uv.x + axes.zw * uv.y;
    return Varying(vec4<f32>(p, 0.0, 1.0), uv, color, i32(origin_tip_lod.z), origin_tip_lod.w);
}

fn pigment(v: Varying) -> vec4<f32> {
    return v.color * textureSampleLevel(tips, tip_sampler, v.uv, v.tip, v.lod).r;
}

@fragment
fn accumulate(v: Varying) -> @location(0) vec4<f32> {
    return pigment(v);
}

struct Weighted {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) alpha: f32,
}

@fragment
fn strongest(v: Varying) -> Weighted {
    let p = pigment(v);
    if p.a == 0.0 {
        discard;
    }
    // Greater-or-equal selects the whole strongest sample, including its drifted color.
    return Weighted(p, p.a);
}

@group(0) @binding(0) var surface: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> paints: array<vec4<f32>>;
@group(0) @binding(2) var<uniform> destination: vec4<u32>;

@compute @workgroup_size(8, 8)
fn resolve(@builtin(global_invocation_id) p: vec3<u32>) {
    let dim = textureDimensions(surface);
    if p.x < dim.x && p.y < dim.y {
        paints[destination.x + p.y * dim.x + p.x] = textureLoad(surface, vec2<i32>(p.xy), 0);
    }
}
