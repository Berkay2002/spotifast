//! The DJ page: the queue as a crate of records, a chat with Claude Code sitting on the page, and
//! the composer floating at the bottom. The DJ's settings are at the end of the file.
//!
//! The chat is read from the session's lines as turns: the listener's message, what the model
//! thought, what it did (tool rows, with the cards the app built for them), and what it said.
//!
//! The strings here are plain English for now (see `docs/_reference/dj.md`).

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Align2, Color32, CornerRadius, Frame, Id, Layout, Margin, Rect, Sense, Shape, Stroke,
    Ui, UiBuilder, pos2, vec2,
};

use crate::app::App;
use crate::dj::card::{Ask, CARD_SONGS};
use crate::dj::{Card, CardSong, Line, LineKind};
use crate::images::ArtLoader;
use crate::model::{Action, Page};
use crate::motion;
use crate::theme::{self, Icon, Palette};

use super::widgets;

/// The width of the chat's column, and of the composer.
const COLUMN: f32 = 760.0;
const COMPOSER_W: f32 = 760.0;
const COMPOSER_H: f32 = 56.0;
const ROW_H: f32 = 54.0;
/// Claude's colour, for its mark on the provider pill.
const CLAUDE: Color32 = Color32::from_rgb(0xd9, 0x77, 0x57);

/// What the chat's drawing needs besides the lines, and what it asks the app to do.
struct Cx<'a> {
    palette: Palette,
    art: &'a ArtLoader,
    actions: Vec<Action>,
}

/// One exchange: the listener's message and everything the DJ did and said in answer.
struct Turn<'a> {
    at: usize,
    user: Option<&'a Line>,
    thought: Option<&'a Line>,
    tools: Vec<&'a Line>,
    replies: Vec<&'a Line>,
    errors: Vec<&'a Line>,
}

fn turns(lines: &[Line]) -> Vec<Turn<'_>> {
    let mut out: Vec<Turn> = Vec::new();
    for line in lines {
        if line.kind == LineKind::User || out.is_empty() {
            out.push(Turn {
                at: out.len(),
                user: None,
                thought: None,
                tools: Vec::new(),
                replies: Vec::new(),
                errors: Vec::new(),
            });
        }
        if let Some(turn) = out.last_mut() {
            match line.kind {
                LineKind::User => turn.user = Some(line),
                LineKind::Thought => turn.thought = Some(line),
                LineKind::Tool => turn.tools.push(line),
                LineKind::Assistant => turn.replies.push(line),
                LineKind::Error => turn.errors.push(line),
            }
        }
    }
    out
}

/// Text that wraps at the page's width (`theme::text` stays on one line).
fn wrapped(ui: &mut Ui, text: &str, font: egui::FontId, color: Color32) {
    ui.add(egui::Label::new(egui::RichText::new(text).font(font).color(color)).wrap());
}

/// Text with `**bold**` runs, which is how the model marks what matters.
fn markdown(ui: &mut Ui, text: &str, color: Color32) {
    let mut job = LayoutJob::default();
    for (i, part) in text.split("**").enumerate() {
        if part.is_empty() {
            continue;
        }
        let font = if i % 2 == 1 {
            theme::semibold(15.0)
        } else {
            theme::regular(15.0)
        };
        job.append(
            part,
            0.0,
            TextFormat {
                font_id: font,
                color,
                line_height: Some(22.0),
                ..Default::default()
            },
        );
    }
    ui.add(egui::Label::new(job).wrap());
}

fn clock(ms: u32) -> String {
    format!("{}:{:02}", ms / 60_000, (ms / 1000) % 60)
}

/// Space that opens and closes over a short time, its body drawn only while it shows.
fn collapse(ui: &mut Ui, id: Id, open: bool, body: impl FnOnce(&mut Ui)) {
    let ctx = ui.ctx().clone();
    let t = ctx.animate_bool_with_time_and_easing(
        id.with("t"),
        open,
        motion::seconds(&ctx, 0.22),
        egui::emath::easing::cubic_out,
    );
    if t <= 0.0 {
        return;
    }
    let width = ui.available_width();
    let known: f32 = ctx.data(|data| data.get_temp(id.with("h"))).unwrap_or(0.0);
    let partial = (t < 1.0 && known > 0.0).then_some(known * t);
    let origin = ui.cursor().min;
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_size(origin, vec2(width, 4000.0)))
            .layout(Layout::top_down(Align::Min)),
    );
    if let Some(height) = partial {
        child.set_clip_rect(
            ui.clip_rect()
                .intersect(Rect::from_min_size(origin, vec2(width, height))),
        );
    }
    body(&mut child);
    let height = child.min_rect().height();
    ctx.data_mut(|data| data.insert_temp(id.with("h"), height));
    ui.allocate_space(vec2(width, partial.unwrap_or(height)));
    if partial.is_some() {
        ctx.request_repaint();
    }
}

/// Runs `add` in a column of `width` centred in the space that is left.
fn column<R>(ui: &mut Ui, width: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let free = ui.available_rect_before_wrap();
    let width = width.min(free.width());
    let rect = Rect::from_min_size(
        pos2(free.center().x - width / 2.0, free.top()),
        vec2(width, free.height()),
    );
    ui.scope_builder(UiBuilder::new().max_rect(rect), add).inner
}

