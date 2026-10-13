//! The DJ page: a chat with Claude Code that picks and plays music, and the DJ's settings.
//!
//! The strings here are plain English for now (see `docs/_reference/dj.md`).

use egui::{Align, Layout};

use crate::app::App;
use crate::dj::LineKind;
use crate::model::Action;
use crate::theme::{self, Icon};

use super::widgets;

/// Text that wraps at the page's width (`theme::text` stays on one line).
fn wrapped(ui: &mut egui::Ui, text: &str, font: egui::FontId, color: egui::Color32) {
    ui.add(egui::Label::new(egui::RichText::new(text).font(font).color(color)).wrap());
}

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
                    wrapped(ui, &line.text, theme::semibold(14.5), palette.accent);
                });
            }
            LineKind::Assistant => {
                wrapped(ui, &line.text, theme::regular(14.5), palette.text);
            }
            LineKind::Tool => {
                ui.horizontal(|ui| {
                    theme::icon(ui, Icon::Sparkles, 14.0, palette.dim);
                    theme::text(ui, line.text.clone(), theme::regular(12.5), palette.dim);
                });
            }
            LineKind::Error => {
                wrapped(ui, &line.text, theme::regular(14.0), palette.danger);
            }
        }
        ui.add_space(6.0);
    }
    if let Some(hint) = &app.dj.hint {
        theme::text(ui, hint.clone(), theme::regular(12.5), palette.secondary);
        ui.add_space(6.0);
    }
    // changes the DJ asked for and the listener has not answered
    let waiting: Vec<(u64, String)> = app
        .dj
        .approvals
        .iter()
        .map(|a| (a.id, a.summary.clone()))
        .collect();
    if !waiting.is_empty() {
        // the unanswered ones time out, so the page keeps ticking
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(1));
    }
    for (id, summary) in waiting {
        ui.add_space(4.0);
        theme::text(
            ui,
            "The DJ wants to:",
            theme::semibold(13.0),
            palette.secondary,
        );
        wrapped(ui, &summary, theme::regular(14.5), palette.text);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if theme::pill_button(ui, &palette, "Approve", true).clicked() {
                app.actions.push(Action::DjApprove(id));
            }
            if theme::pill_button(ui, &palette, "Decline", false).clicked() {
                app.actions.push(Action::DjDeny(id));
            }
        });
        ui.add_space(8.0);
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
