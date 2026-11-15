//! The timeline's own drawing: shaders for what plain fills cannot say (the
//! fog over the future, the glow of fighting, the position lines), and the
//! small builders the view is made of.

use damascene_core::prelude::*;
use damascene_core::vector::{VectorGradient, VectorGradientStop, VectorLinearGradient, VectorSpreadMethod};

const FRAME_AND_IO: &str = r#"
struct FrameUniforms {
    viewport: vec2<f32>, time: f32, scale_factor: f32,
    white_scale: f32, headroom: f32, ref_nits: f32,
};
@group(0) @binding(0) var<uniform> frame: FrameUniforms;

struct VertexInput { @location(0) corner_uv: vec2<f32>, };
struct InstanceInput {
    @location(1) rect: vec4<f32>,
    @location(2) vec_a: vec4<f32>,
    @location(3) vec_b: vec4<f32>,
    @location(4) vec_c: vec4<f32>,
    @location(5) inner_rect: vec4<f32>,
    @location(6) vec_d: vec4<f32>,
    @location(7) vec_e: vec4<f32>,
    @location(8) opacity: f32,
};
struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) @interpolate(perspective, sample) pos_px: vec2<f32>,
    @location(1) inner: vec4<f32>,
    @location(2) a: vec4<f32>,
    @location(3) b: vec4<f32>,
    @location(4) c: vec4<f32>,
    @location(5) opacity: f32,
};

@vertex
fn vs_main(in: VertexInput, inst: InstanceInput) -> VertexOutput {
    let pos_px = in.corner_uv * inst.rect.zw + inst.rect.xy;
    var out: VertexOutput;
    out.clip_pos = vec4<f32>(pos_px.x / frame.viewport.x * 2.0 - 1.0, 1.0 - pos_px.y / frame.viewport.y * 2.0, 0.0, 1.0);
    out.pos_px = pos_px;
    out.inner = inst.inner_rect;
    out.a = inst.vec_a;
    out.b = inst.vec_b;
    out.c = inst.vec_c;
    out.opacity = inst.opacity;
    return out;
}

fn emit(rgb: vec3<f32>, a: f32, opacity: f32) -> vec4<f32> {
    if (a <= 1e-4) { return vec4<f32>(0.0); }
    return vec4<f32>(rgb * frame.white_scale, clamp(a, 0.0, 1.0) * opacity);
}

fn hash(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2<f32>(127.1, 311.7))) * 43758.5453);
}
fn noise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    return mix(mix(hash(i), hash(i + vec2<f32>(1.0, 0.0)), u.x),
               mix(hash(i + vec2<f32>(0.0, 1.0)), hash(i + vec2<f32>(1.0, 1.0)), u.x), u.y);
}
"#;

/// The future that is hidden: a bank of fog with a faint hatch, thickening
/// over its first pixels so it reads as "not yet" rather than as a box.
/// vec_a: fog colour. vec_b: (edge px, hatch spacing px, hatch alpha, cloud contrast).
const FOG: &str = r#"
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let x = in.pos_px.x - in.inner.x;
    let edge = smoothstep(0.0, max(in.b.x, 1.0), x);
    // Fixed to the screen, so the fog stays put and the game uncovers it.
    let p = in.pos_px / 70.0;
    let cloud = noise(p) * 0.65 + noise(p * 2.3 + vec2<f32>(5.2, 1.3)) * 0.35;
    let d = fract((in.pos_px.x + in.pos_px.y) / max(in.b.y, 2.0));
    let hatch = 1.0 - smoothstep(0.0, 0.09, min(d, 1.0 - d));
    let rgb = in.a.rgb * (1.0 + in.b.w * (cloud - 0.5)) + vec3<f32>(in.b.z * hatch);
    return emit(rgb, in.a.a * edge, in.opacity);
}
"#;