pub fn show(app: &mut App, ui: &mut Ui) {
    let palette = app.palette;
    super::dj_crate::show(app, ui);
    ui.add_space(16.0);

    let blocker = app.dj.status.as_ref().and_then(crate::dj::session::blocker);
    if let Some(message) = &blocker {
        wrapped(ui, message, theme::regular(14.0), palette.warning);
        ui.add_space(8.0);
        let label = if app.dj.checking() {
            "Checking…"
        } else {
            "Check again"
        };
        if theme::soft_button(ui, &palette, Some(Icon::Refresh), label, false).clicked()
            && !app.dj.checking()
        {
            app.actions.push(Action::DjCheck);
        }
        ui.add_space(14.0);
    }
    let usable = blocker.is_none();

    title_row(app, ui);
    ui.add_space(8.0);

    if app.dj.lines.is_empty() && app.dj.approvals.is_empty() {
        ui.add_space(24.0);
        widgets::empty_state(
            ui,
            &palette,
            Icon::Sparkles,
            "Nothing playing yet",
            "Tell the DJ what you want: a mood, an artist, a decade. Or let it start from your recent listening.",
        );
        ui.add_space(10.0);
        ui.vertical_centered(|ui| {
            ui.add_enabled_ui(usable && !app.dj.busy, |ui| {
                if theme::pill_button(ui, &palette, "Start DJ", true).clicked() {
                    app.actions.push(Action::DjStart);
                }
            });
        });
    } else {
        let asks = app.dj_asks();
        let height = (ui.clip_rect().bottom() - ui.cursor().top()).max(160.0);
        let mut actions = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt("dj-chat")
            .max_height(height)
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                actions = chat(ui, &palette, app.backend.art(), &app.dj, &asks);
                // the composer floats over the end of the chat, which ends above it
                ui.add_space(COMPOSER_H + 40.0);
            });
        app.actions.extend(actions);
    }
    composer(app, ui, usable);
}

/// The chat's title and the way to another chat. It sits at the page's left edge.
fn title_row(app: &mut App, ui: &mut Ui) {
    let palette = app.palette;
    let title = match app.dj.lines.iter().find(|l| l.kind == LineKind::User) {
        Some(first) => {
            let mut title: String = first.text.chars().take(36).collect();
            if first.text.chars().count() > 36 {
                title.push('…');
            }
            title
        }
        None => "New chat".to_string(),
    };
    let galley = ui
        .painter()
        .layout_no_wrap(title, theme::bold(22.0), palette.text);
    let size = vec2(galley.size().x + 30.0, 34.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Chats"));
    let pos = pos2(rect.left(), rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(pos, galley.clone(), palette.text);
    let chevron = Rect::from_center_size(
        pos2(rect.left() + galley.size().x + 16.0, rect.center().y + 1.0),
        vec2(18.0, 18.0),
    );
    theme::paint_icon(
        ui,
        Icon::ChevronDown,
        chevron,
        18.0,
        if response.hovered() {
            palette.text
        } else {
            palette.secondary
        },
    );
    let busy = app.dj.busy;
    let current = app.dj.chat_id();
    let chats: Vec<(u64, String)> = app
        .dj
        .chats()
        .iter()
        .take(10)
        .map(|chat| (chat.id, chat.title.clone()))
        .collect();
    let has_lines = !app.dj.lines.is_empty();
    egui::Popup::menu(&response)
        .frame(widgets::menu_frame(&palette))
        .show(|ui| {
            ui.set_min_width(260.0);
            ui.set_max_width(360.0);
            ui.add_enabled_ui(!busy, |ui| {
                if widgets::menu_item(ui, &palette, Some(Icon::Plus), "New chat") {
                    app.actions.push(Action::DjNewChat);
                }
                if !chats.is_empty() {
                    widgets::menu_separator(ui, &palette);
                }
                for (id, title) in &chats {
                    let icon = (*id == current).then_some(Icon::Check);
                    if widgets::menu_item(ui, &palette, icon, title) {
                        app.actions.push(Action::DjOpenChat(*id));
                    }
                }
                if has_lines && chats.iter().any(|(id, _)| *id == current) {
                    widgets::menu_separator(ui, &palette);
                    if widgets::menu_item(ui, &palette, Some(Icon::Trash), "Delete this chat") {
                        app.actions.push(Action::DjDeleteChat(current));
                    }
                }
            });
        });
}

/// Today's time, or the day and time.
fn stamp(seconds: i64) -> Option<String> {
    let zone = jiff::tz::TimeZone::system();
    let at = jiff::Timestamp::from_second(seconds)
        .ok()?
        .to_zoned(zone.clone());
    let today = jiff::Timestamp::now().to_zoned(zone);
    let time = at.strftime("%H:%M").to_string();
    Some(if at.date() == today.date() {
        format!("Today {time}")
    } else {
        format!("{} {time}", at.strftime("%b %-d"))
    })
}

fn chat(
    ui: &mut Ui,
    palette: &Palette,
    art: &ArtLoader,
    dj: &crate::dj::Dj,
    asks: &[Ask],
) -> Vec<Action> {
    let mut cx = Cx {
        palette: *palette,
        art,
        actions: Vec::new(),
    };
    let width = ui.available_width();
    if let Some(text) = dj.chat_time().and_then(stamp) {
        ui.vertical_centered(|ui| {
            theme::text(ui, text, theme::regular(12.0), palette.dim);
        });
        ui.add_space(14.0);
    }
    let all = turns(&dj.lines);
    let last = all.len().saturating_sub(1);
    for turn in &all {
        let live = dj.busy && turn.at == last;
        if let Some(user) = turn.user {
            bubble(ui, &cx, &user.text, width);
            ui.add_space(18.0);
        }
        column(ui, COLUMN, |ui| {
            reply_side(ui, &mut cx, turn, live, dj, asks, turn.at == last);
        });
        ui.add_space(18.0);
    }
    if all.is_empty() {
        column(ui, COLUMN, |ui| {
            for ask in asks {
                ask_card(ui, &mut cx, ask);
                ui.add_space(8.0);
            }
        });
    }
    if let Some(hint) = &dj.hint {
        column(ui, COLUMN, |ui| {
            theme::text(ui, hint.clone(), theme::regular(12.5), palette.secondary);
        });
    }
    cx.actions
}

/// The listener's message, on the right.
fn bubble(ui: &mut Ui, cx: &Cx, text: &str, width: f32) {
    ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
        Frame::new()
            .fill(cx.palette.surface)
            .corner_radius(CornerRadius::same(18))
            .inner_margin(Margin::symmetric(16, 10))
            .show(ui, |ui| {
                ui.set_max_width((width * 0.7).clamp(240.0, 640.0));
                wrapped(ui, text, theme::regular(14.5), cx.palette.text);
            });
    });
}

