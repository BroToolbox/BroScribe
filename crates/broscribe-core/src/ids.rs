//! Stable identifiers: meetings (`m-` + 6 hex), people (`p-` + 6 hex) and
//! per-meeting speakers (`S1`, `S2`, …). Collision checks happen in `library`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

fn is_short_hex(s: &str) -> bool {
    s.len() == 6 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

macro_rules! hex_id {
    ($name:ident, $prefix:literal, $what:literal) => {
        #[doc = concat!("A ", $what, " ID: `", $prefix, "` followed by 6 lowercase hex digits.")]
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// A new random ID; the caller checks it against the library.
            pub fn random() -> Self {
                let n: u32 = rand::random::<u32>() & 0x00ff_ffff;
                Self(format!(concat!($prefix, "{:06x}"), n))
            }

            pub fn parse(s: &str) -> Result<Self> {
                match s.strip_prefix($prefix) {
                    Some(hex) if is_short_hex(hex) => Ok(Self(s.to_owned())),
                    _ => Err(Error::InvalidInput(format!(
                        concat!("not a ", $what, " ID (expected ", $prefix, "xxxxxx): {:?}"),
                        s
                    ))),
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// The 6 hex digits after the prefix.
            pub fn short_hex(&self) -> &str {
                &self.0[$prefix.len()..]
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self> {
                Self::parse(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;
            fn try_from(s: String) -> Result<Self> {
                Self::parse(&s)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }
    };
}

hex_id!(MeetingId, "m-", "meeting");
hex_id!(PersonId, "p-", "person");

/// A speaker within one meeting: `S1`, `S2`, … (1-based).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SpeakerId(u32);

impl SpeakerId {
    /// `n` must be at least 1.
    pub fn new(n: u32) -> Result<Self> {
        if n == 0 {
            return Err(Error::InvalidInput("speaker numbers start at 1".into()));
        }
        Ok(Self(n))
    }

    pub fn number(self) -> u32 {
        self.0
    }

    pub fn parse(s: &str) -> Result<Self> {
        let invalid =
            || Error::InvalidInput(format!("not a speaker ID (expected S1, S2, …): {s:?}"));
        let digits = s.strip_prefix('S').ok_or_else(invalid)?;
        if digits.is_empty()
            || !digits.bytes().all(|b| b.is_ascii_digit())
            || digits.starts_with('0')
        {
            return Err(invalid());
        }
        let n = digits.parse::<u32>().map_err(|_| invalid())?;
        Self::new(n)
    }
}

impl fmt::Display for SpeakerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "S{}", self.0)
    }
}

impl FromStr for SpeakerId {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

impl TryFrom<String> for SpeakerId {
    type Error = Error;
    fn try_from(s: String) -> Result<Self> {
        Self::parse(&s)
    }
}

impl From<SpeakerId> for String {
    fn from(id: SpeakerId) -> String {
        id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_ids_are_valid() {
        for _ in 0..100 {
            let m = MeetingId::random();
            assert_eq!(MeetingId::parse(m.as_str()).unwrap(), m);
            let p = PersonId::random();
            assert!(p.as_str().starts_with("p-") && p.short_hex().len() == 6);
        }
    }

    #[test]
    fn parse_rejects_bad_ids() {
        assert!(MeetingId::parse("m-7f3a2c").is_ok());
        for bad in ["m-7F3A2C", "m-7f3a2", "p-7f3a2c", "m-7f3a2cz", "m-zzzzzz"] {
            assert!(MeetingId::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(SpeakerId::parse("S12").unwrap().number(), 12);
        for bad in ["S0", "S", "s1", "S01", "S1a"] {
            assert!(SpeakerId::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn serde_as_strings() {
        let s = SpeakerId::new(3).unwrap();
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"S3\"");
        let p: PersonId = serde_json::from_str("\"p-0a1b2c\"").unwrap();
        assert_eq!(p.to_string(), "p-0a1b2c");
        assert!(serde_json::from_str::<PersonId>("\"m-0a1b2c\"").is_err());
    }
}
