use std::sync::atomic::{AtomicU16, Ordering};

/// Allocates unique SSH ports for parallel VMs.
/// Starts at 10022 and increments. Each VM gets its own port.
static NEXT_PORT: AtomicU16 = AtomicU16::new(10022);

pub fn allocate_ssh_port() -> u16 {
    NEXT_PORT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_are_unique() {
        let a = allocate_ssh_port();
        let b = allocate_ssh_port();
        let c = allocate_ssh_port();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_eq!(b, a + 1);
        assert_eq!(c, a + 2);
    }
}
