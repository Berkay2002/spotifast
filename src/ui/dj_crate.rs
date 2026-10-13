//! The queue as a crate of records at the top of the DJ page: the playing sleeve with its vinyl
//! half out, the next sleeves fanned behind it in perspective.
//!
//! It only draws. The motion is its own: the disc slides out when a song starts, slides back and
//! the crate shifts one sleeve when the song changes, new sleeves are dealt in with a green dot,
//! and the label (the cover, cropped to a circle) turns while the song plays. With less motion
//! asked for, each of them shows where it ends at once and the label stands still.

use std::time::Duration;

use egui::epaint::Vertex;
use egui::{Align2, Color32, Mesh, Painter, Pos2, Rect, Sense, Shape, TextureId, pos2, vec2};

use super::widgets;
use crate::api::models::PlayableItem;
use crate::app::App;
use crate::motion;
use crate::theme::{self, Icon, Palette};

const SLEEVE: f32 = 175.0;
const LABEL_H: f32 = 24.0;
const TITLES_H: f32 = 44.0;
const MAX_NEXT: usize = 12;
/// Seconds: the disc sliding out, the crate shifting, a sleeve being dealt in.
const SLIDE: f32 = 0.4;
const SHIFT: f32 = 0.35;
const DEAL: f32 = 0.35;
/// How long a sleeve dealt in keeps its green dot.
const DOT: f64 = 6.0;
const REVOLUTIONS_PER_SECOND: f32 = 0.55;
/// The flat label's radius as a share of the disc's, as drawn in `assets/dj/vinyl-disc.png`.
const LABEL_SHARE: f32 = 0.336;
const UV: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

#[derive(Clone)]
struct Sleeve {
    uri: String,
    cover: Option<String>,
    title: String,
    artist: String,
}

impl Sleeve {
    fn of(item: &PlayableItem) -> Self {
        Self {
            uri: item.uri().to_string(),
            cover: item.image(300).map(str::to_string),
            title: item.name().to_string(),
            artist: item.subtitle(),
        }
    }
}

/// What the crate remembers between frames.
#[derive(Clone, Default)]
struct Memo {
    /// The song on top, and when it became that.
    uri: String,
    changed: f64,
    /// The song that was on top before, while it leaves.
    gone: Option<Sleeve>,
    last: Option<Sleeve>,
    /// How far the label has turned, in radians.
    angle: f32,
    /// The sleeves of the last frame, and the ones dealt in since, with the time.
    seen: Vec<String>,
    born: Vec<(String, f64)>,
    primed: bool,
}

/// 0 to 1 over `duration` seconds, slowing toward the end; 1 at once when there is no duration.
fn ease(elapsed: f32, duration: f32) -> f32 {
    if duration <= 0.0 {
        return 1.0;
    }
    let t = (elapsed / duration).clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

fn disc_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let id = egui::Id::new("dj-vinyl-disc");
    if let Some(texture) = ctx.data(|data| data.get_temp::<egui::TextureHandle>(id)) {
        return Some(texture);
    }
    let rgba = image::load_from_memory(include_bytes!("../../assets/dj/vinyl-disc.png"))
        .ok()?
        .to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    let texture = ctx.load_texture("dj-vinyl-disc", image, egui::TextureOptions::LINEAR);
    ctx.data_mut(|data| data.insert_temp(id, texture.clone()));
    Some(texture)
}

