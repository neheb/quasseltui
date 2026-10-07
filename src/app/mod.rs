//! The terminal UI (ratatui + crossterm).
//!
//! - [`model`] — the UI state machine, pure and unit-tested.
//! - [`log_view`] — scrollback layout and message-anchored scrolling.
//! - [`format`] — message and sidebar text, sanitized.
//! - [`view`] — drawing.
//! - [`theme`] — terminal palette plus the Omarchy accent, live-reloaded.
//! - [`runtime`] — the terminal event loop.
//! - [`demo`] — offline placeholder state for `ui-demo`.
//! - [`last_buffer`] — the buffer to start in, remembered across runs.

pub mod demo;
pub mod format;
pub mod last_buffer;
pub mod log_view;
pub mod model;
pub mod runtime;
pub mod theme;
pub mod view;

pub use model::{App, Effect, Exit};
pub use runtime::{ClientFactory, run};
