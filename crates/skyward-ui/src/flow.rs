//! The flowing light the recovery's first page is drawn on.
//!
//! Read out of the phone's own splash, which draws it with an AGSL runtime
//! shader: four soft lights over a field of noise, the noise lifting the colour
//! out of them and adding light where it thins, and a little dither over the
//! whole frame. The recovery has no such shader to call, so the same arithmetic
//! is written out here — the same lights, the same field, the same order of
//! mixing — and worked out at a coarse grid that is stretched over the panel.

use crate::canvas::Canvas;
use crate::color::Rgba;

/// How coarse the field is worked out before it is stretched over the panel.
const COARSE: u32 = 12;

/// How fast the lights drift over the page, and how fast the noise runs
/// through them, in fields a second: the noise the faster of the two, so that
/// the colour on the page is seen to move.
const DRIFT: f32 = 0.20;
const FLOW: f32 = 0.95;

/// One light: where it sits, how far it reaches, and its colour.
struct Light {
    x: f32,
    y: f32,
    reach: f32,
    rgb: [f32; 3],
    alpha: f32,
}

/// The lights of the pale page, as the shader's `uColors` and `uPoints` hold
/// them: sky, rose, periwinkle, rose — the colour of the phone's own splash.
const LIGHTS: [Light; 4] = [
    Light { x: 0.670, y: 0.420, reach: 0.56, rgb: [0.580, 0.740, 1.000], alpha: 0.560 },
    Light { x: 0.690, y: 0.750, reach: 0.500, rgb: [1.000, 0.900, 0.930], alpha: 0.590 },
    Light { x: 0.140, y: 0.710, reach: 0.750, rgb: [0.740, 0.760, 1.000], alpha: 1.000 },
    Light { x: 0.140, y: 0.270, reach: 0.800, rgb: [0.970, 0.770, 0.840], alpha: 1.000 },
];

/// How much wider the lights reach than the shader's own points size them: the
/// recovery draws the page on its own panel, without the bound the shader is
/// given, and the lights have to cover it.
const REACH: f32 = 1.5;

/// How far the threads are deepened: away from grey, and a little darker, so
/// that the light running over the page carries colour of its own.
const DEEP: f32 = 1.30;
const DIM: f32 = 0.10;

/// How long the page takes to open: it starts as one light in the middle and
/// spreads out over the page.
const OPENING: f32 = 0.95;

/// Where the point of light that opens the page sits, and how bright the core
/// of it stays once the light has spread over the whole page.
const CORE: (f32, f32) = (0.50, 0.47);
const CORE_LIGHT: f32 = 0.20;

/// Paint the whole of `c`, `size` being the panel it covers, `time` seconds
/// since the page came up.
///
/// The field is worked out at an eighth of the panel, and each row of it is
/// widened once into a strip; the page then takes whole rows of the strip,
/// which is a copy of memory a row rather than a colour worked out a pixel.
/// Worked out a pixel at a time, a frame of this takes longer than the phone
/// spends bringing itself up, and the page would be seen to stutter.
pub fn shade(c: &mut Canvas, size: (u32, u32), time: f32) {
    let (w, h) = (size.0.max(1), size.1.max(1));
    let aspect = w as f32 / h as f32;
    let (gw, gh) = (w / COARSE + 2, h / COARSE + 2);
    // The field, in bands of rows, a band to a thread.
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    let rows = (gh as usize).div_ceil(threads);
    let mut field = vec![[0f32; 3]; (gw * gh) as usize];
    std::thread::scope(|scope| {
        for (i, part) in field.chunks_mut(gw as usize * rows).enumerate() {
            let first = (i * rows) as u32;
            scope.spawn(move || {
                for (k, cell) in part.chunks_mut(gw as usize).enumerate() {
                    let v = ((first + k as u32) as f32 * COARSE as f32) / h as f32;
                    for (gx, c) in cell.iter_mut().enumerate() {
                        let u = (gx as f32 * COARSE as f32) / w as f32;
                        *c = at(u, v, time, aspect);
                    }
                }
            });
        }
    });
    // Each row of the field widened into a strip of the panel's width, the
    // dither of it grain by grain.
    let mut strip = Canvas::band(w, gh, 0..gh, 0..w, Rgba::rgb(0, 0, 0));
    for gy in 0..gh {
        let row = &field[(gy as usize) * (gw as usize)..];
        let grain = &DITHER[((gy as usize) % 4) * 4..][..4];
        strip.paint_row(gy, |x| {
            let gx = (x / COARSE as usize).min(gw as usize - 1);
            let c = row[gx];
            let d = grain[gx % 4] as i32 - 2;
            Rgba::rgb(
                (byte(c[0]) as i32 + d).clamp(0, 255) as u8,
                (byte(c[1]) as i32 + d).clamp(0, 255) as u8,
                (byte(c[2]) as i32 + d).clamp(0, 255) as u8,
            )
        });
    }
    for y in c.held() {
        let gy = (y as usize / COARSE as usize).min(gh as usize - 1) as i32;
        c.put_rows(&strip, gy, gy + 1, y as i32);
    }
}

