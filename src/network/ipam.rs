use anyhow::Result;

use super::NetworkState;

/// Allocate the next available IP from the 10.0.0.0/24 subnet
pub fn allocate(network: &mut NetworkState) -> Result<String> {
    // Find next free octet (2-254)
    let mut octet = network.next_octet;
    loop {
        if octet > 254 {
            anyhow::bail!("IP address pool exhausted (10.0.0.2 - 10.0.0.254)");
        }
        let ip = format!("10.0.0.{octet}");
        if !network.allocated_ips.contains_key(&ip) {
            network.allocated_ips.insert(ip.clone(), String::new());
            network.next_octet = octet + 1;
            return Ok(ip);
        }
        octet += 1;
    }
}

/// Release an IP address back to the pool
pub fn release(network: &mut NetworkState, ip: &str) {
    network.allocated_ips.remove(ip);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocate_sequential() {
        let mut net = NetworkState::default();
        let ip1 = allocate(&mut net).unwrap();
        assert_eq!(ip1, "10.0.0.2");
        let ip2 = allocate(&mut net).unwrap();
        assert_eq!(ip2, "10.0.0.3");
    }

    #[test]
    fn test_release_and_reallocate() {
        let mut net = NetworkState::default();
        let ip1 = allocate(&mut net).unwrap();
        let _ip2 = allocate(&mut net).unwrap();
        release(&mut net, &ip1);
        // Next allocation skips past released IP (goes to next_octet)
        let ip3 = allocate(&mut net).unwrap();
        assert_eq!(ip3, "10.0.0.4");
    }

    #[test]
    fn test_pool_exhaustion() {
        let mut net = NetworkState::default();
        for _ in 2..=254 {
            allocate(&mut net).unwrap();
        }
        assert!(allocate(&mut net).is_err());
    }
}
