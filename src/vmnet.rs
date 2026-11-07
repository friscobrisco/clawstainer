//! Per-VM routed networking for Firecracker sandboxes.
//!
//! Every VM gets its own TAP device and its own `/30` out of
//! [`POOL_CIDR`]: the host end of the TAP is `.1`, the guest is `.2`. There
//! is no shared bridge, so there is no layer-2 path between VMs.
//!
//! One nftables table (`inet clawstainer`) enforces the policy:
//!
//! - **Anti-spoofing** (prerouting, before conntrack): each TAP has its own
//!   chain that drops frames whose source MAC or IPv4 address isn't the
//!   VM's, and drops all IPv6. A matching `arp clawstainer` table pins the
//!   ARP sender MAC and IP, since ARP never reaches `inet` hooks.
//! - **Host protection** (input): a VM may only ping its gateway and get
//!   replies to connections the host opened. Everything else to the host is
//!   dropped.
//! - **Egress** (forward): VM-to-VM, private, link-local/metadata, CGNAT and
//!   loopback destinations are dropped, as is outbound SMTP (port 25).
//!   Everything else is allowed out and masqueraded. Nothing unsolicited is
//!   forwarded in.
//!
//! The rules are produced by pure functions (unit-tested anywhere); the
//! `apply`/`setup`/`teardown` functions run `ip`/`nft` and need Linux + root.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::Ipv4Addr;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// Address pool carved into one `/30` per VM.
pub const POOL_CIDR: &str = "10.200.0.0/16";
const POOL_BASE: u32 = u32::from_be_bytes([10, 200, 0, 0]);
/// Number of `/30` slots in the pool.
pub const SLOTS: u32 = 1 << 14;

pub const TABLE: &str = "clawstainer";

/// Destinations a VM may never reach through the host.
pub const BLOCKED_DESTINATIONS: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

/// Outbound TCP ports blocked for every VM (provider abuse policies).
pub const BLOCKED_TCP_PORTS: &[u16] = &[25];

/// Network identity of one VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmNet {
    pub slot: u32,
    pub tap: String,
    pub host_ip: Ipv4Addr,
    pub guest_ip: Ipv4Addr,
    pub mac: String,
}

impl VmNet {
    /// Derive everything from the slot and the machine id.
    pub fn new(slot: u32, machine_id: &str) -> Result<VmNet> {
        if slot >= SLOTS {
            bail!("network slot {slot} is outside the pool ({SLOTS} slots)");
        }
        let base = POOL_BASE + slot * 4;
        let s = slot.to_be_bytes();
        Ok(VmNet {
            slot,
            tap: tap_name(machine_id),
            host_ip: Ipv4Addr::from(base + 1),
            guest_ip: Ipv4Addr::from(base + 2),
            // 0x02 = locally administered, unicast.
            mac: format!("02:fc:{:02x}:{:02x}:{:02x}:{:02x}", s[0], s[1], s[2], s[3]),
        })
    }

    /// Kernel `ip=` boot argument configuring the guest's eth0.
    pub fn kernel_ip_arg(&self) -> String {
        format!("ip={}::{}:255.255.255.252::eth0:off", self.guest_ip, self.host_ip)
    }

    /// Name of this VM's anti-spoofing chain.
    pub fn chain(&self) -> String {
        format!("vm-{}", self.tap.trim_start_matches("tap-"))
    }
}

/// TAP name for a machine id (`sb-1a2b3c4d` -> `tap-1a2b3c4d`), within the
/// 15-character interface-name limit.
pub fn tap_name(machine_id: &str) -> String {
    let short: String = machine_id.trim_start_matches("sb-").chars().take(11).collect();
    format!("tap-{short}")
}

/// Lowest free slot, given the slots in use.
pub fn allocate_slot(used: &BTreeMap<u32, String>) -> Result<u32> {
    (0..SLOTS)
        .find(|s| !used.contains_key(s))
        .ok_or_else(|| anyhow::anyhow!("VM network pool {POOL_CIDR} is exhausted"))
}

