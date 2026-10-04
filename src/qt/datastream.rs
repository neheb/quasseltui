//! Big-endian QDataStream reader/writer for Qt binary serialization.
//!
//! Wire format rules (Qt's `QDataStream::Qt_4_2`, which Quassel pins both
//! ends to):
//!
//! - All integers are big-endian.
//! - Booleans are one byte (0 or 1).
//! - QString is a `quint32` *byte* length (not character count) followed by
//!   raw UTF-16BE code units. `0xFFFFFFFF` is the null QString.
//! - QByteArray has the same shape with raw bytes; `0xFFFFFFFF` is null.
//! - QDateTime is `quint32 julian_day`, `quint32 ms_since_midnight`,
//!   `bool is_utc` (9 bytes).
//!
//! The reader enforces size limits on every attacker-controlled length
//! prefix so a malformed core message can't make us allocate gigabytes,
//! and a nesting limit so deeply nested containers fail with a typed error
//! instead of overflowing the stack.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};

use crate::protocol::features::Features;

pub const DEFAULT_MAX_STRING_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_BYTEARRAY_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_CONTAINER_ITEMS: usize = 1_000_000;
/// Real Quassel payloads nest QVariant containers 6-8 levels deep at most.
/// The bound exists because a few KB of nested empty lists would otherwise
/// recurse arbitrarily deep.
pub const DEFAULT_MAX_NESTING_DEPTH: usize = 32;

/// Julian Day Number of the proleptic Gregorian date 0001-01-01, which is
/// day 1 in chrono's `num_days_from_ce` numbering.
const GREGORIAN_EPOCH_JDN: i64 = 1_721_425;
const NULL_LEN: u32 = 0xFFFF_FFFF;
const MS_PER_DAY: u32 = 86_400_000;

/// Decoding failed: truncated buffer, bad encoding, or a limit was hit.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DecodeError(pub String);

impl DecodeError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// Bounds on attacker-controlled sizes. The defaults are conservative for
/// an IRC client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_string_bytes: usize,
    pub max_bytearray_bytes: usize,
    pub max_container_items: usize,
    pub max_nesting_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_string_bytes: DEFAULT_MAX_STRING_BYTES,
            max_bytearray_bytes: DEFAULT_MAX_BYTEARRAY_BYTES,
            max_container_items: DEFAULT_MAX_CONTAINER_ITEMS,
            max_nesting_depth: DEFAULT_MAX_NESTING_DEPTH,
        }
    }
}

/// A Qt 4 wire-format `QDateTime`, kept in its raw wire representation.
///
/// Keeping the raw fields means a value read off the wire re-encodes to the
/// exact same bytes, which is what the HeartBeat reply path needs: the core
/// matches replies to its own timestamps. Conversions to chrono types clamp
/// out-of-range values instead of failing, because Quassel emits 0/0/0 for
/// "no timestamp" and that must not break the decode loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QDateTime {
    pub julian_day: u32,
    pub ms_of_day: u32,
    pub is_utc: bool,
}

impl QDateTime {
    pub fn from_utc(value: DateTime<Utc>) -> Self {
        let mut out = Self::from_naive(value.naive_utc());
        out.is_utc = true;
        out
    }

    /// A naive (local wall-clock) datetime, written with `is_utc = false`.
    pub fn from_naive(value: NaiveDateTime) -> Self {
        let jd = i64::from(value.date().num_days_from_ce()) + GREGORIAN_EPOCH_JDN;
        let time = value.time();
        let ms = time.num_seconds_from_midnight() * 1000 + time.nanosecond() / 1_000_000;
        Self {
            julian_day: u32::try_from(jd).unwrap_or(0),
            ms_of_day: ms.min(MS_PER_DAY - 1),
            is_utc: false,
        }
    }

    pub fn now_utc() -> Self {
        Self::from_utc(Utc::now())
    }

    /// The wall-clock value, clamped into chrono's 0001-01-01..9999-12-31
    /// range the way the Python client clamped into `datetime`'s range.
    pub fn to_naive(&self) -> NaiveDateTime {
        let min = NaiveDate::from_ymd_opt(1, 1, 1).expect("valid date");
        let max = NaiveDate::from_ymd_opt(9999, 12, 31).expect("valid date");
        let ordinal = i64::from(self.julian_day) - GREGORIAN_EPOCH_JDN;
        let date = if ordinal < 1 {
            min
        } else if ordinal > i64::from(max.num_days_from_ce()) {
            max
        } else {
            NaiveDate::from_num_days_from_ce_opt(ordinal as i32).unwrap_or(max)
        };
        let ms = self.ms_of_day.min(MS_PER_DAY - 1);
        date.and_time(NaiveTime::MIN) + Duration::milliseconds(i64::from(ms))
    }

