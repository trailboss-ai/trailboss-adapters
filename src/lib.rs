//! Trailboss's adapters: the code that talks to things outside Trailboss.
//!
//! - [`sources`]: reading each coding agent's session transcripts into [`model::Batch`]es.
//!   Agents' data is only ever read; databases are opened read-only.
//! - [`hosts`]: terminal apps (Herdr, tmux) behind the [`hosts::Host`] trait: list terminals,
//!   look at one, type into it, read its screen.
//!
//! What Trailboss does with them (the index, cost, tracking, resume) lives in the core.

pub mod hosts;
pub mod model;
pub mod sources;
