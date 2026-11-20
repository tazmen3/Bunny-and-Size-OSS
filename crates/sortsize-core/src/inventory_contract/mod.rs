//! Shared V0.5 contracts, independent of storage, acquisition and presentation.
//!
//! These types do not activate the new scanner. The current CLI/GUI keep their
//! existing contracts until their adapters are migrated. See ADR-0038 to 0044.
//! There is deliberately no serialization derive: the durable schema and native
//! name codecs must be versioned explicitly by the inventory store.

mod coverage;
mod identity;
mod ingestion;
mod resources;
mod view;

pub use coverage::*;
pub use identity::*;
pub use ingestion::*;
pub use resources::*;
pub use view::*;
