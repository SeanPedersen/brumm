//! Top-level eframe app: pumps core events into [`AppState`] and routes to the screen
//! for the current phase. Screens return [`Command`]s instead of talking to the core.

use std::path::PathBuf;

use brumm_core::api::Command;

use crate::{
    bridge::CoreBridge,
    state::{AppState, Phase},
    ui,
};

pub struct BrummApp {
    core: CoreBridge,
    state: AppState,
    view: ui::ViewState,
}

impl BrummApp {
    pub fn new(cc: &eframe::CreationContext<'_>, data_dir: PathBuf) -> Self {
        ui::theme::apply(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        Self {
            core: CoreBridge::start(data_dir, cc.egui_ctx.clone()),
            state: AppState::default(),
            view: ui::ViewState::default(),
        }
    }
}

impl eframe::App for BrummApp {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        for event in self.core.drain() {
            if matches!(event, brumm_core::api::Event::Error(_)) {
                self.view.onboarding.allow_retry();
            }
            self.state.apply(event);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut commands: Vec<Command> = Vec::new();
        ui::errors::show(ui, &mut self.state);
        match self.state.phase() {
            Phase::Starting => ui::starting::show(ui),
            Phase::Onboarding => {
                ui::onboarding::show(ui, &mut self.view.onboarding, self.state.link_ticket.as_deref(), &mut commands)
            }
            Phase::Main => ui::main_screen::show(ui, &mut self.state, &mut self.view, &mut commands),
            Phase::Revoked => ui::revoked::show(ui),
        }
        for contact in self.state.stale_conversations.drain(..) {
            commands.push(Command::LoadConversation { contact });
        }
        for command in commands {
            self.core.send(command);
        }
    }
}