/// The base table: sets, verdict map, and the hook chains. Creating it is
/// idempotent only through [`ensure_table`], which checks first.
pub fn base_ruleset() -> String {
    let blocked = BLOCKED_DESTINATIONS.join(", ");
    let ports = BLOCKED_TCP_PORTS
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"table inet {TABLE} {{
    set vm_ifaces {{
        type ifname
    }}

    map vm_chains {{
        type ifname : verdict
    }}

    chain prerouting {{
        type filter hook prerouting priority raw; policy accept;
        iifname vmap @vm_chains
    }}

    chain input {{
        type filter hook input priority filter; policy accept;
        iifname @vm_ifaces ct state established,related accept
        iifname @vm_ifaces icmp type echo-request limit rate 10/second accept
        iifname @vm_ifaces counter drop
    }}

    chain forward {{
        type filter hook forward priority filter; policy accept;
        iifname @vm_ifaces oifname @vm_ifaces counter drop
        iifname @vm_ifaces ip daddr {{ {blocked} }} counter drop
        iifname @vm_ifaces tcp dport {{ {ports} }} counter drop
        iifname @vm_ifaces accept
        oifname @vm_ifaces ct state established,related accept
        oifname @vm_ifaces counter drop
    }}

    chain postrouting {{
        type nat hook postrouting priority srcnat; policy accept;
        ip saddr {POOL_CIDR} oifname != @vm_ifaces masquerade
    }}
}}

table arp {TABLE} {{
    map vm_chains {{
        type ifname : verdict
    }}

    chain input {{
        type filter hook input priority filter; policy accept;
        iifname vmap @vm_chains
    }}
}}
"#
    )
}

/// Commands that add one VM's chain and register its TAP.
pub fn add_vm_rules(vm: &VmNet) -> String {
    let chain = vm.chain();
    format!(
        r#"add chain inet {TABLE} {chain}
add rule inet {TABLE} {chain} meta nfproto ipv6 counter drop
add rule inet {TABLE} {chain} ether saddr != {mac} counter drop
add rule inet {TABLE} {chain} ip saddr != {ip} counter drop
add element inet {TABLE} vm_ifaces {{ "{tap}" }}
add element inet {TABLE} vm_chains {{ "{tap}" : jump {chain} }}
add chain arp {TABLE} {chain}
add rule arp {TABLE} {chain} arp saddr ether != {mac} counter drop
add rule arp {TABLE} {chain} arp saddr ip != {ip} counter drop
add element arp {TABLE} vm_chains {{ "{tap}" : jump {chain} }}
"#,
        mac = vm.mac,
        ip = vm.guest_ip,
        tap = vm.tap,
    )
}

