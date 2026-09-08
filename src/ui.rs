// Round-display UI. Each state gets a distinct, legible look: a color-coded
// ring, a center glyph, and a status line.
//
// Everything is laid out inside the inscribed circle of the 240x240 panel -
// the physical bezel clips the corners, so any geometry that strays past
// r=116 from center is simply invisible on the real device.

extern crate alloc;
use alloc::{format, string::String, vec::Vec};

use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, ascii::FONT_9X15_BOLD, MonoTextStyle},
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{
        Arc, Circle, CornerRadii, Line, Primitive, PrimitiveStyle, PrimitiveStyleBuilder,
        Rectangle, RoundedRectangle, Triangle,
    },
    text::{Alignment, Baseline, Text, TextStyleBuilder},
};
use lcd_async::{interface::Interface, models::GC9A01, raw_framebuf::RawFrameBuf, Display};

use crate::pins::{DISPLAY_HEIGHT, DISPLAY_WIDTH};

const W: i32 = DISPLAY_WIDTH as i32;
const H: i32 = DISPLAY_HEIGHT as i32;
const CENTER: Point = Point::new(W / 2, H / 2);

const BG: Rgb565 = Rgb565::new(0, 0, 0);
const DIM: Rgb565 = Rgb565::new(4, 8, 4);
const BLUE: Rgb565 = Rgb565::new(4, 20, 31);
const PURPLE: Rgb565 = Rgb565::new(18, 8, 28);
const GREEN: Rgb565 = Rgb565::new(4, 40, 12);
const RED: Rgb565 = Rgb565::new(28, 4, 4);
const AMBER: Rgb565 = Rgb565::new(31, 40, 0);
const WHITE: Rgb565 = Rgb565::new(31, 63, 31);
const GREY: Rgb565 = Rgb565::new(12, 24, 12);
const ROW_BG: Rgb565 = Rgb565::new(3, 6, 5);

// ---------- Menu geometry ----------
// Shared with the touch hit-testing in main.rs so the drawn rows and the
// tappable rows can never drift apart.

/// Left/right edge of a menu row.
///
/// These used to be 32/208, which fit inside the bezel but ran the rows almost
/// the full width of the panel - on a round display that reads as a list that
/// has outgrown its screen rather than as buttons. 152px leaves a visible gutter
/// following the curve on both sides, and still clears the longest label
/// ("Erase Settings", 126px in FONT_9X15_BOLD) by 13px each side.
///
/// The binding constraint is the lowest row's outer corners: at (196, 190) that
/// is r=103 from center, comfortably inside the r=116 inscribed circle.
pub const MENU_X0: i32 = 44;
pub const MENU_X1: i32 = 196;
pub const MENU_ROW_H: i32 = 40;
const MENU_ROW_GAP: i32 = 6;
const MENU_TOP: i32 = 58;
/// Width of the -/+ tap zones at each end of a row that has a value. Narrowed
/// alongside the rows so the stepper's body zone keeps enough room for the
/// label above the value (36px) rather than being squeezed out by the end zones.
pub const MENU_STEP_ZONE: i32 = 42;

/// Top edge (y) of menu row `i`.
pub const fn menu_row_top(i: usize) -> i32 {
    MENU_TOP + i as i32 * (MENU_ROW_H + MENU_ROW_GAP)
}

/// Which menu row, if any, contains the point `(x, y)`.
pub fn menu_row_at(x: i32, y: i32, row_count: usize) -> Option<usize> {
    if x < MENU_X0 || x > MENU_X1 {
        return None;
    }
    (0..row_count).find(|&i| {
        let top = menu_row_top(i);
        y >= top && y < top + MENU_ROW_H
    })
}

/// Where in a row the tap landed, for rows that expose a -/+ stepper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowZone {
    Decrement,
    Body,
    Increment,
}

pub fn row_zone(x: i32) -> RowZone {
    if x < MENU_X0 + MENU_STEP_ZONE {
        RowZone::Decrement
    } else if x > MENU_X1 - MENU_STEP_ZONE {
        RowZone::Increment
    } else {
        RowZone::Body
    }
}

/// One line of a menu. `value` renders right-aligned; `stepper` adds the
/// -/+ affordances and makes the end zones tappable.
#[derive(Clone)]
pub struct MenuRow {
    pub label: String,
    pub value: Option<String>,
    pub stepper: bool,
    pub danger: bool,
}

