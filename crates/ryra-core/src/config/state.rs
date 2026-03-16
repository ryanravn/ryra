use serde::{Deserialize, Serialize};

/// Internal state managed by ryra (state.toml). Users don't edit this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    #[serde(default = "default_next_port")]
    pub next_port: u16,
    #[serde(default)]
    pub allocated: Vec<PortAllocation>,
    #[serde(default)]
    pub secrets: Vec<SecretEntry>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            next_port: default_next_port(),
            allocated: Vec::new(),
            secrets: Vec::new(),
        }
    }
}

fn default_next_port() -> u16 {
    10000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortAllocation {
    pub service: String,
    pub port_name: String,
    pub host_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretEntry {
    pub service: String,
    pub name: String,
    pub value: String,
}
