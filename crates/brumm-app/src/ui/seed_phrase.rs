//! One-time modal that shows the freshly generated seed phrase for backup.

use crate::{state::AppState, ui::theme};

const WORDS_PER_ROW: usize = 4;

pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(phrase) = &state.seed_phrase_to_show else {
        return;
    };
    let mut acknowledged = false;
    egui::Modal::new(egui::Id::unique("seed_phrase")).show(ui.ctx(), |ui| {
        ui.heading("Write down your seed phrase");
        ui.label("These 24 words are your identity. Anyone who has them can be you, and without them a lost device means a lost identity.");
        ui.add_space(8.0);
        egui::Grid::new("seed_words").num_columns(WORDS_PER_ROW).spacing([16.0, 6.0]).show(ui, |ui| {
            for (index, word) in phrase.split_whitespace().enumerate() {
                ui.monospace(format!("{:>2}. {word}", index + 1));
                if (index + 1) % WORDS_PER_ROW == 0 {
                    ui.end_row();
                }
            }
        });
        ui.add_space(8.0);
        acknowledged = ui.add(theme::primary_button("I have written it down")).clicked();
    });
    if acknowledged {
        state.seed_phrase_to_show = None;
    }
}