// ---------- Settings-list geometry ----------

/// Entries visible at once on the settings card. Three 40px blocks is what fits
/// between the title and the page counter with every corner still inside the
/// bezel; the rest are reached by swiping.
pub const SETTINGS_PAGE_ROWS: usize = 3;
const SETTINGS_TOP: i32 = 50;
const SETTINGS_ROW_H: i32 = 40;

/// One line of the read-only settings card: what the setting is, and what it is
/// currently set to. `placeholder` means `value` is a stand-in ("not set")
/// rather than a real value, and gets drawn in amber to say so.
#[derive(Clone)]
pub struct SettingEntry {
    pub label: String,
    pub value: String,
    pub placeholder: bool,
}

impl MenuRow {
    pub fn action(label: impl Into<String>) -> Self {
        Self { label: label.into(), value: None, stepper: false, danger: false }
    }

    pub fn danger(label: impl Into<String>) -> Self {
        Self { label: label.into(), value: None, stepper: false, danger: true }
    }

    pub fn stepper(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self { label: label.into(), value: Some(value.into()), stepper: true, danger: false }
    }
}

#[derive(Clone)]
pub enum UiState {
    /// Shown from the moment the display is alive until the main loop starts
    /// accepting touch. Without it the device looks finished and ignores taps
    /// for the several seconds that codec init and the wifi join take.
    Boot {
        step: &'static str,
        progress: u8,
    },
    Idle {
        wifi_ok: bool,
        /// Completed back-and-forth pairs still in the context sent to the
        /// model. Zero means a fresh conversation and draws nothing at all.
        exchanges: usize,
        /// `None` until the sampler has published its first average, which is
        /// about two seconds after boot. Drawing nothing is better than
        /// flashing an 0% badge that immediately jumps to full.
        battery: Option<Battery>,
    },
    /// `level` is 0..=10, derived from the live mic RMS so the ring reacts to
    /// the user's actual voice instead of a free-running animation.
    Listening {
        level: u8,
    },
    Thinking {
        tool: Option<String>,
    },
    Speaking {
        snippet: String,
    },
    Error(String),
    Menu {
        title: &'static str,
        rows: Vec<MenuRow>,
    },
    /// Read-only list of what the device is actually configured with.
    /// `offset` is the index of the first visible entry.
    Settings {
        entries: Vec<SettingEntry>,
        offset: usize,
    },
    /// Config AP is up; tell the user what to join and where to browse.
    Portal {
        ap_ssid: String,
        url: String,
        clients: u8,
    },
    Notice {
        title: String,
        body: String,
        color: NoticeColor,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NoticeColor {
    Info,
    Warn,
    Good,
}

/// Widens an Rgb565 screen colour to the 8-bit-per-channel value the WS2812
/// wants. Red and blue are 5-bit on the panel, green is 6-bit, so each channel
/// scales by its own maximum.
fn led_of(color: Rgb565) -> (u8, u8, u8) {
    (
        (color.r() as u16 * 255 / 31) as u8,
        (color.g() as u16 * 255 / 63) as u8,
        (color.b() as u16 * 255 / 31) as u8,
    )
}

/// The status-LED colour for a state, taken from the same palette constant the
/// state's ring is drawn with. Derived rather than duplicated so the light and
/// the screen can't drift apart when a colour is retuned.
pub fn led_color(state: &UiState) -> (u8, u8, u8) {
    const OFF: (u8, u8, u8) = (0, 0, 0);
    match state {
        // Nothing is happening, and a lit LED on a device sitting on a shelf is
        // just light pollution. The menu cards are navigation, not activity.
        UiState::Idle { .. } | UiState::Menu { .. } | UiState::Settings { .. } => OFF,
        // The boot ring's track is DIM; the part that actually moves is the
        // blue progress arc, so that's the colour the boot state reads as.
        UiState::Boot { .. } => led_of(BLUE),
        UiState::Listening { .. } => led_of(BLUE),
        UiState::Thinking { .. } => led_of(PURPLE),
        UiState::Speaking { .. } => led_of(GREEN),
        UiState::Error(_) => led_of(RED),
        UiState::Portal { .. } => led_of(AMBER),
        UiState::Notice { color, .. } => match color {
            NoticeColor::Info => led_of(BLUE),
            NoticeColor::Warn => led_of(AMBER),
            NoticeColor::Good => led_of(GREEN),
        },
    }
}

fn ring(fbuf: &mut RawFrameBuf<Rgb565, &mut [u8]>, color: Rgb565) {
    Circle::with_center(CENTER, (W - 8) as u32)
        .into_styled(PrimitiveStyle::with_stroke(color, 5))
        .draw(fbuf)
        .ok();
}

fn status_text(fbuf: &mut RawFrameBuf<Rgb565, &mut [u8]>, text: &str, color: Rgb565) {
    let style = MonoTextStyle::new(&FONT_6X10, color);
    let text_style = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Middle)
        .build();
    Text::with_text_style(text, Point::new(W / 2, H - 34), style, text_style)
        .draw(fbuf)
        .ok();
}

fn big_label(fbuf: &mut RawFrameBuf<Rgb565, &mut [u8]>, text: &str, color: Rgb565) {
    let style = MonoTextStyle::new(&FONT_9X15_BOLD, color);
    let text_style = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Middle)
        .build();
    Text::with_text_style(text, CENTER, style, text_style)
        .draw(fbuf)
        .ok();
}