/// A sleeve in slight perspective: the right edge is taller than the left, the bottoms level. Cut
/// into strips so the picture does not kink along a diagonal.
#[allow(clippy::too_many_arguments)]
fn paint_sleeve(
    painter: &Painter,
    texture: Option<TextureId>,
    fill: Color32,
    left: f32,
    width: f32,
    base: f32,
    height_left: f32,
    height_right: f32,
    tint: Color32,
) {
    const STRIPS: usize = 6;
    let top = |t: f32| base - (height_left + (height_right - height_left) * t);
    match texture {
        Some(id) => {
            let mut mesh = Mesh::with_texture(id);
            for i in 0..=STRIPS {
                let t = i as f32 / STRIPS as f32;
                let x = left + width * t;
                mesh.vertices.push(Vertex {
                    pos: pos2(x, top(t)),
                    uv: pos2(t, 0.0),
                    color: tint,
                });
                mesh.vertices.push(Vertex {
                    pos: pos2(x, base),
                    uv: pos2(t, 1.0),
                    color: tint,
                });
            }
            for i in 0..STRIPS as u32 {
                let a = i * 2;
                mesh.add_triangle(a, a + 1, a + 2);
                mesh.add_triangle(a + 1, a + 3, a + 2);
            }
            painter.add(Shape::mesh(mesh));
        }
        None => {
            painter.add(Shape::convex_polygon(
                vec![
                    pos2(left, top(0.0)),
                    pos2(left + width, top(1.0)),
                    pos2(left + width, base),
                    pos2(left, base),
                ],
                fill,
                egui::Stroke::NONE,
            ));
        }
    }
}

/// The cover cropped to a circle, turned by `angle`: the positions stay, the picture it samples
/// moves.
fn paint_label(
    painter: &Painter,
    texture: TextureId,
    center: Pos2,
    radius: f32,
    angle: f32,
    tint: Color32,
) {
    const POINTS: usize = 48;
    let mut mesh = Mesh::with_texture(texture);
    mesh.vertices.push(Vertex {
        pos: center,
        uv: pos2(0.5, 0.5),
        color: tint,
    });
    for i in 0..=POINTS {
        let a = i as f32 / POINTS as f32 * std::f32::consts::TAU;
        let (sin, cos) = a.sin_cos();
        let (uv_sin, uv_cos) = (a + angle).sin_cos();
        mesh.vertices.push(Vertex {
            pos: center + vec2(cos, sin) * radius,
            uv: pos2(0.5 + 0.5 * uv_cos, 0.5 + 0.5 * uv_sin),
            color: tint,
        });
    }
    for i in 0..POINTS as u32 {
        mesh.add_triangle(0, 1 + i, 2 + i);
    }
    painter.add(Shape::mesh(mesh));
}

