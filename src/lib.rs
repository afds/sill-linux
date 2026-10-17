//! sill-linux: a Linux (Wayland) host for the Sill iPhone and iPad app.

pub mod protocol;
pub mod server;
pub mod advertise;
#[cfg(feature = "capture")]
pub mod capture;
