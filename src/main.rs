//! sill-linux: streams this Linux desktop to the Sill app (phase 1: the Desktop, plain door).

use sill_linux::server::{Control, Server};
use std::net::{Ipv6Addr, SocketAddr};
use std::sync::mpsc;

const USAGE: &str = "usage: sill-linux [--port N] [--bitrate MBPS] [--name NAME] [--no-advertise]

  --port N         TCP port of the door (default 47460; 0 picks one)
  --bitrate MBPS   HEVC bitrate in Mbit/s (default 15, Sill's Balanced)
  --name NAME      the name the app shows (default: this machine's hostname)
  --no-advertise   do not register _sill._tcp with Avahi (connect by address instead)";

struct Args {
    port: u16,
    bitrate_mbps: u32,
    name: String,
    advertise: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { port: 47460, bitrate_mbps: 15, name: hostname(), advertise: true };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--port" => a.port = value("--port")?.parse().map_err(|_| "bad --port")?,
            "--bitrate" => {
                a.bitrate_mbps = value("--bitrate")?.parse().map_err(|_| "bad --bitrate")?;
                if !(1..=200).contains(&a.bitrate_mbps) {
                    return Err("--bitrate is 1 to 200".into());
                }
            }
            "--name" => a.name = value("--name")?,
            "--no-advertise" => a.advertise = false,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "Linux".into())
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}");
            }
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    let (control_tx, control_rx) = mpsc::channel::<Control>();
    let server = Server::new(args.name.clone(), control_tx);
    let addr = match server.listen(SocketAddr::from((Ipv6Addr::UNSPECIFIED, args.port))) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Cannot listen on port {}: {e}", args.port);
            std::process::exit(1);
        }
    };
    println!("Listening on port {} as \"{}\" (plain door: a Debug build of the Sill app connects).", addr.port(), args.name);
    let _advert = if args.advertise { sill_linux::advertise::publish(&args.name, addr.port()) } else { None };

    run_capture(&server, control_rx, args.bitrate_mbps);
}

#[cfg(feature = "capture")]
fn run_capture(server: &Server, control: mpsc::Receiver<Control>, bitrate_mbps: u32) {
    if let Err(e) = sill_linux::capture::run(server, control, bitrate_mbps) {
        eprintln!("Capture failed: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(not(feature = "capture"))]
fn run_capture(_server: &Server, control: mpsc::Receiver<Control>, _bitrate_mbps: u32) {
    eprintln!("Built without the capture feature: serving the door with no picture.");
    for c in control {
        println!("Control: {c:?}");
    }
}
