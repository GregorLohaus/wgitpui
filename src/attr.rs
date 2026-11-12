//! Line attribution ("who changed this, in which commit, when"): the gutter cell and the hover
//! card, shared by the diff viewer and the conflict resolver.

use crate::font::{MONO, UI};
use crate::git::{self, BlameCommit, UNCOMMITTED};
use crate::ui::{theme, Color, Rect, Ui};

/// Fresh changes glow warm; old ones fade to the accent and then to faint.
pub fn age_color(time: i64) -> Color {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let days = ((now - time).max(0) as f32) / 86_400.0;
    let t = (1.0 - (days + 1.0).ln() / (3.0 * 365.0f32).ln()).clamp(0.0, 1.0);
    theme::FAINT.mix(theme::ACCENT, (t * 1.6).min(1.0)).mix(theme::ORANGE, ((t - 0.6) * 2.5).clamp(0.0, 1.0))
}

pub fn uncommitted(c: &BlameCommit) -> bool {
    c.hash == UNCOMMITTED
}

/// One gutter cell. `text` is false for continuation lines of the same commit (bar only).
pub fn cell(ui: &mut Ui, r: Rect, c: &BlameCommit, text: bool, hovered: bool) {
    let color = if uncommitted(c) { theme::YELLOW } else { age_color(c.time) };
    if hovered {
        ui.p.rect(r, theme::ACCENT.alpha(0.08));
    }
    ui.p.rect(Rect::new(r.right() - 3.0, r.y, 2.0, r.h), color.alpha(0.8));
    if !text {
        return;
    }
    let cy = r.cy();
    let w = r.w - 14.0;
    if uncommitted(c) {
        ui.p.text_fit(r.x + 8.0, cy, w, UI, 11.0, "uncommitted", theme::YELLOW);
        return;
    }
    let short = &c.hash[..7.min(c.hash.len())];
    let age = git::short_age(c.time);
    let hw = ui.p.measure(MONO, 10.5, short);
    let aw = ui.p.measure(UI, 10.5, &age);
    // Narrow gutters drop the author first, then the hash; the age always stays.
    let mut x = r.x + 8.0;
    let right = r.x + w;
    let author_room = right - x - aw - hw - 16.0;
    if author_room > 28.0 {
        x += ui.p.text_fit(x, cy, author_room, UI, 11.0, &c.author, theme::DIM) + 8.0;
    }
    ui.p.text(x, cy, UI, 10.5, &age, color);
    x += aw + 8.0;
    if x + hw <= right + 2.0 {
        ui.p.text(x, cy, MONO, 10.5, short, theme::FAINT);
    }
}

/// Hover card with the full story for one line.
pub fn tooltip(ui: &mut Ui, at: (f32, f32), c: &BlameCommit) {
    let lines: Vec<(String, f32, Color)> = if uncommitted(c) {
        vec![("Not committed yet".into(), 12.5, theme::YELLOW), ("This line only exists in your working copy or index.".into(), 11.5, theme::DIM)]
    } else {
        vec![
            (c.summary.clone(), 12.5, theme::TEXT),
            (format!("{}  ·  {}", c.author, git::abs_time(c.time)), 11.5, theme::DIM),
            (format!("{}  ·  {}  ·  click to open", &c.hash[..10.min(c.hash.len())], git::rel_time(c.time)), 11.0, theme::FAINT),
        ]
    };
    let w = lines.iter().map(|(t, s, _)| ui.p.measure(UI, *s, t)).fold(0.0f32, f32::max).min(460.0) + 28.0;
    let h = 14.0 + lines.len() as f32 * 19.0;
    let s = ui.screen;
    let x = (at.0 + 14.0).min(s.right() - w - 8.0);
    let y = if at.1 + 18.0 + h > s.bottom() { at.1 - h - 8.0 } else { at.1 + 18.0 };
    let r = Rect::new(x, y, w, h);
    ui.p.rounded(Rect::new(r.x, r.y + 4.0, r.w, r.h), 9.0, Color::hex(0x000000).alpha(0.4));
    ui.p.rounded(r, 8.0, theme::RAISED);
    ui.p.rounded_stroke(r, 8.0, 1.0, theme::BORDER.mix(theme::TEXT, 0.12));
    let mut cy = r.y + 16.0;
    for (t, size, color) in &lines {
        ui.p.text_fit(r.x + 14.0, cy, r.w - 28.0, UI, *size, t, *color);
        cy += 19.0;
    }
}
