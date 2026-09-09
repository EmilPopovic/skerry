// SPDX-License-Identifier: Apache-2.0
//! Sandboxed children: what they answer, and the wire between.
//!
//! **This crate decodes nothing.** [ADR
//! 0006](../../../docs/adr/0006-out-of-process-decoders.md) puts every decoder in a
//! helper executable under a sandbox, because a decoder is the largest attack
//! surface in a file manager and it runs on whatever landed in a download
//! directory. What is here is the three things that are left: the bytes that cross
//! between the two ([`wire`]), the child's lifecycle and the batching that keeps
//! the frame loop out of it ([`host`]), and the lockdown a child applies to itself
//! before it reads a byte ([`sandbox`]).
//!
//! The helpers link this crate for the first and the third. That is the whole
//! reason the sandbox lives here rather than in one of them: two helpers would
//! otherwise be two copies of it, and the second copy is the one that gets a layer
//! wrong.
//!
//! **Nothing above this decides what a file *is*.** Kind detection is
//! `sk_desktop`'s and lives one level up, in the caller, because both crates are
//! level 2 and the layering forbids the edge — which turns out to be the right
//! shape anyway: the caller already holds the MIME database, and a host that
//! guessed from an extension would disagree with the mark drawn beside the name.

#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod archive;
pub mod host;
pub mod sandbox;
pub mod wire;

pub use host::{Answer, Key, Kind, Previews, Want};
pub use wire::{Ask, Refusal, Reply, Request};

mod pipe;
mod seccomp;