/// A four by four grain of dither, a step either side of the colour it lands
/// on: the widest grade of the page is pale enough to band without it.
const DITHER: [u8; 16] = [0, 2, 1, 3, 3, 1, 2, 0, 1, 3, 0, 2, 2, 0, 3, 1];

/// The light at `u`, `v` of the page, `time` seconds in — the shader's `main`,
/// laid over the white of the page rather than over black.
pub fn at(u: f32, v: f32, time: f32, aspect: f32) -> [f32; 3] {
    // The shader counts `y` from the bottom of the page and this counts it from
    // the top, so the page is turned over once, as the shader turns it.
    let v = 1.0 - v;
    let open = smoothstepf(time / OPENING);
    // Distances are counted in heights of the page, the width taken with it,
    // so that a light reaches as far across as it does up.
    let across = |u: f32, cu: f32| (u - cu) * aspect;
    // The four lights, mixed in the shader's own order, in the shader's own
    // arithmetic: each colour is premultiplied by its own alpha, and the sum is
    // divided by the alpha it gathered, as the shader does before it uses it.
    let (mut rgb, mut alpha) = ([0.0f32; 3], 0.0f32);
    for (i, light) in LIGHTS.iter().enumerate() {
        let phase = time * DRIFT + i as f32 * 0.37;
        let x = light.x + (phase * 6.283).cos() * 0.05;
        let y = light.y + (phase * 6.283).sin() * 0.04;
        let pct = smoothstep(light.reach * REACH, 0.0, hypot(across(u, x), v - y));
        for k in 0..3 {
            rgb[k] = rgb[k] * (1.0 - pct) + light.rgb[k] * light.alpha * pct;
        }
        alpha = alpha * (1.0 - pct) + light.alpha * pct;
    }
    if alpha > 0.0001 {
        for k in 0..3 {
            rgb[k] /= alpha;
        }
    }
    // The noise, drifting as the shader drifts it; it takes the colour out
    // where it is high and adds light, so that threads of light run over the
    // page.
    let noise = perlin(u * 7.0 - time * FLOW, v * 7.0 - time * FLOW);
    let thread = smoothstepf(noise) * open;
    let grey = luma(rgb);
    for k in 0..3 {
        // The threads are the same colours, deepened: pushed away from grey and
        // taken down a little, so that they read as colour moving over the page
        // rather than as light washing it out.
        let deep = (grey + (rgb[k] - grey) * DEEP) * (1.0 - DIM);
        rgb[k] = rgb[k] * (1.0 - thread) + deep * thread;
    }
    // Laid over the white of the page, so that where the lights leave the page
    // alone it stays white.
    let alpha = alpha.clamp(0.0, 1.0);
    // The page is black until the light spreading from its middle reaches it: it
    // starts as one point in the middle of a black page, and the circle of it
    // grows until it has covered the whole page.
    let from = hypot(across(u, CORE.0), v - (1.0 - CORE.1));
    let reach = 1.20 * open;
    let soft = (reach * 0.55).min(0.16);
    let lit = smoothstep(reach, (reach - soft).max(0.0), from);
    let mut out = [0.0f32; 3];
    for k in 0..3 {
        out[k] = (rgb[k] * alpha + (1.0 - alpha)) * lit;
    }
    // The core of the light stays a little brighter than the page it opened, and
    // comes up with the light itself: at the first frame the page is black and
    // nothing on it.
    let core = smoothstep((0.30f32).min(reach * 0.8), 0.0, from);
    for k in 0..3 {
        out[k] += (1.0 - out[k]) * core * (0.95 - (0.95 - CORE_LIGHT) * open);
    }
    out
}

/// The panel's own proportion, for the tests and the pages that ask only of a
/// point: the lights are drawn as circles on it.
pub const ASPECT: f32 = 1880.0 / 3008.0;