/// Fighting as light: a glow centred on the strip's middle that grows taller
/// and brighter with the intensity, interpolated across one sample step.
/// vec_a: glow colour. vec_b: (intensity at left, at right, edge band px, 0).
const HEAT: &str = r#"
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let u = clamp((in.pos_px.x - in.inner.x) / max(in.inner.z, 1.0), 0.0, 1.0);
    let i = mix(in.b.x, in.b.y, u);
    let v = (in.pos_px.y - in.inner.y) / max(in.inner.w, 1.0);
    let d = abs(v - 0.5) * 2.0;
    let spread = 0.18 + 0.6 * i;
    let glow = i * exp(-(d * d) / (spread * spread) * 2.5);
    let band = select(0.0, i, in.pos_px.y > in.inner.y + in.inner.w - in.b.z);
    return emit(in.a.rgb, max(in.a.a * glow, band * 0.85), in.opacity);
}
"#;

/// A vertical position line with a soft glow, optionally dashed.
/// vec_a: colour. vec_b: (core width px, glow radius px, glow alpha, dash px or 0).
const GLOW: &str = r#"
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let cx = in.inner.x + in.inner.z * 0.5;
    let d = abs(in.pos_px.x - cx);
    let half = in.b.x * 0.5;
    let core = 1.0 - smoothstep(half - 0.5, half + 0.5, d);
    let glow = in.b.z * exp(-(d * d) / max(in.b.y * in.b.y, 0.01));
    var a = max(core, glow);
    if (in.b.w > 0.0 && fract((in.pos_px.y - in.inner.y) / (in.b.w * 2.0)) > 0.5) {
        a = glow * 0.5;
    }
    return emit(in.a.rgb, in.a.a * a, in.opacity);
}
"#;

/// A jump in flight: stripes that run toward the target.
/// vec_a: colour. vec_b: (stripe spacing px, speed px/s, 0, 0).
const SWEEP: &str = r#"
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let s = max(in.b.x, 2.0);
    let d = fract((in.pos_px.x - in.pos_px.y - frame.time * in.b.y) / s);
    let stripe = smoothstep(0.0, 0.15, d) * (1.0 - smoothstep(0.45, 0.6, d));
    return emit(in.a.rgb, in.a.a * stripe, in.opacity);
}
"#;

fn source(body: &str) -> &'static str {
    Box::leak(format!("{FRAME_AND_IO}{body}").into_boxed_str())
}

pub const SHADER_FOG: &str = "barwatch_tl_fog";
pub const SHADER_HEAT: &str = "barwatch_tl_heat";
pub const SHADER_GLOW: &str = "barwatch_tl_glow";
pub const SHADER_SWEEP: &str = "barwatch_tl_sweep";