    /// Interpret the value as UTC. Naive values are treated as UTC too; the
    /// core always sends UTC timestamps in practice.
    pub fn to_utc(&self) -> DateTime<Utc> {
        self.to_naive().and_utc()
    }
}

/// Sequential reader over a byte slice.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    nesting: usize,
    pub limits: Limits,
    /// Negotiated Quassel features. The `Message` user type's wire shape
    /// depends on them, so the codec consults this.
    pub features: Features,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self::with(data, Limits::default(), Features::empty())
    }

    pub fn with_features(data: &'a [u8], features: Features) -> Self {
        Self::with(data, Limits::default(), features)
    }

    pub fn with(data: &'a [u8], limits: Limits, features: Features) -> Self {
        Self {
            data,
            pos: 0,
            nesting: 0,
            limits,
            features,
        }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn at_end(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// Enter one level of recursive decoding; fails past the nesting limit.
    pub(crate) fn push_nesting(&mut self) -> Result<(), DecodeError> {
        self.nesting += 1;
        if self.nesting > self.limits.max_nesting_depth {
            return Err(DecodeError(format!(
                "QVariant nesting depth {} exceeds max_nesting_depth {}",
                self.nesting, self.limits.max_nesting_depth
            )));
        }
        Ok(())
    }

    pub(crate) fn pop_nesting(&mut self) {
        self.nesting -= 1;
    }

    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|end| *end <= self.data.len());
        let Some(end) = end else {
            return Err(DecodeError(format!(
                "truncated buffer: wanted {n} bytes at offset {}, only {} available",
                self.pos,
                self.remaining()
            )));
        };
        let chunk = &self.data[self.pos..end];
        self.pos = end;
        Ok(chunk)
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let bytes = self.read_bytes(N)?;
        Ok(bytes.try_into().expect("read_bytes returned N bytes"))
    }

    pub fn read_u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.read_array::<1>()?[0])
    }

    pub fn read_u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.read_array()?))
    }

    pub fn read_u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.read_array()?))
    }

    pub fn read_u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.read_array()?))
    }

    pub fn read_i8(&mut self) -> Result<i8, DecodeError> {
        Ok(i8::from_be_bytes(self.read_array()?))
    }

    pub fn read_i16(&mut self) -> Result<i16, DecodeError> {
        Ok(i16::from_be_bytes(self.read_array()?))
    }

    pub fn read_i32(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_be_bytes(self.read_array()?))
    }

    pub fn read_i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_be_bytes(self.read_array()?))
    }

    pub fn read_bool(&mut self) -> Result<bool, DecodeError> {
        Ok(self.read_u8()? != 0)
    }

    pub fn read_f64(&mut self) -> Result<f64, DecodeError> {
        Ok(f64::from_be_bytes(self.read_array()?))
    }

    /// Read a QString. `None` is the null QString.
    ///
    /// Qt strings are sequences of UTF-16 code units with no enforced
    /// surrogate pairing. Rust strings can't hold lone surrogates, so those
    /// decode to U+FFFD rather than failing the whole frame.
    pub fn read_qstring(&mut self) -> Result<Option<String>, DecodeError> {
        let length = self.read_u32()?;
        if length == NULL_LEN {
            return Ok(None);
        }
        let length = length as usize;
        if length == 0 {
            return Ok(Some(String::new()));
        }
        if length > self.limits.max_string_bytes {
            return Err(DecodeError(format!(
                "QString length {length} exceeds max_string_bytes {}",
                self.limits.max_string_bytes
            )));
        }
        if !length.is_multiple_of(2) {
            return Err(DecodeError(format!(
                "QString byte length {length} is not a multiple of 2 (UTF-16)"
            )));
        }
        let raw = self.read_bytes(length)?;
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        Ok(Some(String::from_utf16_lossy(&units)))
    }

    /// Read a QByteArray. `None` is the null QByteArray.
    pub fn read_qbytearray(&mut self) -> Result<Option<Vec<u8>>, DecodeError> {
        let length = self.read_u32()?;
        if length == NULL_LEN {
            return Ok(None);
        }
        let length = length as usize;
        if length > self.limits.max_bytearray_bytes {
            return Err(DecodeError(format!(
                "QByteArray length {length} exceeds max_bytearray_bytes {}",
                self.limits.max_bytearray_bytes
            )));
        }
        Ok(Some(self.read_bytes(length)?.to_vec()))
    }

    pub fn read_qdatetime(&mut self) -> Result<QDateTime, DecodeError> {
        Ok(QDateTime {
            julian_day: self.read_u32()?,
            ms_of_day: self.read_u32()?,
            is_utc: self.read_bool()?,
        })
    }

    /// Validate a container count against the configured limit.
    pub(crate) fn check_count(&self, count: u32, kind: &str) -> Result<usize, DecodeError> {
        let count = count as usize;
        if count > self.limits.max_container_items {
            return Err(DecodeError(format!(
                "{kind} count {count} exceeds max_container_items {}",
                self.limits.max_container_items
            )));
        }
        Ok(count)
    }
}

