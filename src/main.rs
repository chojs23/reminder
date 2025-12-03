mod app;
mod domain;
mod github;
mod storage;

use app::{APP_NAME, ReminderApp};
use eframe::NativeOptions;

fn main() -> eframe::Result<()> {
    let options = NativeOptions::default();
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(ReminderApp::new(cc)))),
    )
}
