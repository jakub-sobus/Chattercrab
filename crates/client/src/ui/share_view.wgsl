struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

struct Fit {
    offset: vec2<f32>,
    scale: vec2<f32>,
}

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var<uniform> fit: Fit;

@vertex
fn vs(@builtin(vertex_index) index: u32) -> VsOut {
    // wgpu's clip y points up. The widget's top is +1, which is luma row 0.
    var positions = array<vec2<f32>, 4>(
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, -1.0),
    );
    var uvs = array<vec2<f32>, 4>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
    );
    var out: VsOut;
    out.pos = vec4<f32>(positions[index], 0.0, 1.0);
    out.uv = uvs[index];
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let image_uv = (in.uv - fit.offset) / fit.scale;
    if (image_uv.x < 0.0 || image_uv.x > 1.0 || image_uv.y < 0.0 || image_uv.y > 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    // BT.709 limited range. The hardware color converter and a 1440p stream use this.
    let y = textureSample(y_tex, samp, image_uv).r;
    let uv = textureSample(uv_tex, samp, image_uv).rg;
    let luma = (y - 0.0627451) * 1.164383;
    let u = uv.r - 0.5;
    let v = uv.g - 0.5;
    let r = luma + 1.792741 * v;
    let g = luma - 0.213249 * u - 0.532909 * v;
    let b = luma + 2.112402 * u;
    return vec4<f32>(clamp(r, 0.0, 1.0), clamp(g, 0.0, 1.0), clamp(b, 0.0, 1.0), 1.0);
}
