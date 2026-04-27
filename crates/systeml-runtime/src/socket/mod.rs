//! `.socket` activation engine.
//!
//! [`listen`] handles the bind + listen of every `Listen*=` directive.
//! [`fdpass`] implements the `LISTEN_FDS=` / `LISTEN_PID=` protocol used to
//! hand fds to the launched service.

pub mod fdpass;
pub mod listen;

pub use listen::{bind_all, Listener};
