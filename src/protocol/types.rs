//! Quassel domain types that travel on the wire: identifiers, `BufferInfo`,
//! and the IRC `Message` struct with its type/flag enums.

use std::fmt;

use chrono::{DateTime, Utc};

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident($inner:ty)) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub $inner);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// Quassel `BufferId`, a signed 32-bit buffer row id.
    BufferId(i32)
);
id_type!(
    /// Quassel `NetworkId`, a signed 32-bit network row id.
    NetworkId(i32)
);
id_type!(
    /// Quassel `IdentityId`, a signed 32-bit identity row id.
    IdentityId(i32)
);
id_type!(
    /// Quassel `UserId`, a signed 32-bit core user account id.
    UserId(i32)
);
id_type!(
    /// Quassel `AccountId`, a signed 32-bit client-side account id.
    AccountId(i32)
);
id_type!(
    /// Quassel `MsgId`. Modern cores use qint64; we always read 64 bits.
    MsgId(i64)
);

/// Mirror of `BufferInfo::Type`. Unknown wire values decode to `Invalid` so
/// a future buffer kind doesn't break the rest of the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(i16)]
pub enum BufferType {
    Invalid = 0x00,
    Status = 0x01,
    Channel = 0x02,
    Query = 0x04,
    Group = 0x08,
}

impl BufferType {
    pub fn from_wire(value: i16) -> Self {
        match value {
            0x01 => Self::Status,
            0x02 => Self::Channel,
            0x04 => Self::Query,
            0x08 => Self::Group,
            _ => Self::Invalid,
        }
    }

    pub fn value(self) -> i16 {
        self as i16
    }

    /// Short label used by the CLI summaries.
    pub fn label(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Channel => "chan",
            Self::Query => "query",
            Self::Group => "group",
            Self::Invalid => "?",
        }
    }
}

/// One Quassel buffer row. `name` travels as UTF-8 bytes, not a QString.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BufferInfo {
    pub buffer_id: BufferId,
    pub network_id: NetworkId,
    pub kind: BufferType,
    pub group_id: u32,
    pub name: String,
}

/// Mirror of `Message::Type`. Unknown values decode to `Plain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum MessageType {
    Plain = 0x00001,
    Notice = 0x00002,
    Action = 0x00004,
    Nick = 0x00008,
    Mode = 0x00010,
    Join = 0x00020,
    Part = 0x00040,
    Quit = 0x00080,
    Kick = 0x00100,
    Kill = 0x00200,
    Server = 0x00400,
    Info = 0x00800,
    Error = 0x01000,
    DayChange = 0x02000,
    Topic = 0x04000,
    NetsplitJoin = 0x08000,
    NetsplitQuit = 0x10000,
    Invite = 0x20000,
}

impl MessageType {
    const ALL: [Self; 18] = [
        Self::Plain,
        Self::Notice,
        Self::Action,
        Self::Nick,
        Self::Mode,
        Self::Join,
        Self::Part,
        Self::Quit,
        Self::Kick,
        Self::Kill,
        Self::Server,
        Self::Info,
        Self::Error,
        Self::DayChange,
        Self::Topic,
        Self::NetsplitJoin,
        Self::NetsplitQuit,
        Self::Invite,
    ];

    pub fn from_wire(value: u32) -> Self {
        Self::ALL
            .into_iter()
            .find(|t| *t as u32 == value)
            .unwrap_or(Self::Plain)
    }
}

/// Mirror of `Message::Flags`: a real bitfield stored as `quint8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MessageFlags(pub u8);

impl MessageFlags {
    pub const NONE: Self = Self(0x00);
    pub const SELF: Self = Self(0x01);
    pub const HIGHLIGHT: Self = Self(0x02);
    pub const REDIRECTED: Self = Self(0x04);
    pub const SERVER_MSG: Self = Self(0x08);
    pub const STATUS_MSG: Self = Self(0x10);
    pub const IGNORED: Self = Self(0x20);
    pub const BACKLOG: Self = Self(0x80);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0 && other.0 != 0
    }
}

impl std::ops::BitOr for MessageFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// One IRC message as the core stores and replays it.
///
/// Fields that a given feature set doesn't carry on the wire (sender
/// prefixes, real name, avatar URL) decode as empty strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub msg_id: MsgId,
    pub timestamp: DateTime<Utc>,
    pub kind: MessageType,
    pub flags: MessageFlags,
    pub buffer_info: BufferInfo,
    pub sender: String,
    pub sender_prefixes: String,
    pub real_name: String,
    pub avatar_url: String,
    pub contents: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_wire_values_degrade() {
        assert_eq!(BufferType::from_wire(0x40), BufferType::Invalid);
        assert_eq!(BufferType::from_wire(0x04), BufferType::Query);
        assert_eq!(MessageType::from_wire(0xFFFF), MessageType::Plain);
        assert_eq!(MessageType::from_wire(0x20000), MessageType::Invite);
    }

    #[test]
    fn flags_contains() {
        let flags = MessageFlags::SELF | MessageFlags::HIGHLIGHT;
        assert!(flags.contains(MessageFlags::SELF));
        assert!(flags.contains(MessageFlags::HIGHLIGHT));
        assert!(!flags.contains(MessageFlags::BACKLOG));
        assert!(!flags.contains(MessageFlags::NONE));
    }
}