/// Greedy word wrap to `max_chars` per line, capped at `max_lines`. The last
/// line gets an ellipsis if anything was dropped. Words longer than a line are
/// hard-split rather than overflowing off the edge of the panel.
fn wrap(text: &str, max_chars: usize, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut truncated = false;

    for word in text.split_whitespace() {
        let mut word = word;
        loop {
            let need = if current.is_empty() { word.chars().count() } else { current.chars().count() + 1 + word.chars().count() };
            if need <= max_chars {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
                break;
            }
            if !current.is_empty() {
                lines.push(core::mem::take(&mut current));
                if lines.len() == max_lines {
                    truncated = true;
                    break;
                }
                continue;
            }
            // A single word longer than a whole line: hard-split it.
            let cut = word
                .char_indices()
                .nth(max_chars)
                .map(|(i, _)| i)
                .unwrap_or(word.len());
            lines.push(word[..cut].into());
            if lines.len() == max_lines {
                truncated = true;
                break;
            }
            word = &word[cut..];
        }
        if truncated {
            break;
        }
    }
    if !truncated && !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    } else if !current.is_empty() || truncated {
        truncated = true;
    }

    if truncated {
        if let Some(last) = lines.last_mut() {
            while last.chars().count() > max_chars.saturating_sub(1) {
                last.pop();
            }
            last.push('~');
        }
    }
    lines
}

/// Draws wrapped text with the block vertically centered on `center_y`.
fn wrapped_text(
    fbuf: &mut RawFrameBuf<Rgb565, &mut [u8]>,
    text: &str,
    center_y: i32,
    max_chars: usize,
    max_lines: usize,
    color: Rgb565,
) {
    let lines = wrap(text, max_chars, max_lines);
    if lines.is_empty() {
        return;
    }
    let style = MonoTextStyle::new(&FONT_6X10, color);
    let text_style = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Middle)
        .build();
    const LINE_H: i32 = 12;
    let start = center_y - (lines.len() as i32 - 1) * LINE_H / 2;
    for (i, line) in lines.iter().enumerate() {
        Text::with_text_style(line, Point::new(W / 2, start + i as i32 * LINE_H), style, text_style)
            .draw(fbuf)
            .ok();
    }
}

