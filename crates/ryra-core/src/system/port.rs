use crate::config::schema::Config;
use crate::error::{Error, Result};

const PORT_RANGE_START: u16 = 10000;
const PORT_RANGE_END: u16 = 11000;

/// Allocate the next available port from the range, checking against installed services.
pub fn allocate_port(config: &Config) -> Result<u16> {
    let used: std::collections::HashSet<u16> = config
        .services
        .iter()
        .filter_map(|s| s.host_port)
        .collect();

    (PORT_RANGE_START..PORT_RANGE_END)
        .find(|p| !used.contains(p))
        .ok_or(Error::PortsExhausted {
            start: PORT_RANGE_START,
            end: PORT_RANGE_END,
        })
}
