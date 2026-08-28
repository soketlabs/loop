mod app;
mod approval;
mod chat_ui;
mod controller;
mod editor_launcher;
mod markdown;
mod session_title;
mod state;

pub use app::run_desktop;
pub use controller::{DesktopCommand, DesktopController, DesktopSnapshot};
