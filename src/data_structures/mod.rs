//! Specialized data structures used by Avian.

pub mod bit_vec;
pub mod graph;
pub mod pair_key;
pub mod sparse_secondary_map;
pub mod stable_graph;

#[cfg(feature = "2d")]
pub use arrayvec::ArrayVec;