/// The shaders the timeline paints with; a host app registers them by
/// returning these from `App::shaders`.
pub fn shaders() -> Vec<AppShader> {
    static SOURCES: std::sync::OnceLock<[&'static str; 4]> = std::sync::OnceLock::new();
    let [fog, heat, glow, sweep] = *SOURCES.get_or_init(|| [source(FOG), source(HEAT), source(GLOW), source(SWEEP)]);
    vec![
        AppShader { name: SHADER_FOG, wgsl: fog, samples_backdrop: false, samples_time: false },
        AppShader { name: SHADER_HEAT, wgsl: heat, samples_backdrop: false, samples_time: false },
        AppShader { name: SHADER_GLOW, wgsl: glow, samples_backdrop: false, samples_time: false },
        AppShader { name: SHADER_SWEEP, wgsl: sweep, samples_backdrop: false, samples_time: true },
    ]
}

pub fn fog(color: Color, edge_px: f32) -> El {
    El::new(Kind::Custom("timeline_fog"))
        .shader(ShaderBinding::custom(SHADER_FOG).color("vec_a", color).vec4("vec_b", [edge_px, 7.0, 0.018, 0.5]))
        .fill(color)
}

pub fn heat(color: Color, left: f32, right: f32, band_px: f32) -> El {
    El::new(Kind::Custom("timeline_heat"))
        .shader(ShaderBinding::custom(SHADER_HEAT).color("vec_a", color).vec4("vec_b", [left, right, band_px, 0.0]))
}

pub fn glow_line(color: Color, core: f32, glow: f32, glow_alpha: f32, dash: f32) -> El {
    El::new(Kind::Custom("timeline_line"))
        .shader(ShaderBinding::custom(SHADER_GLOW).color("vec_a", color).vec4("vec_b", [core, glow, glow_alpha, dash]))
}

pub fn sweep(color: Color) -> El {
    El::new(Kind::Custom("timeline_sweep"))
        .shader(ShaderBinding::custom(SHADER_SWEEP).color("vec_a", color).vec4("vec_b", [18.0, 50.0, 0.0, 0.0]))
}

pub fn quad(color: Color) -> El {
    El::new(Kind::Custom("timeline_mark")).fill(color)
}

/// How a child of a [`placed`] area sits: horizontally in fractions of the
/// time axis, vertically in pixels from the area's top.
#[derive(Clone, Copy, Debug)]
pub enum Place {
    /// From one moment to another, between `top` and `bottom` px from the
    /// area's top and bottom edges.
    Span { from: f32, to: f32, top: f32, bottom: f32 },
    /// A vertical strip `w` px wide centred on a moment.
    Line { at: f32, w: f32, top: f32, bottom: f32 },
    /// A box `h` tall at `top`, as wide as its content (or `w`), anchored on a
    /// moment and kept inside the axis.
    Tag { at: f32, w: Option<f32>, h: f32, top: f32, anchor: Anchor },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Anchor {
    Center,
    /// Starts at the moment and runs right.
    After,
    /// Ends at the moment.
    Before,
}

impl Place {
    /// `c` is the area, `axis_x`/`axis_w` the time axis within it.
    pub fn rect(self, c: Rect, axis_x: f32, axis_w: f32, measured_w: f32) -> Rect {
        let x_of = |f: f32| axis_x + f * axis_w;
        match self {
            Place::Span { from, to, top, bottom } => {
                Rect::new(x_of(from), c.y + top, ((to - from) * axis_w).max(0.0), (c.h - top - bottom).max(0.0))
            }
            Place::Line { at, w, top, bottom } => {
                Rect::new((x_of(at) - w / 2.0).round(), c.y + top, w, (c.h - top - bottom).max(0.0))
            }
            Place::Tag { at, w, h, top, anchor } => {
                let w = w.unwrap_or(measured_w).min(axis_w);
                let (x, end) = (x_of(at), axis_x + axis_w);
                // A tag that would run off one side flips to the other, so it
                // stays beside its moment instead of sliding over it.
                let x = match anchor {
                    Anchor::Center => x - w / 2.0,
                    Anchor::After if x + w > end => x - w,
                    Anchor::After => x,
                    Anchor::Before if x - w < axis_x => x,
                    Anchor::Before => x - w,
                };
                let x = x.clamp(axis_x, (axis_x + axis_w - w).max(axis_x));
                Rect::new(x.round(), c.y + top, w, h)
            }
        }
    }
}

/// An area whose children sit on the time axis. The axis starts `gutter` px in
/// from the left; `height` is `None` to fill the parent (an overlay).
pub fn placed(height: Option<f32>, gutter: f32, items: Vec<(Place, El)>) -> El {
    let (places, els): (Vec<Place>, Vec<El>) = items.into_iter().unzip();
    El::new(Kind::Custom("timeline_area"))
        .width(Size::Fill(1.0))
        .height(height.map(Size::Fixed).unwrap_or(Size::Fill(1.0)))
        .children(els)
        .layout(move |cx| {
            let c = cx.container;
            let (axis_x, axis_w) = (c.x + gutter, (c.w - gutter).max(1.0));
            places
                .iter()
                .zip(cx.children)
                .map(|(p, child)| {
                    let measured = if matches!(p, Place::Tag { w: None, .. }) { (cx.measure)(child).0 } else { 0.0 };
                    p.rect(c, axis_x, axis_w, measured)
                })
                .collect()
        })
}

/// A polyline through `points` as a stroked path.
pub fn polyline(points: &[(f32, f32)], color: Color, width: f32) -> Option<VectorPath> {
    let (first, rest) = points.split_first()?;
    let mut p = PathBuilder::new().move_to(first.0, first.1);
    for (x, y) in rest {
        p = p.line_to(*x, *y);
    }
    Some(p.stroke_solid(color, width).stroke_line_join(VectorLineJoin::Round).stroke_line_cap(VectorLineCap::Round).build())
}

/// The area under `points` down to `base_y`, filled with gradient `gradient`.
pub fn area(points: &[(f32, f32)], base_y: f32, fill: VectorColor) -> Option<VectorPath> {
    let (first, _) = points.split_first()?;
    let last = points.last()?;
    let mut p = PathBuilder::new().move_to(first.0, base_y);
    for (x, y) in points {
        p = p.line_to(*x, *y);
    }
    let mut path = p.line_to(last.0, base_y).close().build();
    path.fill = Some(VectorFill { color: fill, opacity: 1.0, rule: VectorFillRule::NonZero });
    Some(path)
}

/// A vertical gradient from `top` (at `y0`) to `bottom` (at `y1`).
pub fn vertical_gradient(y0: f32, y1: f32, top: Color, bottom: Color) -> VectorGradient {
    let srgb = |c: Color| {
        let [r, g, b, a] = c.to_srgb_u8a();
        [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a as f32 / 255.0]
    };
    VectorGradient::Linear(VectorLinearGradient {
        p1: [0.0, y0],
        p2: [0.0, y1],
        stops: vec![
            VectorGradientStop { offset: 0.0, color: srgb(top) },
            VectorGradientStop { offset: 1.0, color: srgb(bottom) },
        ],
        spread: VectorSpreadMethod::Pad,
        absolute_to_local: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
    })
}

/// A small round sign in BAR's badge style: a dark rim, a coloured disc, and a
/// white plus, arrow, cross or minus.
pub fn class_sign(class: super::marks::MarkClass, size: f32) -> El {
    use super::marks::MarkClass;
    let s = size;
    let c = s / 2.0;
    let white = Color::srgb_u8(255, 255, 255);
    let disc = |r: f32, color: Color| {
        let k = 0.5523 * r;
        PathBuilder::new()
            .move_to(c + r, c)
            .cubic_to(c + r, c + k, c + k, c + r, c, c + r)
            .cubic_to(c - k, c + r, c - r, c + k, c - r, c)
            .cubic_to(c - r, c - k, c - k, c - r, c, c - r)
            .cubic_to(c + k, c - r, c + r, c - k, c + r, c)
            .close()
            .fill_solid(color)
            .build()
    };
    let arm = s * 0.26;
    let w = (s * 0.16).max(1.2);
    let line = |x0: f32, y0: f32, x1: f32, y1: f32| {
        PathBuilder::new().move_to(x0, y0).line_to(x1, y1).stroke_solid(white, w).stroke_line_cap(VectorLineCap::Round).build()
    };
    let mut paths = vec![disc(c, Color::srgb_u8a(0, 0, 0, 204)), disc(c - 1.0, class.color())];
    match class {
        MarkClass::Built => paths.extend([line(c - arm, c, c + arm, c), line(c, c - arm, c, c + arm)]),
        MarkClass::Tech => paths.extend([line(c, c + arm, c, c - arm), line(c - arm * 0.8, c - arm * 0.1, c, c - arm), line(c + arm * 0.8, c - arm * 0.1, c, c - arm)]),
        MarkClass::Attack => {
            let a = arm * 0.8;
            paths.extend([line(c - a, c - a, c + a, c + a), line(c - a, c + a, c + a, c - a)])
        }
        MarkClass::Lost => paths.push(line(c - arm, c, c + arm, c)),
        MarkClass::Out => {}
    }
    vector(VectorAsset::from_paths([0.0, 0.0, s, s], paths)).width(Size::Fixed(s)).height(Size::Fixed(s))
}