fn byte(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn luma(rgb: [f32; 3]) -> f32 {
    rgb[0] * 0.299 + rgb[1] * 0.587 + rgb[2] * 0.114
}

fn hypot(x: f32, y: f32) -> f32 {
    (x * x + y * y).sqrt()
}

/// The shader's `smoothstep` with its edges the other way round: 1 at `edge0`,
/// 0 at `edge1`.
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if (edge1 - edge0).abs() < f32::EPSILON {
        return if x < edge0 { 1.0 } else { 0.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn smoothstepf(x: f32) -> f32 {
    let t = x.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The shader's `perlin`.
fn perlin(x: f32, y: f32) -> f32 {
    let (ix, iy) = (x.floor(), y.floor());
    let (fx, fy) = (x - ix, y - iy);
    let a = hash(ix, iy);
    let b = hash(ix + 1.0, iy);
    let c = hash(ix, iy + 1.0);
    let d = hash(ix + 1.0, iy + 1.0);
    let sx = fx * fx * (3.0 - 2.0 * fx);
    let sy = fy * fy * (3.0 - 2.0 * fy);
    let upper = a + (b - a) * sx;
    let lower = c + (d - c) * sx;
    upper + (lower - upper) * sy
}

/// The shader's `hash`.
fn hash(x: f32, y: f32) -> f32 {
    let (px, py) = (fract(x * 0.13), fract(y * 0.13));
    let p3 = [px, py, px];
    let d = p3[0] * (p3[1] + 3.333) + p3[1] * (p3[2] + 3.333) + p3[2] * (p3[0] + 3.333);
    let p3 = [p3[0] + d, p3[1] + d, p3[2] + d];
    fract((p3[0] + p3[1]) * p3[2])
}

fn fract(v: f32) -> f32 {
    v - v.floor()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_is_pale_everywhere_so_the_words_on_it_read() {
        // Once the light has spread, the band the words are written in is pale
        // enough for them to read, and no part of the page is past white.
        for step in 0..20 {
            let time = OPENING + step as f32 * 1.3;
            for k in 0..6 {
                let u = k as f32 / 5.0;
                // The name of the recovery sits a little above the middle.
                for c in at(u, 0.40, time, ASPECT) {
                    assert!(c >= 0.50, "{c} is too dark around the words at {u} after {time}s");
                }
                // Where the words start, in the middle of the page, the light
                // stays well out of their way.
                for c in at(0.5, 0.40, time, ASPECT) {
                    assert!(c >= 0.70, "{c} is too dark under the words after {time}s");
                }
                for v in [0.05, 0.5, 0.95] {
                    for c in at(u, v, time, ASPECT) {
                        assert!(c <= 1.0, "{c} is past white at {u},{v} after {time}s");
                        assert!(c >= 0.35, "{c} is darker than a page of light at {u},{v}");
                    }
                }
            }
        }
        // The first frame is black: the light has not reached anywhere yet.
        for c in at(CORE.0, CORE.1, 0.0, ASPECT) {
            assert!(c <= 0.02, "{c} is lit at the first frame");
        }
    }

    #[test]
    fn the_light_spreads_out_from_the_middle_as_the_page_opens() {
        let spread = |c: [f32; 3]| c.iter().cloned().fold(f32::MIN, f32::max) - c.iter().cloned().fold(f32::MAX, f32::min);
        // The first frame is the black page and nothing on it.
        for (u, v) in [(0.04, 0.04), (0.5, 0.5), (0.96, 0.96)] {
            for c in at(u, v, 0.0, ASPECT) {
                assert!(c <= 0.02, "{c} is lit at {u},{v} before the light starts");
            }
        }
        // A moment in, the middle is lit and the corners are still black: the
        // light starts in the middle and goes out from it.
        let middle = at(CORE.0, CORE.1, 0.30, ASPECT);
        let corner = at(0.04, 0.04, 0.30, ASPECT);
        assert!(luma(middle) > 0.25, "the light starts in the middle");
        assert!(luma(corner) < 0.05, "the light has not reached the corner yet");
        // Once it has spread, the corner carries colour of the page.
        let late = at(0.04, 0.04, 4.0, ASPECT);
        assert!(spread(late) > 0.03, "the corner is coloured once the light reaches it");
    }

    #[test]
    fn the_page_changes_colour_as_time_passes() {
        assert_ne!(at(0.3, 0.4, 0.0, ASPECT), at(0.3, 0.4, 9.0, ASPECT), "the light is flowing, not still");
    }

    #[test]
    fn painting_a_canvas_draws_the_whole_of_it() {
        let mut c = Canvas::filled(64, 48, Rgba::rgb(0, 0, 0));
        shade(&mut c, (64, 48), 3.0);
        let mut png = Vec::new();
        c.write_png(&mut png).unwrap();
        assert!(png.len() > 1000, "the frame was drawn");
    }
}

#[cfg(test)]
mod timing {
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore]
    fn a_frame_of_the_panel_takes() {
        let (w, h) = (1880u32, 3008u32);
        let mut c = Canvas::filled(w, h, Rgba::rgb(255, 255, 255));
        for n in 0..3 {
            let at = Instant::now();
            shade(&mut c, (w, h), n as f32 * 0.5);
            eprintln!("frame {n}: {:?}", at.elapsed());
        }
    }
}
