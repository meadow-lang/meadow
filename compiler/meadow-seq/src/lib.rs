//! **Meadow's lowering to AxCut**: from Meadow's Core (`meadow-core`) to the
//! IR every back end compiles from (`meadow-axcut`), which is re-exported here
//! whole so that what used `meadow_seq`'s IR still finds it.
//!
//! Everything about AxCut itself -- what the statements mean, how they are
//! printed, the abstract machine that runs them -- is `meadow-axcut`'s. What
//! is here is the one thing only Meadow can say: how a Meadow program becomes
//! one.

pub use meadow_axcut::*;

pub mod cut;
mod lower;
pub use lower::{Lowered, Unsupported, lower_program, lower_program_with};