/// Mic glyph: a capsule body cradled in a U-shaped yoke, on a stand.
///
/// The shape is doing real work here - it is the only thing on the Idle card
/// telling a stranger what the device is for.
///
/// **Every part of it is a filled shape, and that is the whole trick.**
/// embedded-graphics does no antialiasing, so a thin stroke following a curve
/// is drawn as a chain of single-pixel steps: a stroked `Arc` at this size
/// comes out visibly lumpy, with the step pattern changing as the curve turns,
/// and it reads as a broken wire rather than a yoke. Filled areas have no thin
/// curve to alias - only their outer boundary is stepped, and at these radii
/// that boundary looks smooth. So the yoke is built by subtraction instead: a
/// disc, a smaller disc punched out of it in `BG`, then the top half cut away,
/// leaving a clean band of exactly even thickness. `render` clears the whole
/// framebuffer to `BG` before dispatching here, and nothing else is drawn
/// within 55px of center on either card that uses this, so punching with `BG`
/// is safe.
///
/// It is also drawn about a third larger than the first version. Curves get
/// smoother the more pixels they span, and the Idle card had the room going
/// spare.
///
/// Order matters: the yoke's cuts would erase the body, so the body goes on
/// last. Furthest corner is 39px from center, inside the radius-55 floor of
/// the Listening level ring.
fn draw_mic(fbuf: &mut RawFrameBuf<Rgb565, &mut [u8]>, color: Rgb565) {
    let yoke = Point::new(W / 2, H / 2 - 14);
    const YOKE_OUTER: u32 = 54;
    const YOKE_INNER: u32 = 44;
    // Weight shared by the yoke band, the stand and the base, so the whole
    // glyph reads as one stroke thickness.
    const STROKE: i32 = (YOKE_OUTER as i32 - YOKE_INNER as i32) / 2;

    let outer = Circle::with_center(yoke, YOKE_OUTER);
    outer
        .into_styled(PrimitiveStyle::with_fill(color))
        .draw(fbuf)
        .ok();
    Circle::with_center(yoke, YOKE_INNER)
        .into_styled(PrimitiveStyle::with_fill(BG))
        .draw(fbuf)
        .ok();
    // Cut the top half off the ring, turning it into a U.
    //
    // The cut is taken from the circle's own `bounding_box`, never recomputed
    // from `yoke` and `YOKE_OUTER`. An even diameter cannot be centred on a
    // pixel, so `with_center` biases the disc right and down by one: d=54 spans
    // `yoke.x - 26 ..= yoke.x + 27`, not `-27 ..= +26`. A rectangle derived the
    // obvious way misses the disc's rightmost column, and the surviving pixels
    // of that column show up as a stray nub on the right arm of the yoke.
    let bb = outer.bounding_box();
    Rectangle::new(bb.top_left, Size::new(bb.size.width, bb.size.height / 2))
        .into_styled(PrimitiveStyle::with_fill(BG))
        .draw(fbuf)
        .ok();

    // Stand, starting high enough to overlap the solid part of the yoke rather
    // than merely touching it - a one-pixel seam here is very visible.
    let yoke_bottom = yoke.y + YOKE_INNER as i32 / 2;
    let base_y = yoke.y + 42;
    Rectangle::new(
        Point::new(W / 2 - STROKE / 2, yoke_bottom),
        Size::new(STROKE as u32, (base_y - yoke_bottom) as u32),
    )
    .into_styled(PrimitiveStyle::with_fill(color))
    .draw(fbuf)
    .ok();
    Rectangle::new(Point::new(W / 2 - 16, base_y), Size::new(33, STROKE as u32))
        .into_styled(PrimitiveStyle::with_fill(color))
        .draw(fbuf)
        .ok();

    // Capsule body last, over the yoke's cuts. A corner radius of half the
    // width is what makes it a capsule rather than a rounded box.
    RoundedRectangle::new(
        Rectangle::with_center(Point::new(W / 2, H / 2 - 18), Size::new(22, 38)),
        CornerRadii::new(Size::new(11, 11)),
    )
    .into_styled(PrimitiveStyle::with_fill(color))
    .draw(fbuf)
    .ok();
}

/// What the battery gauge shows. `percent` is already mapped off the cell
/// voltage by the caller - the UI does not know about millivolts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
}

/// Vertical centre of the two status badges either side of the page dots.
///
/// Not the top edge of the panel, and that matters here: the wifi dot used to
/// live at `(W - 16, 16)`, which is 147px from centre on a panel whose glass
/// stops at 116. It was drawn into the framebuffer on every idle frame and
/// clipped away by the bezel every time - the indicator has never actually been
/// visible. Everything on this row sits inside r=108, clear of the ring's inner
/// edge at r=111.
const BADGE_Y: i32 = 36;
/// Left edge of the battery body. Its full extent including the nub is 73..94,
/// centred on 83.5, which the wifi dot at x=156 mirrors about the vertical axis.
const BATT_X: i32 = 73;
const WIFI_DOT_X: i32 = 156;

fn draw_wifi_dot(fbuf: &mut RawFrameBuf<Rgb565, &mut [u8]>, ok: bool) {
    let color = if ok { GREEN } else { RED };
    Circle::with_center(Point::new(WIFI_DOT_X, BADGE_Y), 5)
        .into_styled(PrimitiveStyle::with_fill(color))
        .draw(fbuf)
        .ok();
}

