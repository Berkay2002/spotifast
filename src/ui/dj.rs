//! The DJ page: a chat with Claude Code that picks and plays music, and the DJ's settings.
//!
//! The strings here are plain English for now (see `docs/_reference/dj.md`).

use egui::{Align, Layout};

use crate::app::App;
use crate::dj::LineKind;
use crate::model::Action;
use crate::theme::{self, Icon};

use super::widgets;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    ui.add_space(8.0);
    theme::text(ui, "DJ", theme::bold(28.0), palette.text);
    ui.add_space(2.0);
    theme::text(
        ui,
        "Claude Code picks and plays music for you, using your Claude subscription.",
        theme::regular(13.5),
        palette.secondary,
    );
    ui.add_space(14.0);

    let blocker = app.dj.status.as_ref().and_then(crate::dj::session::blocker);
    if let Some(message) = &blocker {
        ui.horizontal_wrapped(|ui| {
            theme::text(ui, message.clone(), theme::regular(14.0), palette.warning);
        });
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
    ui.horizontal(|ui| {
        ui.add_enabled_ui(usable && !app.dj.busy, |ui| {
            if theme::pill_button(ui, &palette, "Start DJ", true).clicked() {
                app.actions.push(Action::DjStart);
            }
        });
        if app.dj.busy {
            if theme::pill_button(ui, &palette, "Stop", false).clicked() {
                app.actions.push(Action::DjCancel);
            }
            let phase = match app.dj.phase {
                Some("connecting") => "Connecting…",
                _ => "Thinking…",
            };
            theme::text(ui, phase, theme::regular(13.0), palette.secondary);
        }
    });
    ui.add_space(14.0);

    if app.dj.lines.is_empty() {
        widgets::empty_state(
            ui,
            &palette,
            Icon::Sparkles,
            "Nothing playing yet",
            "Press Start DJ, or ask for a mood, an artist or a decade.",
        );
    }
    for line in &app.dj.lines {
        match line.kind {
            LineKind::User => {
                ui.with_layout(Layout::top_down(Align::Max), |ui| {
                    theme::text(ui, line.text.clone(), theme::semibold(14.5), palette.accent);
                });
            }
            LineKind::Assistant => {
                ui.horizontal_wrapped(|ui| {
                    theme::text(ui, line.text.clone(), theme::regular(14.5), palette.text);
                });
            }
            LineKind::Tool => {
                ui.horizontal(|ui| {
                    theme::icon(ui, Icon::Sparkles, 14.0, palette.dim);
                    theme::text(ui, line.text.clone(), theme::regular(12.5), palette.dim);
                });
            }
            LineKind::Error => {
                ui.horizontal_wrapped(|ui| {
                    theme::text(ui, line.text.clone(), theme::regular(14.0), palette.danger);
                });
            }
        }
        ui.add_space(6.0);
    }
    if let Some(hint) = &app.dj.hint {
        theme::text(ui, hint.clone(), theme::regular(12.5), palette.secondary);
        ui.add_space(6.0);
    }

    ui.add_space(8.0);
    let id = egui::Id::new("dj-input");
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 84.0).max(120.0);
        let response = widgets::search_field(
            ui,
            &palette,
            app.locale,
            id,
            &mut app.dj.input,
            "Tell the DJ what to play",
            width,
        );
        let entered = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let send = theme::pill_button(ui, &palette, "Send", true).clicked() || entered;
        if send && usable && !app.dj.busy && !app.dj.input.trim().is_empty() {
            app.actions.push(Action::DjSend);
            ui.memory_mut(|memory| memory.request_focus(id));
        }
    });
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
