//! sill-linux: a Linux (Wayland) host for the Sill iPhone and iPad app.

pub mod protocol;
pub mod server;
pub mod advertise;
pub mod pairing;
pub mod identity;
pub mod tls;
pub mod door;
pub mod input;
pub mod settings;
#[cfg(feature = "capture")]
pub mod capture;
