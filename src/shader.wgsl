// One instanced quad per terminal cell. Cell position is derived from the
// instance index, so per-cell data is just 16 bytes.

struct Uniforms {
    cell: vec2<f32>,       // cell size in px
    viewport: vec2<f32>,   // surface size in px
    offset: vec2<f32>,     // grid origin (padding) in px
    cols: u32,
    underline_pos: u32,
    underline_thick: u32,
    strike_pos: u32,
    cursor_color: u32,
    _pad: u32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas: texture_2d<f32>;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) @interpolate(flat) glyph: vec2<u32>,
    @location(1) @interpolate(flat) fg: vec4<f32>,
    @location(2) @interpolate(flat) bg: vec4<f32>,
    @location(3) @interpolate(flat) flags: u32,
    @location(4) @interpolate(flat) origin: vec2<f32>,
};

const F_UNDERLINE: u32 = 1u;
const F_STRIKE: u32 = 2u;
const F_CURSOR_UNDERLINE: u32 = 4u;
const F_CURSOR_BAR: u32 = 8u;
const F_CURSOR_HOLLOW: u32 = 16u;

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    @builtin(instance_index) ii: u32,
    @location(0) glyph: u32,
    @location(1) fg: u32,
    @location(2) bg: u32,
    @location(3) flags: u32,
) -> VOut {
    let corner = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let cell = vec2<f32>(f32(ii % u.cols), f32(ii / u.cols));
    let origin = u.offset + cell * u.cell;
    let px = origin + corner * u.cell;
    var out: VOut;
    out.pos = vec4<f32>(px / u.viewport * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    out.glyph = vec2<u32>(glyph & 0xffffu, glyph >> 16u);
    out.fg = unpack4x8unorm(fg);
    out.bg = unpack4x8unorm(bg);
    out.flags = flags;
    out.origin = origin;
    return out;
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    let local = in.pos.xy - in.origin;
    let lp = vec2<u32>(local);
    var a = textureLoad(atlas, in.glyph + lp, 0).r;
    let y = lp.y;
    let t = u.underline_thick;
    if ((in.flags & F_UNDERLINE) != 0u && y >= u.underline_pos && y < u.underline_pos + t) {
        a = 1.0;
    }
    if ((in.flags & F_STRIKE) != 0u && y >= u.strike_pos && y < u.strike_pos + t) {
        a = 1.0;
    }
    var color = mix(in.bg, in.fg, a);
    if (in.flags >= F_CURSOR_UNDERLINE) {
        let cc = unpack4x8unorm(u.cursor_color);
        let cw = u32(u.cell.x);
        let ch = u32(u.cell.y);
        let bt = max(t, 2u);
        var hit = false;
        if ((in.flags & F_CURSOR_UNDERLINE) != 0u && y + bt >= ch) { hit = true; }
        if ((in.flags & F_CURSOR_BAR) != 0u && lp.x < bt) { hit = true; }
        if ((in.flags & F_CURSOR_HOLLOW) != 0u &&
            (lp.x < t || lp.x + t >= cw || y < t || y + t >= ch)) { hit = true; }
        if (hit) { color = cc; }
    }
    return color;
}
