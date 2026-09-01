//! CrewKit core: detect AI clients on this machine, stage a plugin
//! marketplace that serves both the Claude and Codex ecosystems, and
//! install a publisher's kit (plugins + MCP servers) idempotently.
//!
//! Hard rules (see the project brief):
//! - Merge, never overwrite: only entries CrewKit owns are created or
//!   modified; user-configured entries are never touched.
//! - Atomic writes and a snapshot of every file before modification.
//! - Repeated installs update or skip — never duplicate.
//! - OAuth stays entirely inside the clients; CrewKit never sees tokens.
//! - Client paths and commands come from declarative adapters, not code.

pub mod adapter;
pub mod auth;
pub mod bridge;
pub mod cli;
pub mod detect;
pub mod error;
pub mod fsops;
pub mod installer;
pub mod inventory;
pub mod kit;
pub mod kits;
pub mod marketplace;
pub mod mcp;
pub mod paths;
pub mod state;
pub mod translate;

pub use adapter::Adapter;
pub use auth::AuthSession;
pub use error::{Error, Result};
pub use installer::{Engine, InstallReport, InstallScope, ScanReport, StepReport, StepStatus};
pub use kit::Kit;
pub use paths::Paths;