/// Everything the DJ did and said for one message.
fn reply_side(
    ui: &mut Ui,
    cx: &mut Cx,
    turn: &Turn,
    live: bool,
    dj: &crate::dj::Dj,
    asks: &[Ask],
    is_last: bool,
) {
    let palette = cx.palette;
    let has_cards = |f: fn(&Card) -> bool| {
        turn.tools
            .iter()
            .any(|line| line.card.as_ref().is_some_and(f))
    };
    let swapped = has_cards(|c| matches!(c, Card::Removed { .. }))
        && has_cards(|c| matches!(c, Card::Queued { .. }));
    let thinking = live && turn.replies.is_empty() && turn.thought.is_some();

    if let Some(thought) = turn.thought {
        thought_block(ui, cx, turn.at, thought, thinking);
    }

    // what it read stays visible as a bar; the rest of what it did folds under one line
    let is_read = |line: &Line| matches!(line.card, Some(Card::Read { .. }));
    for line in turn.tools.iter().filter(|l| is_read(l)) {
        read_bar(ui, cx, line);
        ui.add_space(8.0);
    }
    let rest: Vec<&Line> = turn.tools.iter().copied().filter(|l| !is_read(l)).collect();
    if live {
        for line in &rest {
            tool_row(ui, cx, line);
        }
    } else if !rest.is_empty() {
        let id = Id::new(("dj-tools", turn.at));
        let secs = turn.user.map_or(0, |user| user.secs);
        let label = if swapped && secs > 0 {
            format!("Worked for {secs}s \u{b7} {} steps", rest.len())
        } else if rest.len() == 1 {
            "Used 1 tool".to_string()
        } else {
            format!("Used {} tools", rest.len())
        };
        let open: bool = ui.ctx().data(|data| data.get_temp(id)).unwrap_or(false);
        if fold_header(ui, &palette, &label, open) {
            ui.ctx().data_mut(|data| data.insert_temp(id, !open));
        }
        collapse(ui, id.with("body"), open, |ui| {
            for line in &rest {
                tool_row(ui, cx, line);
            }
        });
    }

    if live && turn.replies.is_empty() && (turn.thought.is_none() || !turn.tools.is_empty()) {
        let phase = match dj.phase {
            Some("connecting") => "Connecting",
            _ if turn.tools.is_empty() => "Thinking",
            _ => "Working",
        };
        ui.horizontal(|ui| {
            let (square, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
            ui.painter()
                .rect_filled(square, CornerRadius::same(11), palette.surface);
            theme::paint_icon(ui, Icon::Search, square, 16.0, palette.secondary);
            ui.add_space(6.0);
            theme::text(ui, phase, theme::regular(13.5), palette.secondary);
            theme::spinner(ui, 15.0, palette.dim);
        });
        ui.add_space(6.0);
    }

    // the results
    ui.add_space(4.0);
    if swapped {
        let out: Vec<&Card> = turn
            .tools
            .iter()
            .filter_map(|l| l.card.as_ref())
            .filter(|c| matches!(c, Card::Removed { .. }))
            .collect();
        let inn: Vec<&Card> = turn
            .tools
            .iter()
            .filter_map(|l| l.card.as_ref())
            .filter(|c| matches!(c, Card::Queued { .. }))
            .collect();
        swap_tray(ui, cx, &out, &inn);
        ui.add_space(8.0);
    } else {
        for card in turn.tools.iter().filter_map(|l| l.card.as_ref()) {
            match card {
                Card::Queued { songs, total } => {
                    let heading =
                        format!("Queued {total} song{}", if *total == 1 { "" } else { "s" });
                    song_tray(ui, cx, &heading, songs, *total);
                    ui.add_space(8.0);
                }
                Card::Removed { songs, total } => {
                    let heading = format!(
                        "Took {total} song{} out of the queue",
                        if *total == 1 { "" } else { "s" }
                    );
                    song_tray(ui, cx, &heading, songs, *total);
                    ui.add_space(8.0);
                }
                Card::Playlist {
                    name,
                    uri,
                    created,
                    songs,
                    total,
                } => {
                    playlist_card(ui, cx, name, uri, *created, songs, *total);
                    ui.add_space(8.0);
                }
                _ => {}
            }
        }
    }

    if is_last {
        for ask in asks {
            ask_card(ui, cx, ask);
            ui.add_space(8.0);
        }
    }

    for reply in &turn.replies {
        markdown(ui, &reply.text, palette.text);
        ui.add_space(6.0);
    }
    for error in &turn.errors {
        wrapped(ui, &error.text, theme::regular(14.0), palette.danger);
        ui.add_space(6.0);
    }

    if !live && !turn.replies.is_empty() {
        ui.horizontal(|ui| {
            let copy = theme::icon_button(
                ui,
                Icon::Copy,
                16.0,
                palette.dim,
                palette.text,
                "Copy the answer",
            );
            if copy.clicked() {
                let text: Vec<&str> = turn.replies.iter().map(|l| l.text.as_str()).collect();
                ui.ctx().copy_text(text.join("\n\n"));
            }
            if is_last
                && !dj.busy
                && theme::icon_button(
                    ui,
                    Icon::RotateCcw,
                    16.0,
                    palette.dim,
                    palette.text,
                    "Ask again",
                )
                .clicked()
            {
                cx.actions.push(Action::DjRetry);
            }
        });
    }
}

/// "Thought for 6s" and the reasoning under it. While the model is thinking it is open and the
/// word shimmers; afterwards a click opens it again.
fn thought_block(ui: &mut Ui, cx: &Cx, at: usize, line: &Line, thinking: bool) {
    let palette = cx.palette;
    let id = Id::new(("dj-thought", at));
    let stored: Option<bool> = ui.ctx().data(|data| data.get_temp(id));
    let open = stored.unwrap_or(thinking);
    if thinking {
        ui.horizontal(|ui| {
            ui.add_space(2.0);
            let (rect, _) = ui.allocate_exact_size(vec2(16.0, 22.0), Sense::hover());
            theme::paint_icon(ui, Icon::AudioLines, rect, 15.0, palette.dim);
            let t = ui.input(|input| input.time) as f32;
            let glow = (t * 3.0).sin() * 0.5 + 0.5;
            let color = palette.dim.lerp_to_gamma(palette.text, glow * 0.8);
            theme::text(ui, "Thinking", theme::regular(13.5), color);
        });
        ui.ctx().request_repaint();
    } else {
        let label = format!("Thought for {}s", line.secs.max(1));
        if fold_header(ui, &palette, &label, open) {
            ui.ctx().data_mut(|data| data.insert_temp(id, !open));
        }
    }
    collapse(ui, id.with("body"), open, |ui| {
        let width = ui.available_width() - 16.0;
        let galley = ui.painter().layout(
            line.text.trim().to_string(),
            egui::FontId::monospace(12.5),
            palette.dim,
            width,
        );
        let (rect, _) = ui.allocate_exact_size(
            vec2(ui.available_width(), galley.size().y + 10.0),
            Sense::hover(),
        );
        let x = rect.left() + 8.0;
        ui.painter().line_segment(
            [pos2(x, rect.top() + 4.0), pos2(x, rect.bottom() - 4.0)],
            Stroke::new(1.0, palette.outline),
        );
        ui.painter()
            .galley(pos2(x + 12.0, rect.top() + 5.0), galley, palette.dim);
    });
    ui.add_space(2.0);
}

/// A dim line with a chevron that opens what is under it. Answers whether it was clicked.
fn fold_header(ui: &mut Ui, palette: &Palette, label: &str, open: bool) -> bool {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), theme::regular(13.0), palette.dim);
    let size = vec2(galley.size().x + 26.0, 24.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let color = if response.hovered() {
        palette.secondary
    } else {
        palette.dim
    };
    ui.painter().galley(
        pos2(rect.left() + 2.0, rect.center().y - galley.size().y / 2.0),
        galley.clone(),
        color,
    );
    let chevron = Rect::from_center_size(
        pos2(rect.left() + galley.size().x + 14.0, rect.center().y),
        vec2(14.0, 14.0),
    );
    theme::paint_icon(
        ui,
        if open {
            Icon::ChevronDown
        } else {
            Icon::ChevronRight
        },
        chevron,
        14.0,
        color,
    );
    response.clicked()
}

