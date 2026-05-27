//! A library for the definition of WebAssembly component model types.

#![no_std]
#![deny(missing_docs)]

#[macro_use]
extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

/// Insertion-ordered map used throughout the type system. Uses the standard
/// library's randomized hasher with `std`, and a no_std default hasher without.
#[cfg(feature = "std")]
pub type IndexMap<K, V> = indexmap::IndexMap<K, V, std::hash::RandomState>;
/// Insertion-ordered set used throughout the type system (see [`IndexMap`]).
#[cfg(feature = "std")]
pub type IndexSet<T> = indexmap::IndexSet<T, std::hash::RandomState>;
/// Insertion-ordered map used throughout the type system. Uses the standard
/// library's randomized hasher with `std`, and a no_std default hasher without.
#[cfg(not(feature = "std"))]
pub type IndexMap<K, V> = indexmap::IndexMap<K, V, hashbrown::DefaultHashBuilder>;
/// Insertion-ordered set used throughout the type system (see [`IndexMap`]).
#[cfg(not(feature = "std"))]
pub type IndexSet<T> = indexmap::IndexSet<T, hashbrown::DefaultHashBuilder>;

mod aggregator;
mod checker;
mod component;
mod core;
mod names;
mod package;
mod targets;

pub use aggregator::*;
pub use checker::*;
pub use component::*;
pub use core::*;
pub use names::*;
pub use package::*;
pub use targets::*;
