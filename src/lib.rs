//! Object Audio Metadata, the payload that describes where the objects of an immersive mix sit
//! and how the bed folds down.
//!
//! This crate currently provides the OAMD parser used by `truehdd/truehd`, and general-purpose
//! use across codecs is future work. Native AC-4 metadata has a different wire syntax and is
//! not supported by this parser.
//!
//! A payload is read from the bytes a carrier hands over:
//!
//! ```
//! use oamd::ObjectAudioMetadataPayload;
//!
//! # fn main() -> anyhow::Result<()> {
//! let payload = ObjectAudioMetadataPayload::read(oamd::EXAMPLE_DATA)?;
//! println!("{} objects", payload.object_count);
//! # Ok(())
//! # }
//! ```
//!
//! # Scope
//!
//! Parsing only, and no rendering: this crate says what a payload states, not what a renderer
//! should do about it. Fields mix wire codes with decoded values: bed downmix gains retain
//! codes, while object gain, priority, and ramp duration are converted by the parser.

#![forbid(unsafe_code)]

mod bitstream;
mod payload;

pub use payload::*;
