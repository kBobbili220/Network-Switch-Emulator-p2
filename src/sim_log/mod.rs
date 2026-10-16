//! Optional event-log infrastructure.
//!
//! When `Simulator::start_logging(path)` is called, the simulator writes a
//! length-prefixed postcard stream of [`LogFrame`]s to `path` for the
//! lifetime of the run. The same schema is consumed by the browser viewer
//! crate to replay the run interactively.
//!
//! Hot-path cost when logging is *not* enabled: a single
//! `Option::is_none()` check at each hook site.

pub mod conv;
pub mod schema;
pub mod writer;

pub use schema::*;
pub use writer::EventLogger;
