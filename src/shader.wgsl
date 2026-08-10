@group(1) @binding(0) var lut_tex: texture_3d<f32>;
@group(1) @binding(1) var lut_sampler: sampler;

struct ShaperUniforms {
    min_exp: f32,
    max_exp: f32,
    channel_mode: u32,
    padding: f32,
};
@group(1) @binding(2) var<uniform> shaper: ShaperUniforms;

fn linear_to_shaper(linear: vec3<f32>) -> vec3<f32> {
    let eps = vec3<f32>(1e-10);
    let stops = log2(max(linear, eps));
    let range = shaper.max_exp - shaper.min_exp;
    let s = (stops - vec3<f32>(shaper.min_exp)) / vec3<f32>(range);
    return clamp(s, vec3<f32>(0.0), vec3<f32>(1.0));
}

fn apply_lut(scene_linear: vec3<f32>) -> vec3<f32> {
    let coord = linear_to_shaper(scene_linear);
    return textureSample(lut_tex, lut_sampler, coord).rgb;
}


struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var out: VertexOutput;
    let x = f32((vertex_index << 1u) & 2u);
    let y = f32(vertex_index & 2u);
    out.uv = vec2<f32>(x, y);
    out.clip_position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

@group(0) @binding(0) var r_tex: texture_2d<f32>;
@group(0) @binding(1) var g_tex: texture_2d<f32>;
@group(0) @binding(2) var b_tex: texture_2d<f32>;
@group(0) @binding(3) var a_tex: texture_2d<f32>;
@group(0) @binding(4) var exr_sampler: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let r = textureSample(r_tex, exr_sampler, in.uv).r;
    let g = textureSample(g_tex, exr_sampler, in.uv).r;
    let b = textureSample(b_tex, exr_sampler, in.uv).r;
    let a = textureSample(a_tex, exr_sampler, in.uv).r;

    let linear = vec4<f32>(r, g, b, a);
    var isolated: vec3<f32>;

    if (shaper.channel_mode == 1u) {
        isolated = vec3<f32>(linear.r, linear.r, linear.r);
    } else if (shaper.channel_mode == 2u) {
        isolated = vec3<f32>(linear.g, linear.g, linear.g);
    } else if (shaper.channel_mode == 3u) {
        isolated = vec3<f32>(linear.b, linear.b, linear.b);
    } else if (shaper.channel_mode == 4u) {
        isolated = vec3<f32>(linear.a, linear.a, linear.a);
    } else if (shaper.channel_mode == 5u){
        let lum = 0.2126*linear.r + 0.7152 * linear.g + 0.0722 * linear.b;
        isolated = vec3<f32>(lum, lum, lum);
    } else {
        isolated = linear.rgb;
    }


    let display_color = apply_lut(isolated);
    return vec4<f32>(display_color, linear.a);
}