/// Sequential writer accumulating big-endian Qt-formatted bytes.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
    pub features: Features,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_features(features: Features) -> Self {
        Self {
            buf: Vec::new(),
            features,
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn write_bytes(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn write_u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    pub fn write_u16(&mut self, value: u16) {
        self.write_bytes(&value.to_be_bytes());
    }

    pub fn write_u32(&mut self, value: u32) {
        self.write_bytes(&value.to_be_bytes());
    }

    pub fn write_u64(&mut self, value: u64) {
        self.write_bytes(&value.to_be_bytes());
    }

    pub fn write_i8(&mut self, value: i8) {
        self.write_bytes(&value.to_be_bytes());
    }

    pub fn write_i16(&mut self, value: i16) {
        self.write_bytes(&value.to_be_bytes());
    }

    pub fn write_i32(&mut self, value: i32) {
        self.write_bytes(&value.to_be_bytes());
    }

    pub fn write_i64(&mut self, value: i64) {
        self.write_bytes(&value.to_be_bytes());
    }

    pub fn write_bool(&mut self, value: bool) {
        self.write_u8(u8::from(value));
    }

    pub fn write_f64(&mut self, value: f64) {
        self.write_bytes(&value.to_be_bytes());
    }

    /// Write a QString; `None` writes the null sentinel.
    pub fn write_qstring(&mut self, value: Option<&str>) {
        match value {
            None => self.write_u32(NULL_LEN),
            Some(text) => {
                let units: Vec<u16> = text.encode_utf16().collect();
                self.write_u32((units.len() * 2) as u32);
                for unit in units {
                    self.write_u16(unit);
                }
            }
        }
    }

    /// Write a QByteArray; `None` writes the null sentinel.
    pub fn write_qbytearray(&mut self, value: Option<&[u8]>) {
        match value {
            None => self.write_u32(NULL_LEN),
            Some(bytes) => {
                self.write_u32(bytes.len() as u32);
                self.write_bytes(bytes);
            }
        }
    }

    pub fn write_qdatetime(&mut self, value: &QDateTime) {
        self.write_u32(value.julian_day);
        self.write_u32(value.ms_of_day);
        self.write_bool(value.is_utc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32, ms: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap() + Duration::milliseconds(i64::from(ms))
    }

    #[test]
    fn integer_round_trips() {
        let mut w = Writer::new();
        w.write_u8(255);
        w.write_u16(65535);
        w.write_u32(0xDEAD_BEEF);
        w.write_u64(0xDEAD_BEEF_CAFE_BABE);
        w.write_i8(-128);
        w.write_i16(-32768);
        w.write_i32(-2_000_000_000);
        w.write_i64(-(1 << 62));
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_u8().unwrap(), 255);
        assert_eq!(r.read_u16().unwrap(), 65535);
        assert_eq!(r.read_u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.read_u64().unwrap(), 0xDEAD_BEEF_CAFE_BABE);
        assert_eq!(r.read_i8().unwrap(), -128);
        assert_eq!(r.read_i16().unwrap(), -32768);
        assert_eq!(r.read_i32().unwrap(), -2_000_000_000);
        assert_eq!(r.read_i64().unwrap(), -(1 << 62));
        assert!(r.at_end());
    }

    #[test]
    fn bool_round_trip() {
        let mut w = Writer::new();
        w.write_bool(true);
        w.write_bool(false);
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert!(r.read_bool().unwrap());
        assert!(!r.read_bool().unwrap());
    }

    #[test]
    fn u32_is_big_endian() {
        let mut w = Writer::new();
        w.write_u32(0x0102_0304);
        assert_eq!(w.as_bytes(), b"\x01\x02\x03\x04");
    }

    #[test]
    fn qstring_hello_is_known_blob() {
        let mut w = Writer::new();
        w.write_qstring(Some("Hello"));
        assert_eq!(w.as_bytes(), b"\x00\x00\x00\x0a\x00H\x00e\x00l\x00l\x00o");
    }

    #[test]
    fn null_and_empty_qstring() {
        let mut w = Writer::new();
        w.write_qstring(None);
        assert_eq!(w.as_bytes(), b"\xff\xff\xff\xff");
        assert_eq!(Reader::new(w.as_bytes()).read_qstring().unwrap(), None);

        let mut w = Writer::new();
        w.write_qstring(Some(""));
        assert_eq!(w.as_bytes(), b"\x00\x00\x00\x00");
        assert_eq!(
            Reader::new(w.as_bytes()).read_qstring().unwrap(),
            Some(String::new())
        );
    }

    #[test]
    fn qstring_round_trips() {
        for text in [
            "Hello",
            "",
            "ascii only",
            "naïve",
            "日本語",
            "🎉 emoji 🎉",
            "mixed: αβγ Ω",
            "abc\0def\0\0ghi",
        ] {
            let mut w = Writer::new();
            w.write_qstring(Some(text));
            let bytes = w.into_bytes();
            let mut r = Reader::new(&bytes);
            assert_eq!(r.read_qstring().unwrap().as_deref(), Some(text));
            assert!(r.at_end());
        }
    }

    #[test]
    fn qstring_length_counts_utf16_bytes() {
        let mut w = Writer::new();
        w.write_qstring(Some("ab"));
        assert_eq!(&w.as_bytes()[..4], b"\x00\x00\x00\x04");
        let mut w = Writer::new();
        w.write_qstring(Some("🎉"));
        assert_eq!(&w.as_bytes()[..4], b"\x00\x00\x00\x04");
        let mut w = Writer::new();
        w.write_qstring(Some("abc\0def\0\0ghi"));
        assert_eq!(&w.as_bytes()[..4], b"\x00\x00\x00\x18");
    }

    #[test]
    fn qstring_rejects_odd_length() {
        let err = Reader::new(b"\x00\x00\x00\x03\x00\x48\x00")
            .read_qstring()
            .unwrap_err();
        assert!(err.0.contains("multiple of 2"), "{err}");
    }

    #[test]
    fn lone_surrogate_decodes_to_replacement_char() {
        // "a" + lone high surrogate U+D83D + "b"
        let bytes = b"\x00\x00\x00\x06\x00a\xd8\x3d\x00b";
        let decoded = Reader::new(bytes).read_qstring().unwrap().unwrap();
        assert_eq!(decoded, "a\u{fffd}b");
    }

    #[test]
    fn qbytearray_cases() {
        let mut w = Writer::new();
        w.write_qbytearray(Some(b""));
        assert_eq!(w.as_bytes(), b"\x00\x00\x00\x00");
        assert_eq!(
            Reader::new(w.as_bytes()).read_qbytearray().unwrap(),
            Some(vec![])
        );

        let mut w = Writer::new();
        w.write_qbytearray(None);
        assert_eq!(w.as_bytes(), b"\xff\xff\xff\xff");
        assert_eq!(Reader::new(w.as_bytes()).read_qbytearray().unwrap(), None);

        let payload: Vec<u8> = (0..=255).collect();
        let mut w = Writer::new();
        w.write_qbytearray(Some(&payload));
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_qbytearray().unwrap(), Some(payload));
        assert!(r.at_end());
    }

    #[test]
    fn reader_boundaries() {
        let err = Reader::new(b"\x01\x02").read_u32().unwrap_err();
        assert!(err.0.contains("truncated"));

        let data = b"\x00\x00\x00\x05extra";
        let mut r = Reader::new(data);
        assert_eq!(r.remaining(), 9);
        assert_eq!(r.read_u32().unwrap(), 5);
        assert_eq!(r.position(), 4);
        assert_eq!(r.remaining(), 5);
        assert_eq!(r.read_bytes(5).unwrap(), b"extra");
        assert!(r.at_end());
    }

    #[test]
    fn length_limits() {
        let limits = Limits {
            max_string_bytes: 100,
            max_bytearray_bytes: 100,
            ..Limits::default()
        };
        let mut bad = b"\x00\x00\x04\x00".to_vec();
        bad.extend(std::iter::repeat_n(0u8, 1024));
        let err = Reader::with(&bad, limits, Features::empty())
            .read_qstring()
            .unwrap_err();
        assert!(err.0.contains("exceeds max_string_bytes"));
        let err = Reader::with(&bad, limits, Features::empty())
            .read_qbytearray()
            .unwrap_err();
        assert!(err.0.contains("exceeds max_bytearray_bytes"));

        // Exactly at the limit is fine.
        let at_limit = Limits {
            max_string_bytes: 4,
            max_bytearray_bytes: 4,
            ..Limits::default()
        };
        let mut w = Writer::new();
        w.write_qstring(Some("ab"));
        assert_eq!(
            Reader::with(w.as_bytes(), at_limit, Features::empty())
                .read_qstring()
                .unwrap()
                .as_deref(),
            Some("ab")
        );
        let mut w = Writer::new();
        w.write_qbytearray(Some(b"\x01\x02\x03\x04"));
        assert!(
            Reader::with(w.as_bytes(), at_limit, Features::empty())
                .read_qbytearray()
                .is_ok()
        );

        // A huge (even) length is rejected by the limit, not by truncation.
        let err = Reader::new(b"\x7f\xff\xff\xfe").read_qstring().unwrap_err();
        assert!(err.0.contains("exceeds max_string_bytes"));
    }

    #[test]
    fn truncated_payloads() {
        let err = Reader::new(b"\x00\x00\x00\x0a\x00H\x00e")
            .read_qstring()
            .unwrap_err();
        assert!(err.0.contains("truncated"));
        let err = Reader::new(b"\x00\x00\x00\x10ab")
            .read_qbytearray()
            .unwrap_err();
        assert!(err.0.contains("truncated"));
    }

    #[test]
    fn qdatetime_known_blob() {
        let value = QDateTime::from_utc(utc(2026, 4, 14, 12, 34, 56, 789));
        let mut w = Writer::new();
        w.write_qdatetime(&value);
        assert_eq!(w.as_bytes(), b"\x00\x25\x8d\xd9\x02\xb3\x2c\x95\x01");
    }

    #[test]
    fn qdatetime_utc_round_trip() {
        let original = utc(2026, 4, 14, 12, 34, 56, 789);
        let mut w = Writer::new();
        w.write_qdatetime(&QDateTime::from_utc(original));
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        let decoded = r.read_qdatetime().unwrap();
        assert!(decoded.is_utc);
        assert_eq!(decoded.to_utc(), original);
        assert!(r.at_end());
    }

    #[test]
    fn qdatetime_naive_stays_naive() {
        let naive = NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let mut w = Writer::new();
        w.write_qdatetime(&QDateTime::from_naive(naive));
        let decoded = Reader::new(w.as_bytes()).read_qdatetime().unwrap();
        assert!(!decoded.is_utc);
        assert_eq!(decoded.to_naive(), naive);
    }

    #[test]
    fn qdatetime_zero_julian_day_clamps() {
        let decoded = Reader::new(&[0u8; 9]).read_qdatetime().unwrap();
        assert_eq!(decoded.to_naive().year(), 1);
    }

    #[test]
    fn qdatetime_various_moments_round_trip() {
        for moment in [
            utc(1970, 1, 1, 0, 0, 0, 0),
            utc(2000, 2, 29, 12, 0, 0, 0),
            utc(2038, 1, 19, 3, 14, 7, 0),
            utc(2100, 12, 31, 23, 59, 59, 999),
        ] {
            let mut w = Writer::new();
            w.write_qdatetime(&QDateTime::from_utc(moment));
            let decoded = Reader::new(w.as_bytes()).read_qdatetime().unwrap();
            assert_eq!(decoded.to_utc(), moment);
        }
    }

    #[test]
    fn qdatetime_non_utc_offset_is_converted() {
        let eastern = FixedOffset::west_opt(5 * 3600).unwrap();
        let local = eastern
            .with_ymd_and_hms(2026, 4, 14, 7, 34, 56)
            .unwrap()
            .with_timezone(&Utc);
        let value = QDateTime::from_utc(local);
        assert_eq!(value.to_utc(), utc(2026, 4, 14, 12, 34, 56, 0));
    }

    #[test]
    fn features_flow_through() {
        assert_eq!(Reader::new(b"").features, Features::empty());
        assert_eq!(Writer::new().features, Features::empty());
        let f = Features::LONG_TIME | Features::RICH_MESSAGES;
        assert_eq!(Reader::with_features(b"", f).features, f);
        assert_eq!(Writer::with_features(f).features, f);
    }
}
