//! Explicit versioned native names. No lossy string participates in identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum NativeName {
    Posix(Vec<u8>),
    Windows(Vec<u16>),
    Opaque(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidEncoding;

impl NativeName {
    /// The tag includes codec version 1 and key namespace. UTF-16 BE makes
    /// SQLite BLOB order equal to Rust's lexicographic *unit* order, including
    /// unpaired surrogates. This is not case folding or Windows collation.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![1];
        match self {
            Self::Posix(bytes) => {
                out.push(0);
                out.extend(bytes);
            }
            Self::Windows(units) => {
                out.push(1);
                for unit in units {
                    out.extend(unit.to_be_bytes());
                }
            }
            Self::Opaque(bytes) => {
                out.push(2);
                out.extend(bytes);
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, InvalidEncoding> {
        match bytes {
            [1, 0, rest @ ..] => Ok(Self::Posix(rest.to_vec())),
            [1, 1, rest @ ..] if rest.len().is_multiple_of(2) => Ok(Self::Windows(
                rest.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|x| u16::from_be_bytes([x[0], x[1]]))
                    .collect(),
            )),
            [1, 2, rest @ ..] => Ok(Self::Opaque(rest.to_vec())),
            _ => Err(InvalidEncoding),
        }
    }
}

pub fn unsigned(value: u64) -> [u8; 8] {
    value.to_be_bytes()
}
pub fn decode_unsigned(bytes: &[u8]) -> Result<u64, InvalidEncoding> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| InvalidEncoding)?,
    ))
}

/// Seconds from Unix epoch plus 0..1e9 nanoseconds (floor representation),
/// retaining dates before 1970 without REAL conversion.
pub fn timestamp(seconds: i64, nanos: u32) -> Result<(i64, i64), InvalidEncoding> {
    if nanos >= 1_000_000_000 {
        return Err(InvalidEncoding);
    }
    Ok((seconds, i64::from(nanos)))
}