fn tool_icon(line: &Line) -> Icon {
    match &line.card {
        Some(Card::Found { .. }) => Icon::Search,
        Some(Card::Artists { .. }) => Icon::Users,
        Some(Card::Read { .. }) => Icon::ListMusic,
        Some(Card::Queued { .. }) => Icon::ListEnd,
        Some(Card::Removed { .. }) => Icon::Trash,
        Some(Card::Playlist { .. }) => Icon::ListPlus,
        None => Icon::AudioLines,
    }
}

/// A round icon, what the DJ did, and a light result: artists' faces, a few covers.
fn tool_row(ui: &mut Ui, cx: &Cx, line: &Line) {
    let palette = cx.palette;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::hover());
    let disc = Rect::from_min_size(rect.left_top() + vec2(0.0, 4.0), vec2(36.0, 36.0));
    ui.painter()
        .rect_filled(disc, CornerRadius::same(11), palette.surface);
    theme::paint_icon(ui, tool_icon(line), disc, 16.0, palette.secondary);
    let galley =
        ui.painter()
            .layout_no_wrap(line.text.clone(), theme::regular(13.5), palette.secondary);
    let text_at = pos2(disc.right() + 16.0, rect.center().y - galley.size().y / 2.0);
    let end = text_at.x + galley.size().x;
    ui.painter().galley(text_at, galley, palette.secondary);
    let mut x = end + 14.0;
    match &line.card {
        Some(Card::Artists { photos, .. }) => {
            for photo in photos.iter().take(6) {
                let face = Rect::from_min_size(pos2(x, rect.top() + 5.0), vec2(34.0, 34.0));
                widgets::paint_cover(
                    ui,
                    &palette,
                    photo.as_deref(),
                    face,
                    17.0,
                    Icon::User,
                    Some(cx.art),
                );
                x += 46.0;
            }
        }
        Some(Card::Found { covers, .. }) => {
            for (_, cover) in covers.iter().take(6) {
                let thumb = Rect::from_min_size(pos2(x, rect.top() + 5.0), vec2(34.0, 34.0));
                widgets::paint_cover(
                    ui,
                    &palette,
                    cover.as_deref(),
                    thumb,
                    5.0,
                    Icon::Music,
                    Some(cx.art),
                );
                x += 42.0;
            }
        }
        _ => {}
    }
}

