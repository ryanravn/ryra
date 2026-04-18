use std::collections::HashSet;

use crate::config::schema::Config;
use crate::error::{Error, Result};

const PORT_RANGE_START: u16 = 10000;
const PORT_RANGE_END: u16 = 11000;

/// Allocate the next available port from the range, skipping:
/// 1. Ports already assigned to installed services (from `config`)
/// 2. Ports in `extra_used` (for services with multiple `[[ports]]` entries —
///    e.g. ente-web publishing 3000/3002/3003 to distinct host ports)
/// 3. Ports reported as bound by `port_in_use`
///
/// The `port_in_use` callback is passed in rather than probing directly so
/// core can stay deterministic under test and so planning has no system-state
/// side effects: the CLI owns the actual `TcpListener::bind` probe.
pub fn allocate_port_excluding(
    config: &Config,
    extra_used: &HashSet<u16>,
    port_in_use: &dyn Fn(u16) -> bool,
) -> Result<u16> {
    let mut used: HashSet<u16> = config
        .services
        .iter()
        .flat_map(|s| s.ports.values().copied())
        .collect();
    used.extend(extra_used.iter().copied());

    (PORT_RANGE_START..PORT_RANGE_END)
        .find(|p| !used.contains(p) && !port_in_use(*p))
        .ok_or(Error::PortsExhausted {
            start: PORT_RANGE_START,
            end: PORT_RANGE_END,
        })
}
