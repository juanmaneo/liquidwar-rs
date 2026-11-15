//! lwcore: deterministic core of a Rust port of Liquid War.
//!
//! This is a from-scratch port of the *kernel* of Liquid War 6
//! (<https://www.gnu.org/software/liquidwar6/>) — the rules and the famous
//! gradient pathfinding algorithm invented by Thomas Colcombet, as described
//! by Christian Mauduit in the Liquid War 5 algorithm notes
//! (<https://ufoot.org/liquidwar/v5/techinfo/algorithm/en/>).
//!
//! Layers:
//! * [`map`] — battle maps, procedural and ASCII generation, serialization;
//! * [`game`] — the simulation: cursors, armies, gradient, eating rules;
//! * [`proto`] — the wire protocol for lockstep netplay;
//! * [`rng`], [`hash`] — deterministic primitives;
//! * [`tests`] — a portable testkit runnable with `cargo test` or on WASM.

pub mod game;
pub mod hash;
pub mod map;
pub mod proto;
pub mod rng;
pub mod tests;

pub use game::{Command, FighterView, Game, GameParams, Scoreboard, DIRS, INF};
pub use map::Map;
pub use proto::{NetMessage, TeamSetup, PROTO_VERSION};
