//! `ladex autostart`: a node that starts when the person logs in, as the app's Launch at login does for the app.
//! It runs `ladex --data-dir <dir>`, which picks up the passphrase and storage limit saved in that folder.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};
use ladex_core::control::Reply;

use crate::commands::report;

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Switch {
    On,
    Off,
}

pub fn run(switch: Option<Switch>, data_dir: &Path, is_json: bool) -> anyhow::Result<i32> {
    match switch {
        Some(Switch::On) => login_item::install(&node_command(data_dir)?)?,
        Some(Switch::Off) => login_item::remove()?,
        None => {}
    }
    let message = match login_item::is_installed()? {
        true => format!("A node starts when you log in ({}). {}", login_item::describe(), login_item::START_NOW_HINT),
        false => "No node starts when you log in. Turn it on with: ladex autostart on".to_string(),
    };
    Ok(report(&Reply::Acknowledged { message }, is_json))
}

/// The program and arguments that start this node: this very `ladex`, on this data folder.
fn node_command(data_dir: &Path) -> anyhow::Result<Vec<String>> {
    let program = std::env::current_exe().and_then(|exe| exe.canonicalize()).context("could not tell where this ladex is")?;
    let data_dir = std::path::absolute(data_dir).context("could not resolve the data folder")?;
    Ok(vec![text(&program)?, "--data-dir".to_string(), text(&data_dir)?])
}

fn text(path: &Path) -> anyhow::Result<String> {
    path.to_str().map(String::from).with_context(|| format!("{} is not valid text, so it cannot go in a login item", path.display()))
}

fn run_tool(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new(program).args(args).output().with_context(|| format!("could not run {program}"))?;
    if !output.status.success() {
        bail!("{program} {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

#[cfg(unix)]
fn home() -> anyhow::Result<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from).context("HOME is not set")
}

/// A systemd user service: it starts at login, and at boot too once `loginctl enable-linger` is set.
#[cfg(target_os = "linux")]
mod login_item {
    use std::path::PathBuf;

    use super::*;

    const UNIT: &str = "ladex.service";
    pub const START_NOW_HINT: &str = "To start it now: systemctl --user start ladex";

    fn unit_path() -> anyhow::Result<PathBuf> {
        let config = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => home()?.join(".config"),
        };
        Ok(config.join("systemd/user").join(UNIT))
    }

    // Quoted for systemd, where % and $ would otherwise be expanded.
    fn quoted(argument: &str) -> String {
        let escaped = argument.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$");
        format!("\"{escaped}\"")
    }

    fn systemctl(args: &[&str]) -> anyhow::Result<()> {
        run_tool("systemctl", &[&["--user"], args].concat())
    }

    pub fn unit_file(command: &[String]) -> String {
        let exec = command.iter().map(|argument| quoted(argument)).collect::<Vec<_>>().join(" ");
        format!(
            "[Unit]\nDescription=LADEX node\nAfter=network-online.target\n\n[Service]\nExecStart={exec}\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n"
        )
    }

    pub fn install(command: &[String]) -> anyhow::Result<()> {
        let path = unit_path()?;
        std::fs::create_dir_all(path.parent().expect("the unit is inside a folder"))?;
        std::fs::write(&path, unit_file(command)).with_context(|| format!("could not write {}", path.display()))?;
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", UNIT])
    }

    pub fn remove() -> anyhow::Result<()> {
        let path = unit_path()?;
        if !path.exists() {
            return Ok(());
        }
        systemctl(&["disable", UNIT])?;
        std::fs::remove_file(&path).with_context(|| format!("could not remove {}", path.display()))?;
        systemctl(&["daemon-reload"])
    }

    pub fn is_installed() -> anyhow::Result<bool> {
        Ok(unit_path()?.exists())
    }

    pub fn describe() -> String {
        format!("systemd user service {UNIT}")
    }
}

/// A launch agent, which macOS starts at login.
#[cfg(target_os = "macos")]
mod login_item {
    use std::path::PathBuf;

    use super::*;