/// What the DJ read, as a bar.
fn read_bar(ui: &mut Ui, cx: &Cx, line: &Line) {
    let palette = cx.palette;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::hover());
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(10),
        Stroke::new(1.0, palette.outline),
        egui::StrokeKind::Inside,
    );
    let icon = Rect::from_center_size(pos2(rect.left() + 24.0, rect.center().y), vec2(18.0, 18.0));
    theme::paint_icon(ui, Icon::ListMusic, icon, 16.0, palette.secondary);
    let galley = ui
        .painter()
        .layout_no_wrap(line.text.clone(), theme::regular(13.5), palette.text);
    let at = pos2(rect.left() + 46.0, rect.center().y - galley.size().y / 2.0);
    let end = at.x + galley.size().x;
    ui.painter().galley(at, galley, palette.text);
    let chevron = Rect::from_center_size(pos2(end + 14.0, rect.center().y), vec2(14.0, 14.0));
    theme::paint_icon(ui, Icon::ChevronRight, chevron, 14.0, palette.dim);
}

/// A tray of songs under a heading, five rows and "+N more".
fn song_tray(ui: &mut Ui, cx: &Cx, heading: &str, songs: &[CardSong], total: usize) {
    let palette = cx.palette;
    let outer = Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                theme::text(ui, heading, theme::semibold(14.0), palette.text);
            });
            ui.add_space(10.0);
            let behind = ui.painter().add(Shape::Noop);
            let rows = ui.scope(|ui| {
                ui.set_width(ui.available_width());
                song_rows(ui, cx, songs, total);
            });
            ui.painter()
                .set(behind, energy_line(cx, rows.response.rect));
            ui.add_space(8.0);
        });
    let _ = outer;
}

/// A faint green line rising behind the rows: the set builds in energy.
fn energy_line(cx: &Cx, rect: Rect) -> Shape {
    if rect.height() < 20.0 {
        return Shape::Noop;
    }
    let color = cx.palette.accent.gamma_multiply(0.22);
    let points: Vec<egui::Pos2> = (0..=16)
        .map(|i| {
            let t = i as f32 / 16.0;
            pos2(
                rect.left() + 14.0 + (rect.width() - 28.0) * t,
                rect.bottom() - 8.0 - (rect.height() - 16.0) * t * t.sqrt(),
            )
        })
        .collect();
    Shape::line(points, Stroke::new(1.0, color))
}

fn song_rows(ui: &mut Ui, cx: &Cx, songs: &[CardSong], total: usize) {
    let palette = cx.palette;
    for (i, song) in songs.iter().take(CARD_SONGS).enumerate() {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
        let inner = rect.shrink2(vec2(14.0, 0.0));
        if i > 0 {
            ui.painter().line_segment(
                [inner.left_top(), inner.right_top()],
                Stroke::new(1.0, palette.outline),
            );
        }
        let cover = Rect::from_min_size(inner.left_top() + vec2(0.0, 5.0), vec2(44.0, 44.0));
        widgets::paint_cover(
            ui,
            &palette,
            song.cover.as_deref(),
            cover,
            6.0,
            Icon::Music,
            Some(cx.art),
        );
        let clip = ui.painter().with_clip_rect(Rect::from_min_max(
            pos2(cover.right() + 14.0, rect.top()),
            pos2(inner.right() - 54.0, rect.bottom()),
        ));
        clip.text(
            pos2(cover.right() + 14.0, rect.top() + 9.0),
            Align2::LEFT_TOP,
            &song.name,
            theme::regular(14.5),
            palette.text,
        );
        clip.text(
            pos2(cover.right() + 14.0, rect.top() + 28.0),
            Align2::LEFT_TOP,
            &song.artists,
            theme::regular(13.0),
            palette.secondary,
        );
        ui.painter().text(
            pos2(inner.right() - 4.0, rect.center().y),
            Align2::RIGHT_CENTER,
            clock(song.duration_ms),
            theme::regular(13.0),
            palette.dim,
        );
    }
    if total > songs.len().min(CARD_SONGS) {
        let more = total - songs.len().min(CARD_SONGS);
        ui.horizontal(|ui| {
            ui.add_space(18.0);
            theme::text(
                ui,
                format!("+{more} more"),
                theme::regular(13.0),
                palette.dim,
            );
        });
    }
}

