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
