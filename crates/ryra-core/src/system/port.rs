use crate::config::state::State;
use crate::error::{Error, Result};

const PORT_RANGE_START: u16 = 10000;
const PORT_RANGE_END: u16 = 11000;

/// Allocate the next available port from the range, reusing freed ports.
pub fn allocate_port(state: &mut State, service: &str, port_name: &str) -> Result<u16> {
    let used: std::collections::HashSet<u16> = state
        .allocated
        .iter()
        .map(|a| a.host_port)
        .collect();

    let port = (PORT_RANGE_START..PORT_RANGE_END)
        .find(|p| !used.contains(p))
        .ok_or(Error::PortsExhausted {
            start: PORT_RANGE_START,
            end: PORT_RANGE_END,
        })?;

    state.allocated.push(crate::config::state::PortAllocation {
        service: service.to_string(),
        port_name: port_name.to_string(),
        host_port: port,
    });

    Ok(port)
}

/// Deallocate all ports for a service.
pub fn deallocate_ports(state: &mut State, service: &str) {
    state.allocated.retain(|a| a.service != service);
}

/// Get the host port for a service's named port.
pub fn get_port(state: &State, service: &str, port_name: &str) -> Option<u16> {
    state
        .allocated
        .iter()
        .find(|a| a.service == service && a.port_name == port_name)
        .map(|a| a.host_port)
}
