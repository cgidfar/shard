//! Typed harness identifiers for coding agent integrations.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ShardError;

/// Known coding agent harnesses that Shard can integrate with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    ClaudeCode,
    Codex,
}

impl fmt::Display for Harness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Harness::ClaudeCode => write!(f, "claude-code"),
            Harness::Codex => write!(f, "codex"),
        }
    }
}

impl FromStr for Harness {
    type Err = ShardError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude-code" => Ok(Harness::ClaudeCode),
            "codex" => Ok(Harness::Codex),
            _ => Err(ShardError::Other(format!("unknown harness type: '{s}'"))),
        }
    }
}
