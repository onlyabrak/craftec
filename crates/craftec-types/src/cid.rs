//! Content Identifier (CID) — a raw 32-byte BLAKE3 hash.
//!
//! Unlike IPFS CIDs, Craftec CIDs are bare BLAKE3 digests with no multicodec
//! prefix.  They are encoded as lowercase hex strings when displayed or
//! serialized as text.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tracing::{debug, trace};

use crate::error::{CraftecError, Result};

/// The size in bytes of a [`Cid`] (32 bytes = 256-bit BLAKE3 hash).
pub const CID_SIZE: usize = 32;

/// A content identifier: the raw 32-byte BLAKE3 hash of the content it names.
///
/// `Cid` is `Copy`, `PartialEq`, `Eq`, and `Hash`, making it cheap to use
/// as a map key or in sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Cid([u8; CID_SIZE]);

impl Cid {
    /// Create a `Cid` from a pre-computed 32-byte digest.
    #[inline]
    pub fn from_bytes(bytes: [u8; CID_SIZE]) -> Self {
        Self(bytes)
    }

    /// Hash `data` with BLAKE3 and return its `Cid`.
    ///
    /// ```
    /// # use craftec_types::cid::Cid;
    /// let cid = Cid::from_data(b"hello");
    /// assert_eq!(cid, Cid::from_data(b"hello"));
    /// ```
    pub fn from_data(data: &[u8]) -> Self {
        trace!(data_len = data.len(), "computing CID from data");
        let hash = blake3::hash(data);
        let cid = Self(*hash.as_bytes());
        debug!(cid = %cid, "computed CID");
        cid
    }

    /// Verify that `data` hashes to this CID.
    ///
    /// Returns `true` if the content matches.
    pub fn verify(&self, data: &[u8]) -> bool {
        trace!(data_len = data.len(), cid = %self, "verifying CID");
        let expected = Self::from_data(data);
        let ok = expected == *self;
        debug!(cid = %self, verified = ok, "CID verification result");
        ok
    }

    /// Return the raw byte representation.
    #[inline]
    pub fn as_bytes(&self) -> &[u8; CID_SIZE] {
        &self.0
    }
}

// ── Display / FromStr ──────────────────────────────────────────────────────

impl fmt::Display for Cid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl FromStr for Cid {
    type Err = CraftecError;

    fn from_str(s: &str) -> Result<Self> {
        trace!(input = s, "parsing CID from hex string");
        let bytes = hex::decode(s)
            .map_err(|e| CraftecError::IdentityError(format!("invalid CID hex: {e}")))?;
        if bytes.len() != CID_SIZE {
            return Err(CraftecError::IdentityError(format!(
                "CID must be {CID_SIZE} bytes, got {}",
                bytes.len()
            )));
        }
        let mut arr = [0u8; CID_SIZE];
        arr.copy_from_slice(&bytes);
        let cid = Self(arr);
        debug!(cid = %cid, "parsed CID from hex string");
        Ok(cid)
    }
}

// ── Serde ──────────────────────────────────────────────────────────────────

impl Serialize for Cid {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&hex::encode(self.0))
        } else {
            s.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Cid {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            s.parse::<Cid>().map_err(serde::de::Error::custom)
        } else {
            let bytes: &[u8] = serde::de::Deserialize::deserialize(d)?;
            if bytes.len() != CID_SIZE {
                return Err(serde::de::Error::custom(format!(
                    "expected {CID_SIZE} bytes for CID, got {}",
                    bytes.len()
                )));
            }
            let mut arr = [0u8; CID_SIZE];
            arr.copy_from_slice(bytes);
            Ok(Cid(arr))
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_display_from_str() {
        let cid = Cid::from_data(b"craftec");
        let s = cid.to_string();
        assert_eq!(s.len(), CID_SIZE * 2);
        let parsed: Cid = s.parse().unwrap();
        assert_eq!(cid, parsed);
    }

    #[test]
    fn verify_happy_path() {
        let data = b"some piece data";
        let cid = Cid::from_data(data);
        assert!(cid.verify(data));
    }

    #[test]
    fn verify_wrong_data() {
        let cid = Cid::from_data(b"real data");
        assert!(!cid.verify(b"tampered data"));
    }

    #[test]
    fn serde_json_round_trip() {
        let cid = Cid::from_data(b"serde test");
        let json = serde_json::to_string(&cid).unwrap();
        let decoded: Cid = serde_json::from_str(&json).unwrap();
        assert_eq!(cid, decoded);
    }

    #[test]
    fn from_str_rejects_wrong_length() {
        let err = "deadbeef".parse::<Cid>();
        assert!(err.is_err());
    }
}
