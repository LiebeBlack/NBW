//! storage — Persistence and data layer.
//!
//! Fast local storage with embedded engines: serialized JSON/TOML-style
//! configuration files (hand-written serializer/parser — the project's
//! zero-external-dependency policy), written **atomically** (temp file +
//! rename) so a crash can never corrupt state, and **bounded** so no
//! volatile state ever grows unbounded in RAM or on disk.
//!
//! Nothing here touches the UI thread: reads happen once at startup and
//! writes are debounced onto a worker thread by the app layer.

pub mod prefs;

pub use prefs::{from_json, load, store, to_json, Preferences};
