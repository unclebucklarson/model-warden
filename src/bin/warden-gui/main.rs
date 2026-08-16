//! Traditional desktop shell over the headless core: menu bar, status bar,
//! activity log, tabbed central panel. Shell pattern follows llamacppCodeConf's
//! ui.rs (worker thread + mpsc `Msg` channel arrives with M1's scanner).

use eframe::egui;

fn main() -> eframe::Result {
    env_logger::init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1150.0, 720.0])
            .with_title("modelwarden"),
        ..Default::default()
    };
    eframe::run_native(
        "modelwarden",
        options,
        Box::new(|_cc| Ok(Box::new(App::default()))),
    )
}

#[derive(Default, PartialEq)]
enum Pane {
    #[default]
    Inventory,
}

#[derive(Default)]
struct App {
    pane: Pane,
    activity: Vec<String>,
    show_about: bool,
}

impl App {
    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("File", |ui| {
            if ui.button("Quit").clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
        ui.menu_button("Help", |ui| {
            if ui.button("About").clicked() {
                self.show_about = true;
                ui.close();
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| self.menu_bar(ui));
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.label("M0 scaffold — inventory arrives with M1 (see ROADMAP.md)");
        });
        egui::Panel::bottom("activity")
            .resizable(true)
            .default_size(80.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.activity {
                            ui.monospace(line);
                        }
                    });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.pane, Pane::Inventory, "📦 Inventory");
            });
            ui.separator();
            match self.pane {
                Pane::Inventory => {
                    ui.label("No inventory yet — the scanner lands in M1.");
                }
            }
        });

        if self.show_about {
            egui::Window::new("About")
                .collapsible(false)
                .resizable(false)
                .open(&mut self.show_about)
                .show(ui.ctx(), |ui| {
                    ui.label(format!("modelwarden {}", env!("CARGO_PKG_VERSION")));
                    ui.label("Inventory, backup, and archival for local model files.");
                    ui.label("Owns storage truth. Never loses bytes.");
                });
        }
    }
}
