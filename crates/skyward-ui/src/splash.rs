//! The page the recovery shows while it brings the phone up: the flowing light
//! over the name of the recovery, what it is doing, and — only once there is
//! something to go on to — the way on.

use crate::canvas::Canvas;
use crate::color::Rgba;
use crate::flow;
use crate::font::{Face, Fonts};
use crate::lang::{Lang, Text};
use crate::theme::Theme;

/// What the page says the recovery is.
pub const TITLE: &str = "Skyward Recovery";

/// The weight the name is set in: heavier than any page title, so that it
/// carries the page.
pub const TITLE_WGHT: f32 = 620.0;

/// The way on, which appears when the phone has been brought up.
pub const GO: Text = ["继续", "繼續", "Continue"];

/// What the page says while it waits, where the phone has not said what it is
/// doing yet.
pub const STARTING: Text = ["正在启动…", "正在啟動…", "Starting…"];

/// How far up the page the name of the recovery sits.
const TITLE_AT: f32 = 0.44;

/// Where the way on sits, the panel's width and height given it: left, top,
/// width and height, so that the page that runs it knows where a tap lands.
pub fn way_on(w: f32, h: f32, margin: f32, height: f32) -> (f32, f32, f32, f32) {
    (margin, h - margin - height, w - margin * 2.0, height)
}

/// Draw the whole page: `time` seconds since it came up, `note` what the phone
/// is doing, and whether `ready` says the way on is to be shown.
pub fn draw(c: &mut Canvas, t: &Theme, f: &Fonts, title: &Face, lang: Lang, time: f32, note: &str, ready: bool) {
    let m = &t.m;
    let size = (m.w, m.h);
    flow::shade(c, size, time);
    // The name of the recovery, centred, in the ink of the page.
    let px = m.dp(31.0);
    let centre = m.w as f32 / 2.0;
    let cap = title.cap(px);
    let title_y = m.h as f32 * TITLE_AT;
    c.text_center(title, TITLE, centre, title_y, px, t.pal.text);
    // What the phone is doing, under it, in the colour of captions.
    let note = match note.is_empty() {
        true => lang.pick(STARTING),
        false => note,
    };
    c.text_center(&f.body, note, centre, title_y + cap * 1.6 + m.sub_px, m.sub_px, t.pal.subtitle);
    if !ready {
        return;
    }
    // The way on, over the light, in the page's accent.
    let (x, y, w, h) = way_on(m.w as f32, m.h as f32, m.margin, m.dp(54.0));
    let pill = h / 2.0;
    c.fill_round_rect(x, y, w, h, pill, t.pal.accent);
    let label_px = m.dp(17.0);
    c.text_center(&f.body, lang.pick(GO), x + w / 2.0, y + h / 2.0, label_px, Rgba::rgb(255, 255, 255));
}

#[cfg(test)]
mod tests {
    use super::*;
    fn page() -> (Theme, Fonts) {
        (Theme::light(1440, 3200, 3.5), crate::font::bundled())
    }

    #[test]
    fn the_page_draws_without_the_way_on_until_the_phone_is_up_and_with_it_after() {
        let (t, f) = page();
        let title = f.face(TITLE_WGHT).unwrap();
        let mut page = Canvas::new(t.m.w, t.m.h);
        draw(&mut page, &t, &f, &title, Lang::Hans, 1.0, "正在加载内核模块", false);
        assert!(page.whole(), "the whole page is drawn");
        let (x, y, w, h) = way_on(t.m.w as f32, t.m.h as f32, t.m.margin, t.m.dp(54.0));
        assert!(x > 0.0 && y > 0.0 && w > 0.0 && h > 0.0, "the way on is on the page");
    }
}
