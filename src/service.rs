//! A systemd user service, so this host starts with the graphical session and comes back after a
//! crash: `--install-service` writes `~/.config/systemd/user/sill-linux.service` (with the
//! arguments given beside it) and enables and starts it; `--uninstall-service` stops, disables
//! and removes it. Output goes to the journal: `journalctl --user -u sill-linux -f`.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub const UNIT: &str = "sill-linux.service";

fn unit_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("no HOME")?;
    Ok(base.join("systemd/user").join(UNIT))
}

/// A shell-safe word for ExecStart.
fn word(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c)) {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
    }
}

pub fn unit_text(exe: &str, args: &[String]) -> String {
    let mut cmd = vec![word(exe)];
    cmd.extend(args.iter().map(|a| word(a)));
    format!(
        "[Unit]\n\
         Description=Sill for Linux (streams this desktop to the Sill app)\n\
         After=graphical-session.target\n\
         PartOf=graphical-session.target\n\
         \n\
         [Service]\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n",
        cmd.join(" ")
    )
}

fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl").arg("--user").args(args).status().context("running systemctl")?;
    if !status.success() {
        bail!("systemctl --user {} failed", args.join(" "));
    }
    Ok(())
}

pub fn install(args: &[String]) -> Result<()> {
    let exe = std::env::current_exe().context("finding this program")?;
    let path = unit_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, unit_text(&exe.to_string_lossy(), args))?;
    println!("Wrote {}.", path.display());
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", UNIT])?;
    println!(
        "Sill for Linux now runs in the background and starts with your session.\n\
         Its output: journalctl --user -u sill-linux -f\n\
         Pairing codes appear as notifications. It runs {} (rebuilding replaces it; restart with\n\
         systemctl --user restart sill-linux).",
        exe.display()
    );
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let path = unit_path()?;
    let _ = systemctl(&["disable", "--now", UNIT]);
    if path.exists() {
        std::fs::remove_file(&path)?;
        println!("Removed {}.", path.display());
    }
    systemctl(&["daemon-reload"])?;
    Ok(())
}

pub const EXTENSION_UUID: &str = "sill-linux@sill-linux";
const EXTENSION_FILES: [(&str, &str); 2] = [
    ("metadata.json", include_str!("../gnome-extension/metadata.json")),
    ("extension.js", include_str!("../gnome-extension/extension.js")),
];

/// The GNOME Shell extension that lists windows (built into this program): written to
/// ~/.local/share/gnome-shell/extensions and enabled. GNOME on Wayland loads a new extension only
/// at the next login.
pub fn install_extension() -> Result<()> {
    let home = std::env::var_os("HOME").context("no HOME")?;
    let dir = std::path::PathBuf::from(home).join(".local/share/gnome-shell/extensions").join(EXTENSION_UUID);
    std::fs::create_dir_all(&dir)?;
    for (name, text) in EXTENSION_FILES {
        std::fs::write(dir.join(name), text)?;
    }
    println!("Wrote the GNOME Shell extension to {}.", dir.display());
    let enabled = Command::new("gnome-extensions").args(["enable", EXTENSION_UUID]).stderr(std::process::Stdio::null()).status();
    match enabled {
        Ok(s) if s.success() => println!("Enabled it."),
        _ => {
            // Not loaded yet: put it in the list the shell reads at login.
            let list = Command::new("gsettings").args(["get", "org.gnome.shell", "enabled-extensions"]).output().context("running gsettings")?;
            let current = String::from_utf8_lossy(&list.stdout).trim().to_string();
            if !current.contains(EXTENSION_UUID) {
                let new = match current.trim_start_matches("@as ").trim() {
                    "[]" | "" => format!("['{EXTENSION_UUID}']"),
                    l => format!("{}, '{EXTENSION_UUID}']", l.trim_end_matches(']')),
                };
                let ok = Command::new("gsettings").args(["set", "org.gnome.shell", "enabled-extensions", &new]).status()?.success();
                if !ok {
                    bail!("gsettings could not enable the extension");
                }
            }
            println!("Enabled it for the next login.");
        }
    }
    println!(
        "Log out and in again to load it (GNOME on Wayland loads new extensions only at login); then the\n\
         Sill app's bar lists this machine's windows. Updating it later works the same way."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit() {
        let u = unit_text("/home/a/sill-linux/target/release/sill-linux", &["--name".into(), "My PC".into(), "--open".into()]);
        assert!(u.contains("ExecStart=/home/a/sill-linux/target/release/sill-linux --name \"My PC\" --open\n"));
        assert!(u.contains("WantedBy=graphical-session.target"));
        assert_eq!(word("50%"), "50%"); // plain words stay
        assert_eq!(word("a b%"), "\"a b%%\"");
    }
}
