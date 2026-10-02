//! Provider-neutral usage vocabulary. Absent counters mean unavailable, not zero.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use zbus::zvariant::Type;

pub type TokenUsage = HashMap<String, u64>;
pub const COUNTERS: [(&str, &str); 6] = [
    ("input", "input_tokens"),
    ("output", "output_tokens"),
    ("cache_read_input", "cached_input_tokens"),
    ("cache_write_input", "cache_write_input_tokens"),
    ("reasoning_output", "reasoning_output_tokens"),
    ("total", "total_tokens"),
];

pub fn reasoning_effort(value: Option<&str>) -> String {
    match value {
        Some("none" | "minimal" | "low" | "medium" | "high" | "xhigh") => value.unwrap().to_owned(),
        _ => "unknown".to_owned(),
    }
}

/// Coherent usage observation; timestamps retain the producer's RFC3339 value.
/// Revision is scoped to an exported session object. Epoch changes on resets.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct UsageReport {
    pub epoch: u64,
    pub revision: u64,
    pub timestamp: String,
    pub turn_id: String,
    pub model: String,
    pub reasoning_effort: String,
    pub delta: TokenUsage,
    pub totals: TokenUsage,
}