/// What a swap did: the songs that went out, struck through and faded, and the ones that came in.
fn swap_tray(ui: &mut Ui, cx: &Cx, out: &[&Card], inn: &[&Card]) {
    let palette = cx.palette;
    let pick = |cards: &[&Card]| -> (Vec<CardSong>, usize) {
        let mut songs = Vec::new();
        let mut total = 0;
        for card in cards {
            if let Card::Removed { songs: s, total: t } | Card::Queued { songs: s, total: t } = card
            {
                songs.extend(s.iter().cloned());
                total += t;
            }
        }
        (songs, total)
    };
    let widest = [out, inn]
        .iter()
        .map(|cards| pick(cards).0.len())
        .max()
        .unwrap_or(0)
        .min(6);
    let width = (96.0 + widest as f32 * 78.0).min(ui.available_width());
    Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(width - 32.0);
            for (label, cards, gone) in [("Out", out, true), ("In", inn, false)] {
                let (songs, total) = pick(cards);
                let (rect, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 82.0), Sense::hover());
                ui.painter().text(
                    pos2(rect.left(), rect.center().y),
                    Align2::LEFT_CENTER,
                    label,
                    theme::regular(13.5),
                    palette.dim,
                );
                let mut x = rect.left() + 56.0;
                for song in songs.iter().take(6) {
                    let cover = Rect::from_min_size(pos2(x, rect.top() + 9.0), vec2(64.0, 64.0));
                    if gone {
                        // the picture, dimmed, with a hairline through it
                        widgets::paint_cover(
                            ui,
                            &palette,
                            song.cover.as_deref(),
                            cover,
                            4.0,
                            Icon::Music,
                            Some(cx.art),
                        );
                        ui.painter().rect_filled(
                            cover,
                            CornerRadius::same(4),
                            palette.surface.gamma_multiply(0.6),
                        );
                        ui.painter().line_segment(
                            [cover.left_bottom(), cover.right_top()],
                            Stroke::new(1.2, palette.secondary),
                        );
                    } else {
                        widgets::paint_cover(
                            ui,
                            &palette,
                            song.cover.as_deref(),
                            cover,
                            4.0,
                            Icon::Music,
                            Some(cx.art),
                        );
                        ui.painter().circle_filled(
                            cover.left_top() + vec2(5.0, -4.0),
                            3.0,
                            palette.accent,
                        );
                    }
                    x += 78.0;
                }
                if total > songs.len().min(6) {
                    ui.painter().text(
                        pos2(x, rect.center().y),
                        Align2::LEFT_CENTER,
                        format!("+{}", total - songs.len().min(6)),
                        theme::regular(13.0),
                        palette.dim,
                    );
                }
            }
        });
}

/// A playlist the DJ made or added to: a bar with its name and a link, then its songs.
fn playlist_card(
    ui: &mut Ui,
    cx: &mut Cx,
    name: &str,
    uri: &str,
    created: bool,
    songs: &[CardSong],
    total: usize,
) {
    let palette = cx.palette;
    let label = if created {
        format!("Created {name}")
    } else {
        format!(
            "Added {total} song{} to {name}",
            if total == 1 { "" } else { "s" }
        )
    };
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 42.0), Sense::hover());
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(10),
        Stroke::new(1.0, palette.outline),
        egui::StrokeKind::Inside,
    );
    let check = Rect::from_center_size(pos2(rect.left() + 24.0, rect.center().y), vec2(22.0, 22.0));
    ui.painter()
        .circle_filled(check.center(), 11.0, palette.accent);
    theme::paint_icon(ui, Icon::Check, check, 13.0, palette.on_accent);
    let clip = ui.painter().with_clip_rect(Rect::from_min_max(
        pos2(rect.left() + 44.0, rect.top()),
        pos2(rect.right() - 70.0, rect.bottom()),
    ));
    clip.text(
        pos2(rect.left() + 46.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        theme::regular(14.0),
        palette.text,
    );
    let open = Rect::from_min_max(
        pos2(rect.right() - 66.0, rect.top()),
        pos2(rect.right() - 6.0, rect.bottom()),
    );
    let response = ui.interact(open, ui.id().with(("dj-open", uri)), Sense::click());
    let color = if response.hovered() {
        palette.text
    } else {
        palette.dim
    };
    ui.painter().text(
        open.center(),
        Align2::CENTER_CENTER,
        "Open",
        theme::regular(13.0),
        color,
    );
    if response.clicked()
        && let Some(id) = crate::util::uri_id(uri)
    {
        cx.actions
            .push(Action::Open(Page::Playlist(id.to_string())));
    }
    ui.add_space(6.0);
    song_rows(ui, cx, songs, total);
}

/// A change the DJ asked for: what it is, and the listener's yes or no.
fn ask_card(ui: &mut Ui, cx: &mut Cx, ask: &Ask) {
    let palette = cx.palette;
    Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.2, palette.accent))
        .corner_radius(CornerRadius::same(16))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                if !ask.covers.is_empty() {
                    let (rect, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
                    for (i, cover) in ask.covers.iter().take(4).enumerate() {
                        let cell = Rect::from_min_size(
                            rect.left_top() + vec2((i % 2) as f32 * 28.0, (i / 2) as f32 * 28.0),
                            vec2(28.0, 28.0),
                        );
                        widgets::paint_cover(
                            ui,
                            &palette,
                            Some(cover.as_str()),
                            cell,
                            0.0,
                            Icon::Music,
                            Some(cx.art),
                        );
                    }
                    ui.add_space(8.0);
                }
                ui.vertical(|ui| {
                    ui.set_max_width((ui.available_width() - 190.0).max(120.0));
                    wrapped(ui, &ask.title, theme::semibold(14.5), palette.text);
                    if !ask.detail.is_empty() {
                        wrapped(ui, &ask.detail, theme::regular(13.0), palette.secondary);
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::pill_button(ui, &palette, ask.confirm, true).clicked() {
                        cx.actions.push(Action::DjApprove(ask.id));
                    }
                    if theme::pill_button(ui, &palette, "Cancel", false).clicked() {
                        cx.actions.push(Action::DjDeny(ask.id));
                    }
                });
            });
            // the words of the change itself, so the picture and the short lines are never all
            // the listener has to go on
            if !ask.full.is_empty() {
                ui.add_space(6.0);
                wrapped(ui, &ask.full, theme::regular(12.5), palette.secondary);
            }
        });
    // the unanswered ones time out, so the page keeps ticking
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_secs(1));
}

