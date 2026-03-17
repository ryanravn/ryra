use serde::{Deserialize, Serialize};

/// Internal state managed by ryra (state.toml). Tracks port allocations only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub allocated: Vec<PortAllocation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortAllocation {
    pub service: String,
    pub port_name: String,
    pub host_port: u16,
}
