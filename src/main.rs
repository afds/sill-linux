//! sill-linux: streams this Linux desktop to the Sill app.

use sill_linux::door::PairingDoor;
use sill_linux::identity::{self, Identity};
use sill_linux::input::CommandKey;
use sill_linux::server::{Control, Door, Server};
use std::net::{Ipv6Addr, SocketAddr};
use std::sync::mpsc;

const USAGE: &str = "usage: sill-linux [--port N] [--bitrate MBPS] [--name NAME] [--open | --plain] [--no-advertise]
                  [--view-only | --command-key ctrl|super]
       sill-linux --forget-devices

  --port N          TCP port of the door (default 47460)
  --bitrate MBPS    HEVC bitrate in Mbit/s (default 15, Sill's Balanced)
  --name NAME       the name the app shows (default: this machine's hostname)
  --open            TLS, but any device connects without pairing (TXT p=0)
  --plain           plain TCP, no pairing: only a Debug build of the app connects
  --no-advertise    do not register _sill._tcp with Avahi (connect by address instead)
  --view-only       stream only; ask for no remote control (taps and keys from devices are ignored)
  --command-key K   what the device's Command key is here: ctrl (default: ⌘C copies) or super
  --forget-devices  remove every paired device, then exit

By default the door speaks TLS and a device pairs once with a code this terminal shows (TXT p=1),
as the App Store build of the Sill app expects.";

#[derive(PartialEq)]
enum Mode {
    Paired,
    Open,
    Plain,
}

struct Args {
    port: u16,
    bitrate_mbps: u32,
    name: String,
    advertise: bool,
    mode: Mode,
    forget: bool,
    input: Option<CommandKey>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { port: 47460, bitrate_mbps: 15, name: hostname(), advertise: true, mode: Mode::Paired, forget: false, input: Some(CommandKey::Ctrl) };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--port" => {
                a.port = value("--port")?.parse().map_err(|_| "bad --port")?;
                if a.port == 0 {
                    return Err("--port must be 1 to 65535".into());
                }
            }
            "--bitrate" => {
                a.bitrate_mbps = value("--bitrate")?.parse().map_err(|_| "bad --bitrate")?;
                if !(1..=200).contains(&a.bitrate_mbps) {
                    return Err("--bitrate is 1 to 200".into());
                }
            }
            "--name" => a.name = value("--name")?,
            "--open" => a.mode = Mode::Open,
            "--plain" => a.mode = Mode::Plain,
            "--no-advertise" => a.advertise = false,
            "--forget-devices" => a.forget = true,
            "--view-only" => a.input = None,
            "--command-key" => {
                a.input = Some(match value("--command-key")?.as_str() {
                    "ctrl" => CommandKey::Ctrl,
                    "super" => CommandKey::Super,
                    _ => return Err("--command-key is ctrl or super".into()),
                })
            }
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

    let state = identity::state_dir();
    if args.forget {
        match std::fs::remove_file(state.join("paired.json")) {
            Ok(()) => println!("Forgot every paired device ({}).", state.join("paired.json").display()),
            Err(_) => println!("No paired devices."),
        }
        return;
    }

    let (door, txt) = match args.mode {
        Mode::Plain => (Door::Plain, vec![]),
        Mode::Paired | Mode::Open => {
            let identity = Identity::load_or_create(&state).unwrap_or_else(|e| {
                eprintln!("Cannot load or make this host's identity in {}: {e:#}", state.display());
                std::process::exit(1)
            });
            let require = args.mode == Mode::Paired;
            let txt = vec![format!("p={}", if require { 1 } else { 0 }), format!("r={}", identity.recognition_tag())];
            let door = PairingDoor::new(identity, require, args.name.clone(), args.port).unwrap_or_else(|e| {
                eprintln!("Cannot set up TLS: {e:#}");
                std::process::exit(1)
            });
            (Door::Tls(door), txt)
        }
    };

    let (control_tx, control_rx) = mpsc::channel::<Control>();
    let server = Server::new(args.name.clone(), control_tx, door.clone());
    let addr = match server.listen(SocketAddr::from((Ipv6Addr::UNSPECIFIED, args.port))) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Cannot listen on port {}: {e}", args.port);
            std::process::exit(1);
        }
    };
    match &door {
        Door::Plain => println!("Listening on port {} as \"{}\": plain TCP, only a Debug build of the Sill app connects.", addr.port(), args.name),
        Door::Tls(d) if d.require_pairing => println!(
            "Listening on port {} as \"{}\": TLS, pairing required ({} paired; Mac ID {}).",
            addr.port(),
            args.name,
            d.paired_count(),
            d.identity.mac_id
        ),
        Door::Tls(d) => println!(
            "Listening on port {} as \"{}\": TLS, open to any device without pairing (Mac ID {}).",
            addr.port(),
            args.name,
            d.identity.mac_id
        ),
    }
    let _advert = if args.advertise { sill_linux::advertise::publish(&args.name, addr.port(), &txt) } else { None };

    run_capture(&server, control_rx, args.bitrate_mbps, args.input);
}

#[cfg(feature = "capture")]
fn run_capture(server: &Server, control: mpsc::Receiver<Control>, bitrate_mbps: u32, input: Option<CommandKey>) {
    if let Err(e) = sill_linux::capture::run(server, control, bitrate_mbps, input) {
        eprintln!("Capture failed: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(not(feature = "capture"))]
fn run_capture(_server: &Server, control: mpsc::Receiver<Control>, _bitrate_mbps: u32, _input: Option<CommandKey>) {
    eprintln!("Built without the capture feature: serving the door with no picture.");
    for c in control {
        println!("Control: {c:?}");
    }
}
