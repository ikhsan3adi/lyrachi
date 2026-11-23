//! Windows System Media Transport Controls (SMTC) backend.

#[cfg(target_os = "windows")]
mod backend;
#[cfg(target_os = "windows")]
mod com;
mod util;

#[cfg(target_os = "windows")]
pub use backend::{SessionDump, SmtcBackend, dump_sessions, probe, start_monitor};
