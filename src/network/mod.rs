pub mod bridge;
pub mod ipam;
pub mod nat;
pub mod tap;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkState {
    pub bridge: String,
    pub subnet: String,
    pub allocated_ips: HashMap<String, String>, // ip -> machine_id
    pub next_octet: u8,
    /// Firecracker per-VM network slots (see `clawstainer::vmnet`):
    /// slot -> machine_id.
    #[serde(default)]
    pub vm_slots: BTreeMap<u32, String>,
}

impl Default for NetworkState {
    fn default() -> Self {
        Self {
            bridge: "claw-br0".to_string(),
            subnet: "10.0.0.0/24".to_string(),
            allocated_ips: HashMap::new(),
            next_octet: 2,
            vm_slots: BTreeMap::new(),
        }
    }
}
