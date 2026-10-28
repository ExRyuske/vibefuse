use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Element introspection (used by the UI tests) needs debug info in the
    // generated code; release builds go without it.
    let debug = std::env::var("PROFILE").is_ok_and(|profile| profile == "debug");
    // Texts are English; the translations in translations/ are compiled in,
    // one catalog for the whole app (no per-component contexts).
    let config = slint_build::CompilerConfiguration::new()
        .with_debug_info(debug)
        .with_bundled_translations("translations")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    println!("cargo:rerun-if-changed=translations");
    slint_build::compile_with_config("ui/app.slint", config)
        .expect("failed to compile ui/app.slint");

    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => build_mixer_driver(),
        Ok("windows") => embed_windows_resources(),
        _ => {}
    }
}

fn run(command: &mut Command) {
    let status = command
        .status()
        .unwrap_or_else(|e| panic!("failed to run {command:?}: {e}"));
    assert!(status.success(), "{command:?} failed");
}

/// Builds the audio server plug-in with the virtual devices (embedded in
/// the app, which installs it) and the program that tests it.
fn build_mixer_driver() {
    for file in ["VibeFuseMixer.c", "Info.plist", "test_mixer.c"] {
        println!("cargo:rerun-if-changed=driver/macos/{file}");
    }
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    let bundle = Path::new(&out).join("VibeFuseMixer.driver");
    let contents = bundle.join("Contents");
    let _ = std::fs::remove_dir_all(&bundle);
    std::fs::create_dir_all(contents.join("MacOS")).expect("bundle directory");
    std::fs::copy("driver/macos/Info.plist", contents.join("Info.plist")).expect("Info.plist");

    let flags = [
        "-std=c11",
        "-O2",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-Wno-four-char-constants",
        "-Wno-multichar",
        "-mmacosx-version-min=11.0",
        "-framework",
        "CoreAudio",
        "-framework",
        "CoreFoundation",
    ];
    run(Command::new("clang")
        .args(flags)
        .args(["-bundle", "-arch", "arm64", "-arch", "x86_64", "-o"])
        .arg(contents.join("MacOS/VibeFuseMixer"))
        .arg("driver/macos/VibeFuseMixer.c"));
    // coreaudiod only loads signed code; an ad-hoc signature is enough for a
    // plug-in installed on this machine.
    run(Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(&bundle));
    run(Command::new("clang")
        .args(flags)
        .arg("-o")
        .arg(Path::new(&out).join("test_mixer"))
        .arg("driver/macos/test_mixer.c"));
}

/// The .exe's manifest: it runs as the user who starts it (without the
/// request, Windows guesses whether a program is an installer that wants an
/// administrator) and is made for Windows 10 and 11.
const WINDOWS_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>
"#;

/// The program icon, version information and manifest of the .exe,
/// compiled with whichever resource compiler the toolchain has. Without one
/// the build goes on: the window still sets its icon at run time.
fn embed_windows_resources() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest = out.join("vibefuse.manifest");
    std::fs::write(&manifest, WINDOWS_MANIFEST).expect("manifest");
    let version = std::env::var("CARGO_PKG_VERSION").expect("version");
    let numbers: Vec<&str> = version.split(['.', '-']).take(3).collect();
    let [major, minor, patch] = [0, 1, 2].map(|i| numbers.get(i).copied().unwrap_or("0"));
    let icon = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icon.ico");
    let script = format!(
        r#"1 ICON "{icon}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "VibeFuse"
      VALUE "ProductName", "VibeFuse"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "LegalCopyright", "GPL-3.0-or-later"
      VALUE "OriginalFilename", "vibefuse.exe"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = icon.display().to_string().replace('\\', "/"),
        manifest = manifest.display().to_string().replace('\\', "/"),
    );
    let rc = out.join("vibefuse.rc");
    std::fs::write(&rc, script).expect("resource script");

    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|env| env == "msvc");
    let (mut tools, compiled): (Vec<PathBuf>, PathBuf) = if msvc {
        (
            vec!["rc".into(), "llvm-rc".into()],
            out.join("vibefuse.res"),
        )
    } else {
        (
            vec!["x86_64-w64-mingw32-windres".into(), "windres".into()],
            out.join("vibefuse-res.o"),
        )
    };
    if msvc {
        tools.extend(windows_sdk_rc());
    }
    let mut refused = Vec::new();
    for tool in &tools {
        let mut command = Command::new(tool);
        if msvc {
            command.arg("/nologo").arg("/fo").arg(&compiled).arg(&rc);
        } else {
            command.arg("-i").arg(&rc).arg("-o").arg(&compiled);
        }
        match command.status() {
            Ok(status) if status.success() => {
                println!("cargo:rustc-link-arg-bins={}", compiled.display());
                return;
            }
            Ok(_) => refused.push(tool.display().to_string()),
            Err(_) => {}
        }
    }
    // A script the compilers refuse would otherwise leave the .exe without
    // its manifest unnoticed.
    assert!(
        refused.is_empty(),
        "{} could not compile {}",
        refused.join(", "),
        rc.display()
    );
    println!("cargo:warning=no resource compiler found: the .exe gets no icon nor manifest");
}

/// `rc.exe` of the newest Windows SDK: outside a developer prompt it is not
/// on the path.
fn windows_sdk_rc() -> Option<PathBuf> {
    let root = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| path.join(r"x64\rc.exe").exists())
        .collect();
    versions.sort();
    Some(versions.pop()?.join(r"x64\rc.exe"))
}