    const LABEL: &str = "io.github.gshreekar.ladex.node";
    pub const START_NOW_HINT: &str = "To start it now: launchctl load ~/Library/LaunchAgents/io.github.gshreekar.ladex.node.plist";

    fn plist_path() -> anyhow::Result<PathBuf> {
        Ok(home()?.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
    }

    fn xml_escaped(text: &str) -> String {
        text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    pub fn install(command: &[String]) -> anyhow::Result<()> {
        let arguments: String = command.iter().map(|argument| format!("    <string>{}</string>\n", xml_escaped(argument))).collect();
        let log = home()?.join("Library/Logs/ladex-node.log");
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n{arguments}  </array>\n  \
             <key>RunAtLoad</key>\n  <true/>\n  <key>StandardErrorPath</key>\n  <string>{}</string>\n</dict>\n</plist>\n",
            xml_escaped(&text(&log)?)
        );
        let path = plist_path()?;
        std::fs::create_dir_all(path.parent().expect("the agent is inside a folder"))?;
        std::fs::write(&path, plist).with_context(|| format!("could not write {}", path.display()))
    }

    pub fn remove() -> anyhow::Result<()> {
        let path = plist_path()?;
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("could not remove {}", path.display()))?;
        }
        Ok(())
    }

    pub fn is_installed() -> anyhow::Result<bool> {
        Ok(plist_path()?.exists())
    }

    pub fn describe() -> String {
        format!("launch agent {LABEL}")
    }
}

/// A Run entry for this user; the node's console window starts minimized.
#[cfg(windows)]
mod login_item {
    use super::*;

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "LADEX node";
    pub const START_NOW_HINT: &str = "To start it now, run: ladex";

    pub fn install(command: &[String]) -> anyhow::Result<()> {
        let quoted: Vec<String> = command.iter().map(|argument| format!("\"{argument}\"")).collect();
        let entry = format!("cmd.exe /c start \"{VALUE}\" /min {}", quoted.join(" "));
        run_tool("reg", &["add", RUN_KEY, "/v", VALUE, "/t", "REG_SZ", "/d", &entry, "/f"])
    }

    pub fn remove() -> anyhow::Result<()> {
        if !is_installed()? {
            return Ok(());
        }
        run_tool("reg", &["delete", RUN_KEY, "/v", VALUE, "/f"])
    }

    pub fn is_installed() -> anyhow::Result<bool> {
        let output = Command::new("reg").args(["query", RUN_KEY, "/v", VALUE]).output().context("could not run reg")?;
        Ok(output.status.success())
    }

    pub fn describe() -> String {
        format!("the \"{VALUE}\" entry under {RUN_KEY}")
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod login_item {
    use super::*;

    pub const START_NOW_HINT: &str = "";

    pub fn install(_command: &[String]) -> anyhow::Result<()> {
        bail!("starting a node at login is not supported on this system")
    }

    pub fn remove() -> anyhow::Result<()> {
        Ok(())
    }

    pub fn is_installed() -> anyhow::Result<bool> {
        Ok(false)
    }

    pub fn describe() -> String {
        String::new()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::login_item::unit_file;

    fn exec_line(command: &[&str]) -> String {
        let command: Vec<String> = command.iter().map(|argument| argument.to_string()).collect();
        unit_file(&command).lines().find(|line| line.starts_with("ExecStart=")).unwrap().to_string()
    }

    #[test]
    fn the_service_runs_this_ladex_on_the_data_folder() {
        assert_eq!(
            exec_line(&["/usr/bin/ladex", "--data-dir", "/home/sam/.ladex/files"]),
            r#"ExecStart="/usr/bin/ladex" "--data-dir" "/home/sam/.ladex/files""#
        );
    }

    #[test]
    fn spaces_quotes_and_systemd_specifiers_in_a_path_are_kept_literally() {
        assert_eq!(
            exec_line(&["/opt/my apps/ladex", "--data-dir", "/data/100%\"$HOME"]),
            r#"ExecStart="/opt/my apps/ladex" "--data-dir" "/data/100%%\"$$HOME""#
        );
    }
}