/// Commands that remove one VM's chain and registrations. Each line is run
/// separately by [`remove_vm`] so a partial earlier setup still cleans up.
pub fn remove_vm_rules(vm: &VmNet) -> Vec<String> {
    vec![
        format!("delete element inet {TABLE} vm_chains {{ \"{}\" }}", vm.tap),
        format!("delete element inet {TABLE} vm_ifaces {{ \"{}\" }}", vm.tap),
        format!("delete chain inet {TABLE} {}", vm.chain()),
        format!("delete element arp {TABLE} vm_chains {{ \"{}\" }}", vm.tap),
        format!("delete chain arp {TABLE} {}", vm.chain()),
    ]
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {cmd}"))?;
    if !out.status.success() {
        bail!(
            "`{cmd} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Feed a script to `nft -f -`.
pub fn nft_script(script: &str) -> Result<()> {
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run nft (is nftables installed?)")?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(script.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("nft rejected the ruleset: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Create the base table if it doesn't exist, and enable forwarding.
pub fn ensure_table() -> Result<()> {
    run("sysctl", &["-q", "-w", "net.ipv4.ip_forward=1"])?;
    let exists = Command::new("nft")
        .args(["list", "table", "inet", TABLE])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !exists {
        nft_script(&base_ruleset())?;
    }
    Ok(())
}

/// Remove both tables (all VMs). Used when the last VM is gone.
pub fn remove_table() -> Result<()> {
    let _ = run("nft", &["delete", "table", "arp", TABLE]);
    run("nft", &["delete", "table", "inet", TABLE])
}

/// Create the TAP, address it, lock it down, and install its rules.
/// On failure, whatever was created is removed again.
pub fn setup(vm: &VmNet) -> Result<()> {
    ensure_table()?;
    let res = run("ip", &["tuntap", "add", "dev", &vm.tap, "mode", "tap"]).and_then(|_| attach(vm));
    if res.is_err() {
        teardown(vm);
    }
    res
}

/// Configure an existing link device named `vm.tap` as this VM's host end:
/// no IPv6, the host address, the firewall rules, then bring it up. Split
/// from [`setup`] so tests can use a veth pair in place of a TAP.
pub fn attach(vm: &VmNet) -> Result<()> {
    run("sysctl", &["-q", "-w", &format!("net.ipv6.conf.{}.disable_ipv6=1", vm.tap)])?;
    run("ip", &["addr", "add", &format!("{}/30", vm.host_ip), "dev", &vm.tap])?;
    nft_script(&add_vm_rules(vm))?;
    run("ip", &["link", "set", "dev", &vm.tap, "up"])
}

/// Best-effort removal of a VM's rules and TAP.
pub fn teardown(vm: &VmNet) {
    for cmd in remove_vm_rules(vm) {
        let _ = nft_script(&cmd);
    }
    let _ = run("ip", &["link", "delete", "dev", &vm.tap]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_map_to_disjoint_slash30s() {
        let a = VmNet::new(0, "sb-00000001").unwrap();
        assert_eq!(a.host_ip, Ipv4Addr::new(10, 200, 0, 1));
        assert_eq!(a.guest_ip, Ipv4Addr::new(10, 200, 0, 2));
        let b = VmNet::new(1, "sb-00000002").unwrap();
        assert_eq!(b.host_ip, Ipv4Addr::new(10, 200, 0, 5));
        assert_eq!(b.guest_ip, Ipv4Addr::new(10, 200, 0, 6));
        let last = VmNet::new(SLOTS - 1, "sb-ffffffff").unwrap();
        assert_eq!(last.guest_ip, Ipv4Addr::new(10, 200, 255, 254));
        assert!(VmNet::new(SLOTS, "sb-x").is_err());
    }

    #[test]
    fn macs_are_unique_local_unicast() {
        let a = VmNet::new(0, "sb-1").unwrap();
        let b = VmNet::new(258, "sb-2").unwrap();
        assert_eq!(a.mac, "02:fc:00:00:00:00");
        assert_eq!(b.mac, "02:fc:00:00:01:02");
        assert_ne!(a.mac, b.mac);
    }

    #[test]
    fn tap_names_fit_ifnamsiz() {
        assert_eq!(tap_name("sb-1a2b3c4d"), "tap-1a2b3c4d");
        assert!(tap_name("sb-0123456789abcdef").len() <= 15);
    }

    #[test]
    fn kernel_ip_arg_points_at_host_end() {
        let vm = VmNet::new(1, "sb-1").unwrap();
        assert_eq!(vm.kernel_ip_arg(), "ip=10.200.0.6::10.200.0.5:255.255.255.252::eth0:off");
    }

    #[test]
    fn allocate_reuses_lowest_free_slot() {
        let mut used = BTreeMap::new();
        assert_eq!(allocate_slot(&used).unwrap(), 0);
        used.insert(0, "a".to_string());
        used.insert(1, "b".to_string());
        used.insert(3, "c".to_string());
        assert_eq!(allocate_slot(&used).unwrap(), 2);
    }

    #[test]
    fn vm_rules_pin_mac_and_ip() {
        let vm = VmNet::new(1, "sb-abcd1234").unwrap();
        let rules = add_vm_rules(&vm);
        assert!(rules.contains("ether saddr != 02:fc:00:00:00:01 counter drop"));
        assert!(rules.contains("ip saddr != 10.200.0.6 counter drop"));
        assert!(rules.contains("meta nfproto ipv6 counter drop"));
        assert!(rules.contains(r#"vm_chains { "tap-abcd1234" : jump vm-abcd1234 }"#));
        let removal = remove_vm_rules(&vm);
        assert!(rules.contains("arp saddr ip != 10.200.0.6 counter drop"));
        assert_eq!(removal.len(), 5);
        assert!(removal[2].ends_with("vm-abcd1234"));
    }

    #[test]
    fn base_ruleset_blocks_the_dangerous_paths() {
        let base = base_ruleset();
        assert!(base.contains("iifname @vm_ifaces oifname @vm_ifaces counter drop"));
        assert!(base.contains("169.254.0.0/16"));
        assert!(base.contains("tcp dport { 25 }"));
        assert!(base.contains("oifname @vm_ifaces counter drop"));
    }
}
