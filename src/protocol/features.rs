//! Quassel feature negotiation: the stringly-named `FeatureList` and the
//! legacy binary `Features` bitmask.
//!
//! The negotiated set is the intersection of what we offer and what the
//! core advertises. Three of the features change the `Message` user type's
//! byte layout (`LongTime`, `SenderPrefixes`, `RichMessages`), so the set
//! travels with every reader and writer.

use std::fmt;
use std::ops::{BitAnd, BitOr, BitOrAssign};

/// The set of Quassel features this client knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Features(u8);

impl Features {
    /// Message timestamps are qint64 ms-since-epoch instead of quint32
    /// seconds (which wrap in 2038).
    pub const LONG_TIME: Self = Self(1 << 0);
    /// Messages carry a `senderPrefixes` field (`@`, `+`, ...).
    pub const SENDER_PREFIXES: Self = Self(1 << 1);
    /// Messages carry `realName` and `avatarUrl`.
    pub const RICH_MESSAGES: Self = Self(1 << 2);
    /// MsgId is qint64. We always read it as qint64; advertising this just
    /// tells the core not to bother with the legacy path.
    pub const LONG_MESSAGE_ID: Self = Self(1 << 3);

    const NAMED: [(Self, &'static str); 4] = [
        (Self::LONG_TIME, "LongTime"),
        (Self::SENDER_PREFIXES, "SenderPrefixes"),
        (Self::RICH_MESSAGES, "RichMessages"),
        (Self::LONG_MESSAGE_ID, "LongMessageId"),
    ];

    pub const fn empty() -> Self {
        Self(0)
    }

    /// Everything we advertise in `ClientInit`.
    pub const fn client_default() -> Self {
        Self(0b1111)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Feature names, in the order `ClientInit.FeatureList` sends them.
    pub fn names(self) -> Vec<&'static str> {
        Self::NAMED
            .iter()
            .filter(|(flag, _)| self.contains(*flag))
            .map(|(_, name)| *name)
            .collect()
    }

    /// Known features named in `names`; unknown names are ignored.
    pub fn from_names<S: AsRef<str>>(names: &[S]) -> Self {
        let mut out = Self::empty();
        for name in names {
            if let Some((flag, _)) = Self::NAMED.iter().find(|(_, n)| *n == name.as_ref()) {
                out |= *flag;
            }
        }
        out
    }

    /// Legacy binary bitmask for the features that have a legacy bit.
    ///
    /// `ExtendedFeatures` (bit 15) is deliberately not set: setting it makes
    /// some older cores change their `ClientInitAck` format in ways we don't
    /// handle. The string `FeatureList` is enough for modern cores.
    pub fn to_bitmask(self) -> u32 {
        if self.contains(Self::SENDER_PREFIXES) {
            LEGACY_SENDER_PREFIXES
        } else {
            0
        }
    }

    /// Known features encoded in a legacy binary bitmask.
    pub fn from_bitmask(bitmask: u32) -> Self {
        if bitmask & LEGACY_SENDER_PREFIXES != 0 {
            Self::SENDER_PREFIXES
        } else {
            Self::empty()
        }
    }
}

/// Legacy `Quassel::Feature` bit for SenderPrefixes (bit 13).
pub const LEGACY_SENDER_PREFIXES: u32 = 1 << 13;
/// Legacy bit that signals the core understands string-based negotiation.
pub const LEGACY_EXTENDED_FEATURES: u32 = 1 << 15;

impl BitOr for Features {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Features {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for Features {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl fmt::Display for Features {
    /// Sorted, comma-separated names, or `(none)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names = self.names();
        names.sort_unstable();
        if names.is_empty() {
            f.write_str("(none)")
        } else {
            f.write_str(&names.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_prefixes_maps_to_bit_13() {
        assert_ne!(
            Features::SENDER_PREFIXES.to_bitmask() & LEGACY_SENDER_PREFIXES,
            0
        );
    }

    #[test]
    fn features_without_legacy_bits_produce_zero() {
        assert_eq!(Features::LONG_TIME.to_bitmask(), 0);
        assert_eq!(Features::empty().to_bitmask(), 0);
    }

    #[test]
    fn default_features_never_set_extended_bit() {
        let mask = Features::client_default().to_bitmask();
        assert_ne!(mask & LEGACY_SENDER_PREFIXES, 0);
        assert_eq!(mask & LEGACY_EXTENDED_FEATURES, 0);
    }

    #[test]
    fn bitmask_to_features() {
        assert!(Features::from_bitmask(0x2000).contains(Features::SENDER_PREFIXES));
        assert!(Features::from_bitmask(0xFEFF).contains(Features::SENDER_PREFIXES));
        assert!(Features::from_bitmask(0).is_empty());
        assert!(Features::from_bitmask(0x0001).is_empty());
    }

    #[test]
    fn names_round_trip() {
        let all = Features::client_default();
        assert_eq!(
            all.names(),
            [
                "LongTime",
                "SenderPrefixes",
                "RichMessages",
                "LongMessageId"
            ]
        );
        assert_eq!(Features::from_names(&all.names()), all);
        assert_eq!(
            Features::from_names(&["LongTime", "SynchronizedMarkerLine"]),
            Features::LONG_TIME
        );
        assert_eq!(Features::empty().to_string(), "(none)");
        assert_eq!(
            (Features::RICH_MESSAGES | Features::LONG_TIME).to_string(),
            "LongTime, RichMessages"
        );
    }
}
