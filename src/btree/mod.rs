// SPDX-License-Identifier: APSL-1.2

//! A read-only HFS+ B-tree engine.
//!
//! The engine is deliberately independent of any particular tree. The catalog,
//! the extents overflow tree and the attributes tree are all the same
//! structure with different key types, so nothing here knows what a "catalog
//! record" is. Callers decode keys and records through
//! [`key`](key) and their own record types.
//!
//! Mining reference: Apple `core/BTree.c` and the `BTree*` family implement the
//! same engine for the kernel. This is the userspace translation of the
//! algorithms and data structures, with the kernel scaffolding — locking,
//! `vnode`, `buf_meta_t`, `vfs_context` — deliberately dropped.
//!
//! # Scope
//!
//! This is the read half of the engine: node parsing, record addressing,
//! iteration and search. Mutation (`InsertRecord`, `DeleteRecord`, node
//! allocation and tree splitting) is a later milestone, and the structures here
//! are shaped so it can be added without reshaping them.

pub mod header;
pub mod io;
pub mod key;
pub mod node;

pub use header::{BTreeHeader, KeyCompareType};
pub use io::{BTreeFile, HEADER_NODE_NUM};
pub use key::{CatalogKey, ExtentKey, KeyRef};
pub use node::{Node, NodeDescriptor, NodeKind};
