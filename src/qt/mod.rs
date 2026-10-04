//! Qt binary serialization: `QDataStream` primitives and `QVariant`.
//!
//! The wire format is pinned to `QDataStream::Qt_4_2`, big-endian, which is
//! what Quassel uses on both ends.

pub mod datastream;
pub mod variant;

pub use datastream::{DecodeError, Limits, QDateTime, Reader, Writer};
pub use variant::{Variant, VariantMap, read_variant, write_variant};
