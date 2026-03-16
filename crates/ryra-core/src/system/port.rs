use crate::config::state::State;
use crate::error::{Error, Result};

const PORT_RANGE_START: u16 = 10000;
const PORT_RANGE_END: u16 = 11000;

/// Allocate the next available port from the range, updating state.
pub fn allocate_port(state: &mut State, service: &str, port_name: &str) -> Result<u16> {
    let port = state.next_port;
    if port >= PORT_RANGE_END {
        return Err(Error::PortsExhausted {
            start: PORT_RANGE_START,
            end: PORT_RANGE_END,
        });
    }

    state.allocated.push(crate::config::state::PortAllocation {
        service: service.to_string(),
        port_name: port_name.to_string(),
        host_port: port,
    });
    state.next_port = port + 1;

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
