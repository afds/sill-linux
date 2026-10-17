//! Bonjour: `_sill._tcp` registered through Avahi's `avahi-publish` (avahi-utils), so the app's
//! connect screen lists this machine. No TXT record yet: no `p` key says "plain door", which a
//! Debug build of the app dials and a Release build shows as "Update Sill" (phase 3 adds TLS
//! and `p`). The child dies with this process (PR_SET_PDEATHSIG) and on drop.

use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

pub struct Advertisement(Child);

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn publish(name: &str, port: u16) -> Option<Advertisement> {
    let mut cmd = Command::new("avahi-publish");
    cmd.args(["-s", name, "_sill._tcp", &port.to_string()]).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: prctl is async-signal-safe; nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(child) => {
            println!("Advertising _sill._tcp as \"{name}\" through Avahi.");
            Some(Advertisement(child))
        }
        Err(e) => {
            eprintln!("Not advertised (avahi-publish: {e}); connect by address.");
            None
        }
    }
}
