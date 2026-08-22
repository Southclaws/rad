//! The shared secret guarding the internal listener.

use std::path::Path;

use axum::http::HeaderMap;

/// Why a token could not be loaded. A misconfigured secret stops the process
/// rather than degrading to an open port.
#[derive(Debug)]
pub enum TokenError {
    Unreadable(std::io::Error),
    Empty,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(error) => {
                write!(formatter, "cannot read the relay token file: {error}")
            }
            Self::Empty => write!(formatter, "the relay token file is empty"),
        }
    }
}

impl std::error::Error for TokenError {}

/// A database-scoped shared secret. Not a user credential: it names no
/// account, grants no authorization, and never reaches the database.
pub struct Token {
    value: Vec<u8>,
}

impl Token {
    /// Read the secret from a file.
    ///
    /// A file, never a flag or an inline environment value: argv is readable
    /// by any process on the host, and environment blocks are inherited by
    /// children and captured in crash dumps.
    pub fn load(path: &Path) -> Result<Self, TokenError> {
        let raw = std::fs::read(path).map_err(TokenError::Unreadable)?;
        let trimmed = raw
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .map(|start| {
                let end = raw
                    .iter()
                    .rposition(|byte| !byte.is_ascii_whitespace())
                    .expect("a non-whitespace byte exists");
                raw[start..=end].to_vec()
            })
            .unwrap_or_default();
        if trimmed.is_empty() {
            return Err(TokenError::Empty);
        }
        Ok(Self { value: trimmed })
    }

    #[cfg(test)]
    pub(super) fn from_value(value: &str) -> Self {
        Self {
            value: value.as_bytes().to_vec(),
        }
    }

    pub fn header_value(&self) -> String {
        format!("Bearer {}", String::from_utf8_lossy(&self.value))
    }

    /// Whether the request carries this secret.
    ///
    /// The comparison runs in time independent of how much of the secret is
    /// correct, so a caller cannot learn it one byte at a time from response
    /// timing.
    pub fn authorizes(&self, headers: &HeaderMap) -> bool {
        let Some(header) = headers.get(axum::http::header::AUTHORIZATION) else {
            return false;
        };
        let Some(presented) = header
            .as_bytes()
            .strip_prefix(b"Bearer ")
            .or_else(|| header.as_bytes().strip_prefix(b"bearer "))
        else {
            return false;
        };
        constant_time_eq(presented, &self.value)
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    // Length is not secret, but the loop must still run over a fixed operand
    // so an early return cannot reveal where the values diverge.
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}
