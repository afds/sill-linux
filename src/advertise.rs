//! Bonjour: `_sill._tcp` registered through Avahi's `avahi-publish` (avahi-utils), so the app's
//! connect screen lists this machine. The TXT record says how the door speaks: `p=1` (TLS,
//! pairing required) or `p=0` (TLS, any device), with `r`, the recognition tag a paired device
//! knows this host by; no `p` is a plain door, which only a Debug build of the app dials (a
//! Release build shows "Update Sill"). The child dies with this process (PR_SET_PDEATHSIG) and on
//! drop.

use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

pub struct Advertisement(Child);

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn publish(name: &str, port: u16, txt: &[String]) -> Option<Advertisement> {
    let mut cmd = Command::new("avahi-publish");
    cmd.args(["-s", name, "_sill._tcp", &port.to_string()]).args(txt).stdout(Stdio::null()).stderr(Stdio::null());
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
