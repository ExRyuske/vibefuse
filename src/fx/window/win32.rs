//! Plug-in windows on Windows: a plain top-level window the plug-in puts its
//! child windows in. Its messages are not pumped here: winit's loop already
//! dispatches every window of the thread, ours too.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{COLOR_BTNFACE, HBRUSH};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, GW_CHILD,
    GetClientRect, GetWindow, IsIconic, IsWindowVisible, MoveWindow, RegisterClassW, SW_HIDE,
    SW_SHOW, SWP_NOMOVE, SWP_NOZORDER, SetForegroundWindow, SetWindowPos, ShowWindow, WM_CLOSE,
    WM_SIZE, WNDCLASSW, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_OVERLAPPEDWINDOW,
};

use crate::fx::Result;

/// Clipping keeps the parent from painting over the plug-in's children.
const STYLE: u32 = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_CLIPSIBLINGS;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The outer size of a window with a client area of `width × height`.
fn outer(width: i32, height: i32) -> (i32, i32) {
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: width.max(160),
        bottom: height.max(120),
    };
    // SAFETY: an in-out rectangle.
    unsafe { AdjustWindowRect(&mut frame, STYLE, 0) };
    (frame.right - frame.left, frame.bottom - frame.top)
}

pub struct PluginWindow {
    hwnd: HWND,
}

impl PluginWindow {
    pub fn open(title: &str, width: i32, height: i32) -> Result<Self> {
        let class = class();
        let title = wide(title);
        let (width, height) = outer(width, height);
        // SAFETY: a registered class, NUL-terminated strings.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                STYLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                width,
                height,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err("the window did not open".into());
        }
        Ok(Self { hwnd })
    }

    pub fn show(&self) {
        // SAFETY: our live window.
        unsafe {
            ShowWindow(self.hwnd, SW_SHOW);
            SetForegroundWindow(self.hwnd);
        }
    }

    pub fn resize(&self, width: i32, height: i32) {
        resize_handle(self.hwnd, width, height);
    }

    pub fn handle(&self) -> *mut c_void {
        self.hwnd
    }

    /// The close button only hides the window (see `procedure`); a
    /// minimized window still counts as visible.
    pub fn closed_by_user(&self) -> bool {
        // SAFETY: our live window.
        unsafe { IsWindowVisible(self.hwnd) == 0 && IsIconic(self.hwnd) == 0 }
    }
}

impl Drop for PluginWindow {
    fn drop(&mut self) {
        // The plug-in has let go of the window by now.
        // SAFETY: ours, destroyed once.
        unsafe { DestroyWindow(self.hwnd) };
    }
}

/// Resizes a window by its handle (for VST3's frame, which only has it).
pub fn resize_handle(handle: *mut c_void, width: i32, height: i32) {
    if handle.is_null() {
        return;
    }
    let (width, height) = outer(width, height);
    // SAFETY: a bad handle makes the call fail, nothing worse.
    unsafe {
        SetWindowPos(
            handle,
            std::ptr::null_mut(),
            0,
            0,
            width,
            height,
            SWP_NOMOVE | SWP_NOZORDER,
        );
    }
}

pub fn client_size(handle: *mut c_void) -> (i32, i32) {
    let mut client = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: as above.
    if handle.is_null() || unsafe { GetClientRect(handle, &mut client) } == 0 {
        return (0, 0);
    }
    (client.right - client.left, client.bottom - client.top)
}

/// The plug-in's child window follows ours in size; the close button only
/// hides, as destroying the window under a live plug-in crashes it.
unsafe extern "system" fn procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_SIZE => {
            let (width, height) = client_size(hwnd);
            // SAFETY: our window and its child, if any.
            unsafe {
                let child = GetWindow(hwnd, GW_CHILD);
                if !child.is_null() {
                    MoveWindow(child, 0, 0, width, height, 1);
                }
            }
        }
        WM_CLOSE => {
            // SAFETY: our window.
            unsafe { ShowWindow(hwnd, SW_HIDE) };
            return 0;
        }
        _ => {}
    }
    // SAFETY: everything else goes to the system.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// The window class, registered once.
fn class() -> &'static [u16] {
    use std::sync::OnceLock;
    static NAME: OnceLock<Vec<u16>> = OnceLock::new();
    NAME.get_or_init(|| {
        let name = wide("VibeFusePluginEditor");
        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(procedure),
            cbClsExtra: 0,
            cbWndExtra: 0,
            // SAFETY: this program's module.
            hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            // Without a background the window shows screen garbage until the
            // plug-in draws.
            hbrBackground: (COLOR_BTNFACE + 1) as usize as HBRUSH,
            lpszMenuName: std::ptr::null(),
            lpszClassName: name.as_ptr(),
        };
        // SAFETY: a filled-in class with a name that lives for the program.
        unsafe { RegisterClassW(&class) };
        name
    })
}
