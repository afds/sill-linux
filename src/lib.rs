//! sill-linux: a Linux (Wayland) host for the Sill iPhone and iPad app.

pub mod protocol;
pub mod server;
pub mod advertise;
pub mod pairing;
pub mod identity;
pub mod tls;
pub mod door;
pub mod input;
pub mod gestures;
pub mod clipboard;
pub mod pointer;
pub mod restore;
pub mod remote;
pub mod settings;
pub mod notify;
pub mod service;
#[cfg(feature = "capture")]
pub mod capture;
#[cfg(feature = "capture")]
pub mod mutter;
#[cfg(feature = "capture")]
pub mod shell;
