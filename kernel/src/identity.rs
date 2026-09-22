//! Public product identity: the one place the user-visible name lives
//! (banner, shell prompt, `version`). Technical code keeps neutral names
//! (`kernel`, `hal`, `arch`, ...). See docs/adr/0003-brand-migration-harlan.md.

pub const PRODUCT_NAME: &str = "HARLAN OS";
/// Lowercase, space-free form of the name, for identifiers and paths.
pub const PRODUCT_ID: &str = "harlanos";
/// Single source of truth stays the workspace `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SHELL_PROMPT: &str = "Harlan> ";
pub const TAGLINE: &str = "Computing with intent.";