/// Battery badge: a 20x11 pill with a fill bar, a nub, and the percentage
/// underneath.
///
/// The number is there because the glyph alone can't distinguish "half" from
/// "just under half" at 16 pixels of bar, and this is a device you carry
/// around - the difference between 45% and 15% is the whole reason to look.
///
/// Colour is a plain three-band split rather than a gradient: green down to
/// 50%, amber down to 20%, red below. While charging it stays green whatever
/// the level, since the level is on its way up.
fn draw_battery(fbuf: &mut RawFrameBuf<Rgb565, &mut [u8]>, batt: Battery) {
    let pct = batt.percent.min(100) as i32;
    let color = if batt.charging {
        GREEN
    } else if pct >= 50 {
        GREEN
    } else if pct >= 20 {
        AMBER
    } else {
        RED
    };

    // Body outline and the nub on the positive end.
    Rectangle::new(Point::new(BATT_X, BADGE_Y - 5), Size::new(20, 11))
        .into_styled(PrimitiveStyle::with_stroke(color, 1))
        .draw(fbuf)
        .ok();
    Rectangle::new(Point::new(BATT_X + 20, BADGE_Y - 2), Size::new(2, 5))
        .into_styled(PrimitiveStyle::with_fill(color))
        .draw(fbuf)
        .ok();

    // Fill bar in the 16x7 interior. A cell that is nearly flat but not dead
    // still gets one column, so "almost empty" and "gauge not reading" don't
    // look the same.
    let fill = (16 * pct / 100).clamp(if pct > 0 { 1 } else { 0 }, 16);
    if fill > 0 {
        Rectangle::new(Point::new(BATT_X + 2, BADGE_Y - 3), Size::new(fill as u32, 7))
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(fbuf)
            .ok();
    }

    // Charging bolt, drawn over the fill in white so it reads at any level.
    // Two triangles is all there is room for at 7px tall, but the silhouette is
    // recognisable enough to mean "plugged in".
    if batt.charging {
        let bolt = PrimitiveStyle::with_fill(WHITE);
        Triangle::new(
            Point::new(BATT_X + 12, BADGE_Y - 3),
            Point::new(BATT_X + 7, BADGE_Y + 1),
            Point::new(BATT_X + 11, BADGE_Y + 1),
        )
        .into_styled(bolt)
        .draw(fbuf)
        .ok();
        Triangle::new(
            Point::new(BATT_X + 8, BADGE_Y + 3),
            Point::new(BATT_X + 13, BADGE_Y - 1),
            Point::new(BATT_X + 9, BADGE_Y - 1),
        )
        .into_styled(bolt)
        .draw(fbuf)
        .ok();
    }

    let style = MonoTextStyle::new(&FONT_6X10, GREY);
    let text_style = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Middle)
        .build();
    Text::with_text_style(
        &format!("{pct}%"),
        Point::new(BATT_X + 10, BADGE_Y + 14),
        style,
        text_style,
    )
    .draw(fbuf)
    .ok();
}

/// Cards in the swipe stack: the assistant (0), the configure actions (1) and the
/// read-only settings list (2).
const CARD_COUNT: usize = 3;

/// Page indicator along the top: one dot per card, the current one white and
/// the rest greyed out. This sits where the round panel has the most room to
/// spare, unlike the edges, and it says "there is another card" without having
/// to encode which direction to swipe.
fn page_dots(fbuf: &mut RawFrameBuf<Rgb565, &mut [u8]>, active: usize) {
    const DIAMETER: u32 = 6;
    const SPACING: i32 = 14;
    let left = W / 2 - (CARD_COUNT as i32 - 1) * SPACING / 2;
    for i in 0..CARD_COUNT {
        let color = if i == active { WHITE } else { GREY };
        Circle::with_center(Point::new(left + i as i32 * SPACING, 20), DIAMETER)
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(fbuf)
            .ok();
    }
}

