//! Progress of a harvest, for the console or the app window.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A new phase: "Ищу новые видео", "Нарезаю клипы"…
    Stage { text: String },
    /// Progress within the current phase.
    Progress { done: usize, total: usize },
    /// A detail line.
    Log { text: String },
    /// A phrase went into the base.
    Added { pt: String, ru: String, done: usize, want: usize },
}

pub trait Report: Sync {
    fn event(&self, e: Event);
    /// The user pressed "Stop": finish the current step and return.
    fn cancelled(&self) -> bool {
        false
    }
}

impl dyn Report + '_ {
    pub fn stage(&self, text: impl Into<String>) {
        self.event(Event::Stage { text: text.into() });
    }
    pub fn log(&self, text: impl Into<String>) {
        self.event(Event::Log { text: text.into() });
    }
    pub fn progress(&self, done: usize, total: usize) {
        self.event(Event::Progress { done, total });
    }
}

/// Prints to stderr: the command-line harvester.
pub struct Console;

impl Report for Console {
    fn event(&self, e: Event) {
        match e {
            Event::Stage { text } => eprintln!("▶ {text}"),
            Event::Log { text } => eprintln!("  {text}"),
            Event::Added { pt, ru, done, want } => eprintln!("  ✓ [{done}/{want}] {pt}  →  {ru}"),
            Event::Progress { .. } => {}
        }
    }
}
