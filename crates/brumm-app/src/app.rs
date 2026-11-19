//! Top-level eframe app: pumps core events into [`AppState`] and routes to the screen
//! for the current phase. Screens return [`Command`]s instead of talking to the core.

use std::path::PathBuf;

use brumm_core::api::Command;

use crate::{
    bridge::CoreBridge,
    preview::PreviewLoader,
    state::{AppState, Phase},
    ui,
    window_size::WindowSize,
};

pub struct BrummApp {
    core: CoreBridge,
    state: AppState,
    view: ui::ViewState,
    window_size: WindowSize,
    previews: PreviewLoader,
}

impl BrummApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        data_dir: PathBuf,
        window_size: WindowSize,
    ) -> Self {
        ui::theme::apply(&cc.egui_ctx);
        // After the text fonts, before any sans-serif fallback that also has emoji outlines.
        cc.egui_ctx
            .add_font_provider(std::sync::Arc::new(ui::emoji_font::ColorEmojiFont::new()));
        egui_extras::install_image_loaders(&cc.egui_ctx);
        Self {
            core: CoreBridge::start(data_dir, cc.egui_ctx.clone()),
            state: AppState::default(),
            view: ui::ViewState::default(),
            window_size,
            previews: PreviewLoader::start(),
        }
    }
}

impl eframe::App for BrummApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Points, including on Wayland where the native inner rect is unavailable.
        let size = ctx.input(|input| input.viewport_rect().size());
        self.window_size.observe(size);
        self.window_size.flush_if_due();
        for event in self.core.drain() {
            if matches!(event, brumm_core::api::Event::Error(_)) {
                self.view.onboarding.allow_retry();
            }
            self.state.apply(event, ctx);
        }
        while let Some(job) = self.state.take_pending_preview() {
            self.previews
                .submit(job.contact, job.id, job.bytes, job.max_edge, ctx.clone());
        }
        for ready in self.previews.poll() {
            if let (Some(bytes), Some((width, height))) = (ready.persist, ready.source_size) {
                self.core.send(brumm_core::api::Command::StoreImagePreview {
                    contact: ready.contact,
                    id: ready.id,
                    width,
                    height,
                    max_edge: ready.max_edge,
                    bytes,
                });
            }
            self.state
                .install_attachment((ready.contact, ready.id), ready.loaded, ctx);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut commands: Vec<Command> = Vec::new();
        ui::errors::show(ui, &mut self.state);
        match self.state.phase() {
            Phase::Starting => ui::starting::show(ui),
            Phase::Onboarding => ui::onboarding::show(
                ui,
                &mut self.view.onboarding,
                self.state.link_ticket.as_deref(),
                &mut commands,
            ),
            Phase::Main => {
                ui::main_screen::show(ui, &mut self.state, &mut self.view, &mut commands)
            }
            Phase::Revoked => ui::revoked::show(ui),
        }
        for contact in self.state.stale_conversations.drain(..) {
            commands.push(Command::LoadConversation { contact });
        }
        if let Some(contact) = self.state.remember_open.take() {
            commands.push(Command::RememberOpenChat { contact });
        }
        for command in commands {
            self.core.send(command);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.window_size.flush();
    }
}