fn draw_menu(fbuf: &mut RawFrameBuf<Rgb565, &mut [u8]>, title: &str, rows: &[MenuRow]) {
    ring(fbuf, DIM);

    let title_style = MonoTextStyle::new(&FONT_9X15_BOLD, WHITE);
    let centered = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Middle)
        .build();
    Text::with_text_style(title, Point::new(W / 2, 34), title_style, centered)
        .draw(fbuf)
        .ok();

    for (i, row) in rows.iter().enumerate() {
        let top = menu_row_top(i);
        let accent = if row.danger { RED } else { GREY };
        let fill = PrimitiveStyleBuilder::new()
            .fill_color(ROW_BG)
            .stroke_color(accent)
            .stroke_width(1)
            .build();
        RoundedRectangle::new(
            Rectangle::new(
                Point::new(MENU_X0, top),
                Size::new((MENU_X1 - MENU_X0) as u32, MENU_ROW_H as u32),
            ),
            CornerRadii::new(Size::new(8, 8)),
        )
        .into_styled(fill)
        .draw(fbuf)
        .ok();

        let mid = top + MENU_ROW_H / 2;
        let label_color = if row.danger { RED } else { WHITE };
        let label_style = MonoTextStyle::new(&FONT_9X15_BOLD, label_color);

        match (&row.value, row.stepper) {
            (Some(value), true) => {
                // "-  Label  value  +" : label centered, stepper glyphs pinned
                // to the tap zones so what you press is what you see.
                let glyph = MonoTextStyle::new(&FONT_9X15_BOLD, BLUE);
                Text::with_text_style(
                    "-",
                    Point::new(MENU_X0 + MENU_STEP_ZONE / 2, mid),
                    glyph,
                    centered,
                )
                .draw(fbuf)
                .ok();
                Text::with_text_style(
                    "+",
                    Point::new(MENU_X1 - MENU_STEP_ZONE / 2, mid),
                    glyph,
                    centered,
                )
                .draw(fbuf)
                .ok();
                let small = MonoTextStyle::new(&FONT_6X10, GREY);
                Text::with_text_style(&row.label, Point::new(W / 2, mid - 8), small, centered)
                    .draw(fbuf)
                    .ok();
                Text::with_text_style(value, Point::new(W / 2, mid + 8), label_style, centered)
                    .draw(fbuf)
                    .ok();
            }
            (Some(value), false) => {
                let small = MonoTextStyle::new(&FONT_6X10, GREY);
                Text::with_text_style(&row.label, Point::new(W / 2, mid - 8), small, centered)
                    .draw(fbuf)
                    .ok();
                Text::with_text_style(value, Point::new(W / 2, mid + 8), label_style, centered)
                    .draw(fbuf)
                    .ok();
            }
            (None, _) => {
                Text::with_text_style(&row.label, Point::new(W / 2, mid), label_style, centered)
                    .draw(fbuf)
                    .ok();
            }
        }
    }

    // Configure is the only menu card, so it is always the second dot.
    page_dots(fbuf, 1);
}

fn draw_settings(fbuf: &mut RawFrameBuf<Rgb565, &mut [u8]>, entries: &[SettingEntry], offset: usize) {
    ring(fbuf, DIM);

    let centered = TextStyleBuilder::new()
        .alignment(Alignment::Center)
        .baseline(Baseline::Middle)
        .build();
    let title_style = MonoTextStyle::new(&FONT_9X15_BOLD, WHITE);
    Text::with_text_style("Settings", Point::new(W / 2, 32), title_style, centered)
        .draw(fbuf)
        .ok();

    let label_style = MonoTextStyle::new(&FONT_6X10, GREY);
    let last_visible = entries.len().min(offset + SETTINGS_PAGE_ROWS);
    for (slot, index) in (offset..last_visible).enumerate() {
        let entry = &entries[index];
        let top = SETTINGS_TOP + slot as i32 * SETTINGS_ROW_H;

        Text::with_text_style(&entry.label, Point::new(W / 2, top + 7), label_style, centered)
            .draw(fbuf)
            .ok();
        let value_color = if entry.placeholder { AMBER } else { WHITE };
        wrapped_text(fbuf, &entry.value, top + 23, 28, 2, value_color);

        // No rule under the final entry - there is nothing below it to divide
        // it from, and a trailing line reads as "more, cut off".
        if index + 1 < entries.len() {
            let y = top + SETTINGS_ROW_H - 2;
            Line::new(Point::new(44, y), Point::new(196, y))
                .into_styled(PrimitiveStyle::with_stroke(DIM, 1))
                .draw(fbuf)
                .ok();
        }
    }

    if !entries.is_empty() {
        let counter = format!("{}-{} of {}", offset + 1, last_visible, entries.len());
        Text::with_text_style(&counter, Point::new(W / 2, 184), label_style, centered)
            .draw(fbuf)
            .ok();
    }
    status_text(fbuf, "swipe up/down", GREY);
    page_dots(fbuf, 2);
}

