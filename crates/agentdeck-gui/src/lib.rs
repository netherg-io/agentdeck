//! agentdeck GUI shell (iced 0.14): sidebar with Instances, Drift, Plugins and
//! Processes pages. Drift shows per-instance-group drift with Apply / Import /
//! Fix / Re-enable actions behind a preview-then-confirm flow. All data reads
//! are read-only; refresh ticks and actions run off the UI thread via
//! `Task::perform`. Never opens a window in tests: see tests/.

pub mod app;
pub mod data;
pub mod i18n;
pub mod overlay;
pub mod pages;
mod single;
pub mod style;

pub use app::{Message, Page, State};

/// Open the GUI, or raise the one already running. Without a display the
/// backend returns an error (no panic).
pub fn run() -> iced::Result {
    if let single::Claim::Handed = single::claim() {
        return Ok(());
    }
    iced::application(State::new, app::update, app::view)
        .title(app::title)
        .theme(app::theme)
        .style(app::style)
        .subscription(app::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(1100.0, 700.0),
            min_size: Some(iced::Size::new(900.0, 560.0)),
            ..iced::window::Settings::default()
        })
        .run()
}
