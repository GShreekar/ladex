//! On macOS, puts the `ladex` command that ships inside the app on the terminal's PATH.

use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use tauri::{AppHandle, Manager};

// On every Mac's default PATH (/etc/paths).
const LINK: &str = "/usr/local/bin/ladex";
// Written when the person is asked for their password, so they are asked at most once.
const ASKED_MARKER: &str = "cli-link-asked";

/// Links the bundled `ladex` into PATH, leaving alone any `ladex` that did not come from the app.
pub fn ensure_linked(app: &AppHandle) {
    let Some(bundled) = bundled_cli() else { return };
    if !should_link(Path::new(LINK), &bundled) {
        return;
    }
    match link_to(&bundled) {
        Ok(()) => tracing::info!("Linked {LINK} to {}", bundled.display()),
        Err(error) if matches!(error.kind(), ErrorKind::PermissionDenied | ErrorKind::NotFound) => ask_for_password_once(app, bundled),
        Err(error) => tracing::warn!("Could not link {LINK}: {error}"),
    }
}

// Only an app copied into Applications is linked: one run from the disk image or a quarantined copy would leave a dangling link.
fn bundled_cli() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let home_applications = PathBuf::from(std::env::var_os("HOME")?).join("Applications");
    let is_installed = executable.starts_with("/Applications") || executable.starts_with(home_applications);
    let bundled = executable.parent()?.join("ladex");
    (is_installed && bundled.exists()).then_some(bundled)
}

// A missing link, or one to another copy of the app, is ours to set; anything else was installed some other way.
fn should_link(link: &Path, bundled: &Path) -> bool {
    match fs::symlink_metadata(link) {
        Err(error) => error.kind() == ErrorKind::NotFound,
        Ok(metadata) if metadata.file_type().is_symlink() => match fs::read_link(link) {
            Ok(target) => target != bundled && target.to_string_lossy().contains(".app/Contents/MacOS/"),
            Err(_) => false,
        },
        Ok(_) => false,
    }
}

fn link_to(bundled: &Path) -> io::Result<()> {
    if fs::symlink_metadata(LINK).is_ok() {
        fs::remove_file(LINK)?;
    }
    symlink(bundled, LINK)
}

fn ask_for_password_once(app: &AppHandle, bundled: PathBuf) {
    let Ok(config_dir) = app.path().app_config_dir() else { return };
    let marker = config_dir.join(ASKED_MARKER);
    if marker.exists() {
        return;
    }
    if let Err(error) = fs::create_dir_all(&config_dir).and_then(|()| fs::write(&marker, "")) {
        tracing::warn!("Not asking to link {LINK}, since the answer could not be remembered: {error}");
        return;
    }
    // The password dialog blocks until answered, so it waits on its own thread.
    std::thread::spawn(move || {
        let script = format!(
            r#"do shell script "mkdir -p /usr/local/bin && ln -sfn " & quoted form of "{}" & " {LINK}" with prompt "LADEX wants to add the ladex command to your terminal." with administrator privileges"#,
            applescript_escaped(&bundled.to_string_lossy())
        );
        match Command::new("osascript").arg("-e").arg(script).output() {
            Ok(output) if output.status.success() => tracing::info!("Linked {LINK} to {}", bundled.display()),
            Ok(_) => tracing::info!("Not linking {LINK}: the password prompt was cancelled"),
            Err(error) => tracing::warn!("Could not ask to link {LINK}: {error}"),
        }
    });
}

fn applescript_escaped(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}