/// The bar floating at the bottom: new chat, the message, model, effort, send or stop.
fn composer(app: &mut App, ui: &mut Ui, usable: bool) {
    let palette = app.palette;
    let clip = ui.clip_rect();
    let width = (clip.width() - 32.0).clamp(280.0, COMPOSER_W);
    let pos = pos2(
        clip.center().x - width / 2.0,
        clip.bottom() - COMPOSER_H - 18.0,
    );
    let busy = app.dj.busy;
    let input_id = Id::new("dj-input");
    let mut changed = false;
    egui::Area::new(Id::new("dj-composer"))
        .order(egui::Order::Middle)
        .fixed_pos(pos)
        .show(ui.ctx(), |ui| {
            Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(24))
                .inner_margin(Margin::symmetric(8, 7))
                .shadow(egui::epaint::Shadow {
                    offset: [0, 8],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(if palette.dark { 110 } else { 40 }),
                })
                .show(ui, |ui| {
                    ui.set_width(width - 18.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        ui.add_enabled_ui(!busy, |ui| {
                            if theme::circle_button(
                                ui,
                                Icon::Plus,
                                38.0,
                                palette.surface_hover,
                                palette.surface_active,
                                palette.secondary,
                                "New chat",
                            )
                            .clicked()
                            {
                                app.actions.push(Action::DjNewChat);
                            }
                        });
                        let right = 112.0 + 38.0 + 42.0 + 24.0;
                        let edit_width = (ui.available_width() - right).max(80.0);
                        let response = ui.add_sized(
                            vec2(edit_width, 38.0),
                            egui::TextEdit::singleline(&mut app.dj.input)
                                .id(input_id)
                                .hint_text(
                                    egui::RichText::new("Tell the DJ what you want")
                                        .color(palette.dim),
                                )
                                .font(theme::regular(14.5))
                                .text_color(palette.text)
                                .frame(egui::Frame::NONE)
                                .margin(Margin::symmetric(6, 0))
                                .vertical_align(Align::Center),
                        );
                        let entered =
                            response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        changed |= model_pill(ui, &mut app.settings.dj_model, &palette);
                        changed |= effort_button(ui, &mut app.settings.dj_effort, &palette);
                        let can_send = usable && !busy && !app.dj.input.trim().is_empty();
                        if send_button(ui, &palette, busy, can_send) || (entered && can_send) {
                            if busy {
                                app.actions.push(Action::DjCancel);
                            } else if can_send {
                                app.actions.push(Action::DjSend);
                                ui.memory_mut(|memory| memory.request_focus(input_id));
                            }
                        }
                    });
                });
        });
    if changed {
        app.actions.push(Action::SettingsChanged);
    }
}

/// The provider and model: "Claude" with its mark, a menu of models.
fn model_pill(ui: &mut Ui, model: &mut String, palette: &Palette) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(112.0, 38.0), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Model"));
    let fill = if response.hovered() {
        palette.surface_active
    } else {
        palette.surface_hover
    };
    ui.painter().rect_filled(rect, CornerRadius::same(19), fill);
    let mark = Rect::from_center_size(pos2(rect.left() + 20.0, rect.center().y), vec2(18.0, 18.0));
    theme::paint_icon(ui, Icon::Asterisk, mark, 17.0, CLAUDE);
    ui.painter().text(
        pos2(rect.left() + 36.0, rect.center().y),
        Align2::LEFT_CENTER,
        "Claude",
        theme::medium(13.5),
        palette.text,
    );
    let chevron =
        Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(14.0, 14.0));
    theme::paint_icon(ui, Icon::ChevronDown, chevron, 14.0, palette.secondary);
    let mut changed = false;
    let current = model.clone();
    egui::Popup::menu(&response)
        .frame(widgets::menu_frame(palette))
        .show(|ui| {
            ui.set_min_width(180.0);
            for option in crate::dj::models() {
                let icon = (current == option.id).then_some(Icon::Check);
                if widgets::menu_item(ui, palette, icon, &option.name) && current != option.id {
                    *model = option.id;
                    changed = true;
                }
            }
        });
    changed
}

/// The brain: how hard the model thinks, a menu of efforts.
fn effort_button(ui: &mut Ui, effort: &mut String, palette: &Palette) -> bool {
    let tip = format!("Thinking effort: {effort}");
    let response = theme::circle_button(
        ui,
        Icon::Brain,
        38.0,
        palette.surface_hover,
        palette.surface_active,
        palette.secondary,
        &tip,
    );
    let mut changed = false;
    let current = effort.clone();
    egui::Popup::menu(&response)
        .frame(widgets::menu_frame(palette))
        .show(|ui| {
            ui.set_min_width(150.0);
            for option in crate::dj::EFFORTS {
                let mut label = option.to_string();
                if let Some(first) = label.get_mut(..1) {
                    first.make_ascii_uppercase();
                }
                let icon = (current == option).then_some(Icon::Check);
                if widgets::menu_item(ui, palette, icon, &label) && current != option {
                    *effort = option.to_string();
                    changed = true;
                }
            }
        });
    changed
}

