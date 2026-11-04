//! One adapter per agent: each reads a transcript (or, for opencode, a database) into
//! [`Batch`](crate::model::Batch)es. `custom` reads any JSONL through a mapping from the
//! config; `catalog` covers agents described by data rather than code.

pub mod catalog;
pub mod claude;
pub mod codex;
pub mod custom;
pub mod opencode;
pub mod pi;