#[allow(clippy::too_many_arguments)]
fn paint_disc(
    painter: &Painter,
    palette: &Palette,
    disc: Option<&egui::TextureHandle>,
    center: Pos2,
    radius: f32,
    label: Option<(TextureId, f32)>,
    alpha: f32,
) {
    let tint = Color32::WHITE.gamma_multiply(alpha);
    match disc {
        Some(disc) => {
            painter.image(
                disc.id(),
                Rect::from_center_size(center, vec2(radius * 2.0, radius * 2.0)),
                UV,
                tint,
            );
        }
        None => {
            painter.circle_filled(center, radius, Color32::from_gray(18).gamma_multiply(alpha));
        }
    }
    if let Some((texture, angle)) = label {
        paint_label(
            painter,
            texture,
            center,
            radius * LABEL_SHARE * 1.03,
            angle,
            tint,
        );
    }
    painter.circle_filled(center, radius * 0.035, palette.window.gamma_multiply(alpha));
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let now_playing = app.now_playing();
    let is_playing = now_playing.as_ref().is_some_and(|n| n.playing);
    let progress = now_playing
        .as_ref()
        .filter(|n| n.duration_ms > 0)
        .map_or(0.0, |n| {
            (n.position_ms as f32 / n.duration_ms as f32).clamp(0.0, 1.0)
        });
    let playing = now_playing.map(|n| Sleeve {
        uri: n.uri,
        cover: n.art_url.or(n.art_small),
        title: n.title,
        artist: n.subtitle,
    });
    let next: Vec<Sleeve> = app
        .queue
        .get()
        .map(|queue| queue.queue.iter().take(MAX_NEXT).map(Sleeve::of).collect())
        .unwrap_or_default();
    if playing.is_none() && next.is_empty() {
        return;
    }

    let ctx = ui.ctx().clone();
    let (now, dt) = ctx.input(|input| (input.time, input.stable_dt));
    let reduced = motion::reduced(&ctx);
    let id = egui::Id::new("dj-crate");
    let mut memo: Memo = ctx.data(|data| data.get_temp(id)).unwrap_or_default();
    let uri = playing.as_ref().map_or("", |sleeve| sleeve.uri.as_str());
    if !memo.primed || memo.uri != uri {
        memo.gone = if memo.primed { memo.last.take() } else { None };
        memo.uri = uri.to_string();
        memo.changed = now;
    }
    if memo.primed {
        for sleeve in &next {
            if !memo.seen.contains(&sleeve.uri) && !memo.born.iter().any(|(u, _)| *u == sleeve.uri)
            {
                memo.born.push((sleeve.uri.clone(), now));
            }
        }
    }
    memo.seen = next.iter().map(|sleeve| sleeve.uri.clone()).collect();
    memo.born.retain(|(_, at)| now - at < DOT);
    memo.primed = true;
    memo.last = playing.clone();
    if is_playing && !reduced {
        memo.angle = (memo.angle + dt * REVOLUTIONS_PER_SECOND * std::f32::consts::TAU)
            % std::f32::consts::TAU;
    }

    let secs = |seconds: f32| motion::seconds(&ctx, seconds);
    let age = (now - memo.changed) as f32;
    let shift = ease(age, secs(SHIFT));
    let slide = ease(age - secs(0.12), secs(SLIDE));
    let dealing = memo
        .born
        .iter()
        .any(|(_, at)| ((now - at) as f32) < secs(DEAL));
    if age < secs(SLIDE + 0.12) || dealing {
        ctx.request_repaint();
    }
    if is_playing && !reduced {
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    let art = app.backend.art();
    let disc = disc_texture(&ctx);
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), LABEL_H + SLEEVE + TITLES_H),
        Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.text(
        rect.left_top() + vec2(2.0, 2.0),
        Align2::LEFT_TOP,
        "Up next",
        theme::regular(12.5),
        palette.dim,
    );

    let top = Rect::from_min_size(
        pos2(rect.left(), rect.top() + LABEL_H),
        vec2(SLEEVE, SLEEVE),
    );
    let base = top.bottom() - 4.0;
    let first = top.right() + 42.0;
    let step = |k: usize| 108.0 + 38.0 * 0.7_f32.powi(k as i32);
    let mut lefts = Vec::with_capacity(next.len());
    let mut x = first;
    for k in 0..next.len() {
        lefts.push(x);
        x += step(k);
    }
    let shifted = if memo.gone.is_some() {
        step(0) * (1.0 - shift)
    } else {
        0.0
    };

    // far sleeves first, so the near ones lie over them
    for (k, sleeve) in next.iter().enumerate().rev() {
        let born = memo
            .born
            .iter()
            .find(|(u, _)| *u == sleeve.uri)
            .map(|(_, at)| *at);
        let appear = born.map_or(1.0, |at| ease((now - at) as f32, secs(DEAL)));
        let left = lefts[k] + shifted + (1.0 - appear) * 36.0;
        if left > rect.right() {
            continue;
        }
        let width = step(k) - 5.0;
        let height = (168.0 - 3.4 * k as f32).max(118.0);
        let shade = 255 - (k * 7).min(60) as u8;
        let tint = Color32::from_gray(shade).gamma_multiply(appear);
        let texture = sleeve
            .cover
            .as_deref()
            .and_then(|url| widgets::cover_texture(ui, url, vec2(width, height), art));
        paint_sleeve(
            &painter,
            texture.map(|t| t.id),
            palette.surface_hover.gamma_multiply(appear),
            left,
            width,
            base,
            height,
            height * 1.06,
            tint,
        );
        if born.is_some() {
            painter.circle_filled(
                pos2(left + width * 0.5, base - height * 1.03 - 8.0),
                2.5,
                palette.accent.gamma_multiply(appear),
            );
        }
        let clip = painter.with_clip_rect(Rect::from_min_size(
            pos2(left, base + 4.0),
            vec2(step(k) - 8.0, 40.0),
        ));
        clip.text(
            pos2(left + 2.0, base + 8.0),
            Align2::LEFT_TOP,
            &sleeve.title,
            theme::regular(12.0),
            palette.secondary.gamma_multiply(appear),
        );
        clip.text(
            pos2(left + 2.0, base + 24.0),
            Align2::LEFT_TOP,
            &sleeve.artist,
            theme::regular(11.0),
            palette.dim.gamma_multiply(appear),
        );
    }

    let radius = SLEEVE / 2.0;
    let out = |t: f32| pos2(top.center().x + radius * t, top.center().y);

    // the song that just left: its disc goes back in under its sleeve
    if let Some(gone) = &memo.gone
        && shift < 1.0
    {
        let texture = gone
            .cover
            .as_deref()
            .and_then(|url| widgets::cover_texture(ui, url, top.size(), art));
        paint_disc(
            &painter,
            &palette,
            disc.as_ref(),
            out(1.0 - shift),
            radius,
            texture.map(|t| (t.id, memo.angle)),
            1.0 - shift,
        );
    }

    if let Some(sleeve) = &playing {
        // from where the next sleeve was to the top of the crate
        let from = Rect::from_min_max(pos2(first, base - 168.0), pos2(first + step(0) - 5.0, base));
        let at = if memo.gone.is_some() {
            Rect::from_min_max(
                from.min + (top.min - from.min) * shift,
                from.max + (top.max - from.max) * shift,
            )
        } else {
            top
        };
        let texture = sleeve
            .cover
            .as_deref()
            .and_then(|url| widgets::cover_texture(ui, url, top.size(), art));
        paint_disc(
            &painter,
            &palette,
            disc.as_ref(),
            out(slide),
            radius,
            texture.map(|t| (t.id, memo.angle)),
            1.0,
        );
        match texture {
            Some(texture) => {
                painter.image(texture.id, at, UV, Color32::WHITE);
            }
            None => {
                painter.rect_filled(at, 0.0, palette.surface_hover);
                theme::paint_icon(ui, Icon::Music, at, 56.0, palette.dim);
            }
        }
        let bar = Rect::from_min_size(pos2(at.left(), at.bottom() - 3.0), vec2(at.width(), 3.0));
        painter.rect_filled(bar, 0.0, Color32::from_black_alpha(110));
        painter.rect_filled(
            Rect::from_min_size(bar.min, vec2(bar.width() * progress, 3.0)),
            0.0,
            palette.accent,
        );
        let clip = painter.with_clip_rect(Rect::from_min_size(
            pos2(top.left(), top.bottom() + 6.0),
            vec2(SLEEVE + 90.0, 40.0),
        ));
        clip.text(
            pos2(top.left(), top.bottom() + 8.0),
            Align2::LEFT_TOP,
            &sleeve.title,
            theme::semibold(14.0),
            palette.text,
        );
        clip.text(
            pos2(top.left(), top.bottom() + 26.0),
            Align2::LEFT_TOP,
            &sleeve.artist,
            theme::regular(13.0),
            palette.secondary,
        );
    }

    // the sleeve that left slides away and fades
    if let Some(gone) = &memo.gone
        && shift < 1.0
    {
        let at = top.translate(vec2(-60.0 * shift, 0.0));
        let fade = 1.0 - shift;
        match gone
            .cover
            .as_deref()
            .and_then(|url| widgets::cover_texture(ui, url, top.size(), art))
        {
            Some(texture) => {
                painter.image(texture.id, at, UV, Color32::WHITE.gamma_multiply(fade));
            }
            None => {
                painter.rect_filled(at, 0.0, palette.surface_hover.gamma_multiply(fade));
            }
        }
    }

    ctx.data_mut(|data| data.insert_temp(id, memo));
}
