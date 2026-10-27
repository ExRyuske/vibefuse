//! Firmware update check.
//!
//! The official app asks Arturia Software Center, which needs an account.
//! The same information is public: Arturia's resources page for the
//! MiniFuse links the newest firmware file
//! (`MiniFuse1_Firmware_Update_1_5_0_212_win.mf`). The page is fetched with
//! the system's `curl` (part of macOS and of Windows 10 and later), which
//! keeps a TLS stack out of this program. Updating itself is left to the
//! official tools linked from the same page.

use std::fmt;
use std::process::{Command, Stdio};

use crate::tr;

pub const RESOURCES_PAGE: &str = "https://www.arturia.com/products/audio/minifuse/resources";
const FILE_PREFIX: &str = "MiniFuse1_Firmware_Update_";
const TIMEOUT_SECONDS: &str = "20";

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

fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// `CREATE_NO_WINDOW`: no console flashes up.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Asks Arturia's website for the newest firmware. Blocks for a few seconds.
pub fn fetch_latest() -> Result<Version, String> {
    let output = command("curl")
        .args(["-fsSL", "--max-time", TIMEOUT_SECONDS, RESOURCES_PAGE])
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
    latest_in(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| tr!("Arturia's page lists no MiniFuse 1 firmware"))
}

/// Opens the download page in the browser.
pub fn open_page() -> Result<(), String> {
    let mut command = if cfg!(target_os = "macos") {
        command("open")
    } else if cfg!(windows) {
        let mut command = command("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else {
        command("xdg-open")
    };
    command
        .arg(RESOURCES_PAGE)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|error| tr!("Couldn't open the browser: {}", error))
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
