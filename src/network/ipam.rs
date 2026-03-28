use anyhow::Result;

use super::NetworkState;

/// Allocate the next available IP from the 10.0.0.0/24 subnet.
/// Stores the machine_id so IPs can be traced back to their owner.
pub fn allocate(network: &mut NetworkState, machine_id: &str) -> Result<String> {
    // Find next free octet (2-254)
    let mut octet = network.next_octet;
    loop {
        if octet > 254 {
            anyhow::bail!(
                "IP address pool exhausted (10.0.0.2 - 10.0.0.254). \
                 Destroy unused machines to free IPs."
            );
        }
        let ip = format!("10.0.0.{octet}");
        if !network.allocated_ips.contains_key(&ip) {
            network.allocated_ips.insert(ip.clone(), machine_id.to_string());
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
        let ip1 = allocate(&mut net, "sb-00000001").unwrap();
        assert_eq!(ip1, "10.0.0.2");
        let ip2 = allocate(&mut net, "sb-00000002").unwrap();
        assert_eq!(ip2, "10.0.0.3");
    }

    #[test]
    fn test_allocate_stores_machine_id() {
        let mut net = NetworkState::default();
        let ip = allocate(&mut net, "sb-aabbccdd").unwrap();
        assert_eq!(net.allocated_ips.get(&ip).unwrap(), "sb-aabbccdd");
    }

    #[test]
    fn test_release_and_reallocate() {
        let mut net = NetworkState::default();
        let ip1 = allocate(&mut net, "sb-00000001").unwrap();
        let _ip2 = allocate(&mut net, "sb-00000002").unwrap();
        release(&mut net, &ip1);
        // Next allocation skips past released IP (goes to next_octet)
        let ip3 = allocate(&mut net, "sb-00000003").unwrap();
        assert_eq!(ip3, "10.0.0.4");
    }

    #[test]
    fn test_release_clears_machine_id() {
        let mut net = NetworkState::default();
        let ip = allocate(&mut net, "sb-00000001").unwrap();
        assert!(net.allocated_ips.contains_key(&ip));
        release(&mut net, &ip);
        assert!(!net.allocated_ips.contains_key(&ip));
    }

    #[test]
    fn test_pool_exhaustion() {
        let mut net = NetworkState::default();
        for i in 2..=254u16 {
            allocate(&mut net, &format!("sb-{:08x}", i)).unwrap();
        }
        let err = allocate(&mut net, "sb-overflow").unwrap_err();
        assert!(err.to_string().contains("exhausted"));
    }
}
