//! Where the effects play. The chain always takes Input 1 of the MiniFuse;
//! the user picks the output: on macOS any device that plays sound (the
//! card's own outputs, a VibeFuse device to reach the mixer, a virtual
//! cable to another app), on Windows a pair of outputs of Arturia's ASIO
//! driver. The stream runs only while the effects are on.
//!
//! The window says what it wants ([`set`]) and looks at how it goes
//! ([`status`]) on its timer.

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::{outputs, refresh, set, status, tick};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{outputs, refresh, set, status, tick};

/// An output to pick.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    /// Kept in the settings: a device UID, or the first ASIO channel.
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    /// The rate the stream runs at, while it runs.
    pub rate: Option<f64>,
    /// Why it does not run.
    pub problem: Option<String>,
}