/// Renders one state into `frame` (a persistent 240*240*2-byte PSRAM buffer
/// you own and reuse across calls) and flushes it to the display.
pub async fn render<DI, RST>(
    display: &mut Display<DI, GC9A01, RST>,
    frame: &mut [u8],
    state: &UiState,
    anim_tick: u32,
) where
    DI: Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    let mut fbuf = RawFrameBuf::<Rgb565, _>::new(
        &mut frame[..],
        DISPLAY_WIDTH as usize,
        DISPLAY_HEIGHT as usize,
    );
    fbuf.clear(BG).ok();

    match state {
        UiState::Boot { step, progress } => {
            // Unfilled track plus a filled arc, so how much is left to wait is
            // visible at a glance rather than just "something is happening".
            ring(&mut fbuf, DIM);
            let sweep = (*progress).min(100) as f32 * 3.6;
            if sweep > 0.0 {
                Arc::with_center(
                    CENTER,
                    (W - 8) as u32,
                    // Start at 12 o'clock and fill clockwise, which is what a
                    // progress ring is expected to do.
                    Angle::from_degrees(90.0),
                    Angle::from_degrees(-sweep),
                )
                .into_styled(PrimitiveStyle::with_stroke(BLUE, 5))
                .draw(&mut fbuf)
                .ok();
            }
            let title_style = MonoTextStyle::new(&FONT_9X15_BOLD, WHITE);
            let centered = TextStyleBuilder::new()
                .alignment(Alignment::Center)
                .baseline(Baseline::Middle)
                .build();
            Text::with_text_style("starting up", CENTER, title_style, centered)
                .draw(&mut fbuf)
                .ok();
            wrapped_text(&mut fbuf, step, H / 2 + 26, 28, 2, GREY);
        }
        UiState::Idle { wifi_ok, exchanges, battery } => {
            ring(&mut fbuf, DIM);
            draw_mic(&mut fbuf, GREY);
            // Sits between the mic glyph (which ends at y=135) and the status
            // line at y=206, and is drawn only when there is history to
            // report - a fresh device looks exactly as it did before.
            if *exchanges > 0 {
                let plural = if *exchanges == 1 { "" } else { "s" };
                let label = format!("{exchanges} exchange{plural}");
                let style = MonoTextStyle::new(&FONT_6X10, DIM);
                let centered = TextStyleBuilder::new()
                    .alignment(Alignment::Center)
                    .baseline(Baseline::Middle)
                    .build();
                Text::with_text_style(&label, Point::new(W / 2, 172), style, centered)
                    .draw(&mut fbuf)
                    .ok();
            }
            status_text(&mut fbuf, "tap to talk", GREY);
            if let Some(batt) = battery {
                draw_battery(&mut fbuf, *batt);
            }
            draw_wifi_dot(&mut fbuf, *wifi_ok);
            page_dots(&mut fbuf, 0);
        }
        UiState::Listening { level } => {
            // Ring radius tracks the live mic level, so the user can see the
            // device actually hearing them (and see when it hears nothing).
            let level = (*level).min(10) as i32;
            let r = (110 + level * 12).min(W - 8) as u32;
            Circle::with_center(CENTER, r)
                .into_styled(PrimitiveStyle::with_stroke(BLUE, 5))
                .draw(&mut fbuf)
                .ok();
            draw_mic(&mut fbuf, WHITE);
            // Two rows: the hint no longer fits beside the state name on one
            // line inside the bezel at this y (~25 characters of headroom).
            wrapped_text(&mut fbuf, "listening...", H - 50, 26, 1, BLUE);
            status_text(&mut fbuf, "tap send / swipe cancel", GREY);
        }
        UiState::Thinking { tool } => {
            ring(&mut fbuf, PURPLE);
            // Rotating arc spinner.
            let angle = ((anim_tick * 24) % 360) as f32;
            Arc::with_center(
                CENTER,
                (W - 8) as u32,
                Angle::from_degrees(angle),
                Angle::from_degrees(90.0),
            )
            .into_styled(PrimitiveStyle::with_stroke(WHITE, 5))
            .draw(&mut fbuf)
            .ok();
            big_label(&mut fbuf, "...", WHITE);
            let label = match tool {
                Some(t) => t.as_str(),
                None => "thinking...",
            };
            // Same two-row split as Listening: the state name sits above the
            // gesture hint, which doesn't fit beside it inside the bezel.
            wrapped_text(&mut fbuf, label, H - 50, 26, 1, PURPLE);
            status_text(&mut fbuf, "swipe to cancel", GREY);
        }
        UiState::Speaking { snippet } => {
            // The equaliser bars that used to sit here were pretty but cost
            // most of the panel: the reply got four lines and was routinely
            // cut off mid-sentence. The whole centre is text now.
            //
            // Deliberately static, and filled rather than a ring with a moving
            // sweep. Every frame is a full-framebuffer render plus a 115KB SPI
            // push, and it is joined with the I2S write - `join` waits for the
            // slowest arm, so a frame that outlasts its ~170ms DMA block drags
            // the block boundary out, and with I2S_TX_STOP_EN cleared that
            // overshoot is the TX unit repeating its last sample. Anything
            // animated here needs a frame on a timer whatever the audio is
            // doing; static means a redraw only when the caption changes, a
            // handful per reply. The pulsing lives on the WS2812 instead, which
            // is 24 bits over RMT and shares neither the SPI bus, GDMA nor
            // PSRAM with the audio path. A solid ring is also cheaper to
            // rasterise than the two arcs it replaces, which needed trig per
            // pixel across their bounding boxes.
            //
            // Same `ring()` every other card uses, so the green sits at exactly
            // the weight of the Listening grey and the Thinking purple.
            ring(&mut fbuf, GREEN);
            // 30x11 is what the inscribed circle actually allows: a 30-char
            // line of FONT_6X10 is 180px wide, and the outermost line of an
            // 11-line block sits 60px off centre, where the circle is still
            // 96px wide either side of the axis.
            wrapped_text(&mut fbuf, snippet, H / 2, 30, 11, WHITE);
        }
        UiState::Error(msg) => {
            ring(&mut fbuf, RED);
            big_label(&mut fbuf, "!", RED);
            // 22 chars at y=188, not 28 at y=200. The old numbers were the one
            // place on any card where a full-width line actually collided with
            // the bezel: two lines centered on y=200 sit at y=194 and y=206, and
            // at y=206 the inscribed circle is only 78px either side of the axis
            // while 28 chars of FONT_6X10 is 84. Lifting the block to y=188 and
            // capping it at 132px wide leaves ~23px of gutter on the worst line.
            wrapped_text(&mut fbuf, msg, H - 52, 22, 2, RED);
        }
        UiState::Menu { title, rows } => draw_menu(&mut fbuf, title, rows),
        UiState::Settings { entries, offset } => draw_settings(&mut fbuf, entries, *offset),
        UiState::Portal { ap_ssid, url, clients } => {
            ring(&mut fbuf, AMBER);
            let title_style = MonoTextStyle::new(&FONT_9X15_BOLD, AMBER);
            let centered = TextStyleBuilder::new()
                .alignment(Alignment::Center)
                .baseline(Baseline::Middle)
                .build();
            Text::with_text_style("Wi-Fi Setup", Point::new(W / 2, 46), title_style, centered)
                .draw(&mut fbuf)
                .ok();
            wrapped_text(&mut fbuf, "join this network:", 78, 28, 1, GREY);
            wrapped_text(&mut fbuf, ap_ssid, 96, 26, 1, WHITE);
            wrapped_text(&mut fbuf, "then browse to:", 124, 28, 1, GREY);
            wrapped_text(&mut fbuf, url, 142, 26, 1, WHITE);
            let status = if *clients > 0 { "device connected" } else { "waiting for device" };
            let color = if *clients > 0 { GREEN } else { GREY };
            wrapped_text(&mut fbuf, status, 172, 28, 1, color);
            status_text(&mut fbuf, "tap to cancel", GREY);
        }
        UiState::Notice { title, body, color } => {
            let c = match color {
                NoticeColor::Info => BLUE,
                NoticeColor::Warn => AMBER,
                NoticeColor::Good => GREEN,
            };
            ring(&mut fbuf, c);
            let title_style = MonoTextStyle::new(&FONT_9X15_BOLD, c);
            let centered = TextStyleBuilder::new()
                .alignment(Alignment::Center)
                .baseline(Baseline::Middle)
                .build();
            Text::with_text_style(title, Point::new(W / 2, H / 2 - 26), title_style, centered)
                .draw(&mut fbuf)
                .ok();
            wrapped_text(&mut fbuf, body, H / 2 + 12, 28, 4, WHITE);
        }
    }

    display
        .show_raw_data(0, 0, DISPLAY_WIDTH, DISPLAY_HEIGHT, frame)
        .await
        .ok();
}
