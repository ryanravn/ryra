use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A single test assertion — used by both VM and live test runners.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TestDef {
    pub name: String,
    pub run: String,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn default_timeout() -> u64 {
    30
}