/// The white disc: an arrow to send, a square to stop.
fn send_button(ui: &mut Ui, palette: &Palette, busy: bool, can_send: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(42.0, 42.0), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            true,
            if busy { "Stop" } else { "Send" },
        )
    });
    let live = busy || can_send;
    let fill = if live {
        if response.hovered() {
            palette.text.gamma_multiply(0.88)
        } else {
            palette.text
        }
    } else {
        palette.surface_active
    };
    ui.painter().circle_filled(rect.center(), 21.0, fill);
    if busy {
        ui.painter().rect_filled(
            Rect::from_center_size(rect.center(), vec2(14.0, 14.0)),
            CornerRadius::same(3),
            palette.window,
        );
    } else {
        theme::paint_icon(
            ui,
            Icon::ArrowUp,
            rect,
            20.0,
            if live { palette.window } else { palette.dim },
        );
    }
    theme::focus_ring(ui, &response);
    response.clicked()
}

/// The DJ's section of the settings page. Returns whether a setting changed.
pub fn settings(app: &mut App, ui: &mut egui::Ui) -> bool {
    let palette = app.palette;
    let mut changed = false;
    widgets::setting_row(
        ui,
        &palette,
        "AI DJ",
        "Show the DJ in the sidebar. It runs the Claude Code CLI you are signed in to; Spotifast never sees your Claude login.",
        |ui| {
            if widgets::switch(ui, &palette, "AI DJ", &mut app.settings.dj_enabled).changed() {
                changed = true;
            }
        },
    );
    widgets::setting_row(
        ui,
        &palette,
        "Ask before changing my library",
        "The DJ asks you to approve each like, playlist change or scheduled task before it happens.",
        |ui| {
            if widgets::switch(
                ui,
                &palette,
                "Ask before changing my library",
                &mut app.settings.dj_confirm_writes,
            )
            .changed()
            {
                changed = true;
            }
        },
    );
    widgets::setting_row(
        ui,
        &palette,
        "Let the DJ search the web",
        "Lets the DJ use Claude Code's own web search to look up music facts. The search words it chooses go to Claude's search; it cannot open pages.",
        |ui| {
            if widgets::switch(
                ui,
                &palette,
                "Let the DJ search the web",
                &mut app.settings.dj_web,
            )
            .changed()
            {
                changed = true;
            }
        },
    );
    widgets::setting_row(
        ui,
        &palette,
        "Autopilot",
        "While Spotifast is open, the DJ tops up the queue when it runs low and reacts to skips. It uses your Claude plan.",
        |ui| {
            if widgets::switch(ui, &palette, "Autopilot", &mut app.settings.dj_autopilot).changed()
            {
                changed = true;
            }
        },
    );
    widgets::setting_row(
        ui,
        &palette,
        "Background model",
        "The model for the DJ's own turns (autopilot, scheduled tasks). A smaller one costs less of your plan.",
        |ui| {
            let current = app.settings.dj_background_model.clone();
            let shown = if current.is_empty() {
                "Same as the DJ model".to_string()
            } else {
                crate::dj::models()
                    .into_iter()
                    .find(|m| m.id == current)
                    .map_or_else(|| current.clone(), |m| m.name)
            };
            egui::ComboBox::from_id_salt("dj_background_model")
                .selected_text(shown)
                .width(200.0_f32.min(ui.available_width()))
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(current.is_empty(), "Same as the DJ model")
                        .clicked()
                        && !current.is_empty()
                    {
                        app.settings.dj_background_model = String::new();
                        changed = true;
                    }
                    for model in crate::dj::models() {
                        if ui
                            .selectable_label(current == model.id, &model.name)
                            .clicked()
                            && current != model.id
                        {
                            app.settings.dj_background_model = model.id;
                            changed = true;
                        }
                    }
                });
        },
    );
    widgets::setting_row(
        ui,
        &palette,
        "DJ model",
        "Which Claude model the DJ asks for.",
        |ui| {
            let current = app.settings.dj_model.clone();
            let shown = crate::dj::models()
                .into_iter()
                .find(|m| m.id == current)
                .map_or_else(|| current.clone(), |m| m.name);
            egui::ComboBox::from_id_salt("dj_model")
                .selected_text(shown)
                .width(200.0_f32.min(ui.available_width()))
                .show_ui(ui, |ui| {
                    for model in crate::dj::models() {
                        if ui
                            .selectable_label(current == model.id, &model.name)
                            .clicked()
                            && current != model.id
                        {
                            app.settings.dj_model = model.id;
                            changed = true;
                        }
                    }
                });
        },
    );
    widgets::setting_row(
        ui,
        &palette,
        "DJ effort",
        "How hard the model thinks before it answers. Lower is faster and uses less of your plan.",
        |ui| {
            let current = app.settings.dj_effort.clone();
            egui::ComboBox::from_id_salt("dj_effort")
                .selected_text(current.clone())
                .width(200.0_f32.min(ui.available_width()))
                .show_ui(ui, |ui| {
                    for effort in crate::dj::EFFORTS {
                        if ui.selectable_label(current == effort, effort).clicked()
                            && current != effort
                        {
                            app.settings.dj_effort = effort.to_string();
                            changed = true;
                        }
                    }
                });
        },
    );
    changed
}
