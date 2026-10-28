//! Firmware update check.
//!
//! The official app asks Arturia Software Center, which needs an account.
//! The same information is public: Arturia's resources page for the
//! MiniFuse links the newest firmware file
//! (`MiniFuse1_Firmware_Update_1_5_0_212_win.mf`). The page is fetched by
//! the system (`curl` on macOS, WinHTTP on Windows), which keeps a TLS stack
//! out of this program. Updating itself is left to the official tools linked
//! from the same page.

use std::fmt;

use crate::tr;

pub const RESOURCES_PAGE: &str = "https://www.arturia.com/products/audio/minifuse/resources";
const FILE_PREFIX: &str = "MiniFuse1_Firmware_Update_";

/// `major.minor.patch`, as the device reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version([u32; 3]);

impl Version {
    /// Reads `1.5.0` or `1_5_0` (anything after the third number is ignored).
    pub fn parse(text: &str) -> Option<Self> {
        let mut numbers = text
            .split(['.', '_'])
            .map(|part| part.trim().parse::<u32>().ok());
        Some(Self([numbers.next()??, numbers.next()??, numbers.next()??]))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch] = self.0;
        write!(f, "{major}.{minor}.{patch}")
    }
}

/// The newest MiniFuse 1 firmware linked from the resources page.
pub fn latest_in(page: &str) -> Option<Version> {
    page.match_indices(FILE_PREFIX)
        .filter_map(|(at, _)| {
            let rest = &page[at + FILE_PREFIX.len()..];
            let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '_'))?;
            Version::parse(&rest[..end])
        })
        .max()
}

/// Asks Arturia's website for the newest firmware. Blocks for a few seconds.
pub fn fetch_latest() -> Result<Version, String> {
    let page = platform::fetch(RESOURCES_PAGE)?;
    latest_in(&page).ok_or_else(|| tr!("Arturia's page lists no MiniFuse 1 firmware"))
}

/// Opens the download page in the browser.
pub fn open_page() -> Result<(), String> {
    platform::open(RESOURCES_PAGE).map_err(|error| tr!("Couldn't open the browser: {}", error))
}

#[cfg(target_os = "macos")]
mod platform {
    use std::process::{Command, Stdio};

    use crate::tr;

    const TIMEOUT_SECONDS: &str = "20";

    pub fn fetch(url: &str) -> Result<String, String> {
        let output = Command::new("curl")
            .args(["-fsSL", "--max-time", TIMEOUT_SECONDS, url])
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| format!("curl: {error}"))?;
        if !output.status.success() {
            let reason = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(if reason.is_empty() {
                tr!("Arturia's website did not answer")
            } else {
                reason
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub fn open(url: &str) -> Result<(), String> {
        Command::new("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(drop)
            .map_err(|error| error.to_string())
    }
}

/// WinHTTP and the shell rather than `curl` and `rundll32` started hidden:
/// a console program downloading in the background is what antivirus
/// heuristics look for in malware.
#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::ptr;

    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Networking::WinHttp::{
        INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
        WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect,
        WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData,
        WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    };
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    use crate::tr;

    /// For each step: finding the host, connecting, sending, receiving.
    const TIMEOUT_MS: i32 = 20_000;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Why the last WinHTTP call failed; its codes have no system message.
    fn failure() -> String {
        // SAFETY: plain Win32 call.
        let code = unsafe { GetLastError() };
        format!(
            "{} (WinHTTP {code})",
            tr!("Arturia's website did not answer")
        )
    }

    fn checked(ok: i32) -> Result<(), String> {
        if ok != 0 { Ok(()) } else { Err(failure()) }
    }

    /// A WinHTTP handle, closed when dropped.
    struct Handle(*mut c_void);

    impl Handle {
        fn new(raw: *mut c_void) -> Result<Self, String> {
            if raw.is_null() {
                Err(failure())
            } else {
                Ok(Self(raw))
            }
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: opened by WinHTTP, closed once.
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }

    /// GETs an `https://` address; redirects are followed.
    pub fn fetch(url: &str) -> Result<String, String> {
        let rest = url.strip_prefix("https://").ok_or("not an HTTPS address")?;
        let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let agent = wide(concat!("VibeFuse/", env!("CARGO_PKG_VERSION")));
        let (host, path, verb, any) = (wide(host), wide(path), wide("GET"), wide("*/*"));
        let accept = [any.as_ptr(), ptr::null()];
        // SAFETY: WinHTTP's calls in their documented order; the strings and
        // buffers outlive the calls, the handles close in reverse order.
        unsafe {
            let session = Handle::new(WinHttpOpen(
                agent.as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                ptr::null(),
                ptr::null(),
                0,
            ))?;
            checked(WinHttpSetTimeouts(
                session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS,
            ))?;
            let connection = Handle::new(WinHttpConnect(
                session.0,
                host.as_ptr(),
                INTERNET_DEFAULT_HTTPS_PORT,
                0,
            ))?;
            let request = Handle::new(WinHttpOpenRequest(
                connection.0,
                verb.as_ptr(),
                path.as_ptr(),
                ptr::null(),
                ptr::null(),
                accept.as_ptr(),
                WINHTTP_FLAG_SECURE,
            ))?;
            checked(WinHttpSendRequest(
                request.0,
                ptr::null(),
                0,
                ptr::null(),
                0,
                0,
                0,
            ))?;
            checked(WinHttpReceiveResponse(request.0, ptr::null_mut()))?;
            let mut status = 0u32;
            let mut size = size_of::<u32>() as u32;
            checked(WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                ptr::null(),
                (&raw mut status).cast(),
                &mut size,
                ptr::null_mut(),
            ))?;
            if !(200..300).contains(&status) {
                return Err(format!("HTTP {status}"));
            }
            let mut page = Vec::new();
            let mut chunk = [0u8; 16 * 1024];
            loop {
                let mut read = 0u32;
                checked(WinHttpReadData(
                    request.0,
                    chunk.as_mut_ptr().cast(),
                    chunk.len() as u32,
                    &mut read,
                ))?;
                if read == 0 {
                    break;
                }
                page.extend_from_slice(&chunk[..read as usize]);
            }
            Ok(String::from_utf8_lossy(&page).into_owned())
        }
    }

    pub fn open(url: &str) -> Result<(), String> {
        let (verb, url) = (wide("open"), wide(url));
        // SAFETY: plain Win32 call with NUL-terminated strings.
        let result = unsafe {
            ShellExecuteW(
                ptr::null_mut(),
                verb.as_ptr(),
                url.as_ptr(),
                ptr::null(),
                ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        // Above 32 is success.
        if result.addr() > 32 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_newest_firmware_on_the_page() {
        let page = r#"<a href="/api/download/products/minifuse-1/firmware/MiniFuse1_Firmware_Update_1_0_4_140_win.mf">
            /api/download/products/minifuse-1/firmware/MiniFuse1_Firmware_Update_1_5_0_212_win.mf
            MiniFuse1_Firmware_Update_1_0_5_151.mf
            MiniFuse4_Firmware_Update_1_0_1_35_win.mf4"#;
        assert_eq!(latest_in(page), Some(Version([1, 5, 0])));
        assert_eq!(latest_in("nothing here"), None);
    }

    #[test]
    fn versions_compare_numerically() {
        let device = Version::parse("1.5.0").unwrap();
        assert!(Version::parse("1.10.0").unwrap() > device);
        assert_eq!(Version::parse("1_5_0_212"), Some(device));
        assert_eq!(device.to_string(), "1.5.0");
        assert_eq!(Version::parse("1.5"), None);
    }
}
