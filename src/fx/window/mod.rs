//! The empty window a plug-in puts its editor in. VST2 and VST3 work the
//! same: the host makes a window and hands over its handle (an `NSView` on
//! macOS, an `HWND` on Windows), and the plug-in fills it.
//!
//! The user may close the window from its title bar while the plug-in still
//! lives in it, so closing only hides it; the editor notices
//! ([`PluginWindow::closed_by_user`]) and detaches the plug-in the way its
//! format says before the window goes.

#[cfg(target_os = "macos")]
mod cocoa;
#[cfg(target_os = "macos")]
pub use cocoa::{PluginWindow, Size, client_size, resize_handle};

#[cfg(windows)]
mod win32;
#[cfg(windows)]
pub use win32::{PluginWindow, client_size, resize_handle};

/// The window type as VST3 names it, NUL-terminated.
#[cfg(target_os = "macos")]
pub const PLATFORM_TYPE: &[u8] = b"NSView\0";
#[cfg(windows)]
pub const PLATFORM_TYPE: &[u8] = b"HWND\0";
