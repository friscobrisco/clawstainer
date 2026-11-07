//! Firewall isolation tests for `clawstainer::vmnet` on a real Linux kernel.
//!
//! Each "VM" is a network namespace connected by a veth pair whose host end
//! carries the VM's TAP name, so the nftables rules see exactly what they
//! would see for a Firecracker TAP.
//!
//! Needs root, `ip`, `nft`, `ping`, and bash. Opt in with:
//!
//! ```text
//! sudo -E CLAW_NET_TESTS=1 cargo test --test vmnet_linux -- --test-threads=1
//! ```

#![cfg(target_os = "linux")]

use std::net::TcpListener;
use std::process::{Command, Output};

use clawstainer::vmnet::{self, VmNet, TABLE};

fn enabled() -> bool {
    let root = unsafe { libc::geteuid() } == 0;
    let opted_in = std::env::var("CLAW_NET_TESTS").as_deref() == Ok("1");
    if !(root && opted_in) {
        eprintln!("skipping: set CLAW_NET_TESTS=1 and run as root");
    }
    root && opted_in
}

fn sh(cmd: &str) -> Output {
    Command::new("bash").args(["-c", cmd]).output().expect("bash")
}

fn ok(cmd: &str) {
    let out = sh(cmd);
    assert!(
        out.status.success(),
        "`{cmd}` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct FakeVm {
    vm: VmNet,
    ns: String,
}

impl FakeVm {
    fn new(slot: u32, id: &str) -> FakeVm {
        let vm = VmNet::new(slot, id).unwrap();
        let ns = format!("clawtest-{}", vm.tap.trim_start_matches("tap-"));
        let _ = sh(&format!("ip netns del {ns} 2>/dev/null; ip link del {} 2>/dev/null", vm.tap));
        ok(&format!("ip netns add {ns}"));
        ok(&format!("ip link add {} type veth peer name eth0 netns {ns}", vm.tap));
        vmnet::attach(&vm).unwrap();
        ok(&format!("ip -n {ns} link set eth0 address {}", vm.mac));
        ok(&format!("ip -n {ns} addr add {}/30 dev eth0", vm.guest_ip));
        ok(&format!("ip -n {ns} link set lo up"));
        ok(&format!("ip -n {ns} link set eth0 up"));
        ok(&format!("ip -n {ns} route add default via {}", vm.host_ip));
        FakeVm { vm, ns }
    }

    /// Run a command inside the VM; true if it succeeded.
    fn run(&self, cmd: &str) -> bool {
        sh(&format!("ip netns exec {} bash -c {}", self.ns, shell_quote(cmd))).status.success()
    }

    fn ping(&self, ip: &str) -> bool {
        self.run(&format!("ping -c1 -W1 {ip}"))
    }

    fn tcp(&self, ip: &str, port: u16) -> bool {
        self.run(&format!("timeout 2 bash -c '</dev/tcp/{ip}/{port}'"))
    }
}

impl Drop for FakeVm {
    fn drop(&mut self) {
        vmnet::teardown(&self.vm);
        let _ = sh(&format!("ip netns del {}", self.ns));
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Packet counter of the first rule in an `inet` chain containing `marker`.
fn counter(chain: &str, marker: &str) -> u64 {
    counter_in("inet", chain, marker)
}

fn counter_in(family: &str, chain: &str, marker: &str) -> u64 {
    let out = sh(&format!("nft list chain {family} {TABLE} {chain}"));
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .find(|l| l.contains(marker))
        .unwrap_or_else(|| panic!("no rule containing {marker:?} in:\n{text}"));
    let after = line.split("packets ").nth(1).expect("rule has no counter");
    after.split_whitespace().next().unwrap().parse().unwrap()
}

#[test]
fn per_vm_isolation() {
    if !enabled() {
        return;
    }
    vmnet::ensure_table().unwrap();
    let a = FakeVm::new(9000, "sb-c1a00001");
    let b = FakeVm::new(9001, "sb-c1a00002");
    let a_gw = a.vm.host_ip.to_string();
    let b_ip = b.vm.guest_ip.to_string();

    // Gateway is reachable; the other VM is not.
    assert!(a.ping(&a_gw), "VM cannot ping its gateway");
    assert!(b.ping(&b.vm.host_ip.to_string()), "VM B cannot ping its gateway");
    let before = counter("forward", "iifname @vm_ifaces oifname @vm_ifaces");
    assert!(!a.ping(&b_ip), "VM A reached VM B");
    assert!(!a.tcp(&b_ip, 22), "VM A opened TCP to VM B");
    assert!(counter("forward", "iifname @vm_ifaces oifname @vm_ifaces") > before);

    // Host services are unreachable from the VM.
    let listener = TcpListener::bind("0.0.0.0:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(!a.tcp(&a_gw, port), "VM reached a host TCP service");
    drop(listener);

    // Source-IP spoofing (claiming B's address) is dropped at prerouting.
    let chain = a.vm.chain();
    let before = counter(&chain, "ip saddr !=");
    assert!(a.run(&format!("ip addr add {b_ip}/32 dev eth0")));
    assert!(!a.run(&format!("ping -c1 -W1 -I {b_ip} {a_gw}")), "spoofed source got through");
    assert!(counter(&chain, "ip saddr !=") > before);
    assert!(a.run(&format!("ip addr del {b_ip}/32 dev eth0")));

    // MAC spoofing: ARP from a forged MAC is dropped by the arp table...
    let host_mac = std::fs::read_to_string(format!("/sys/class/net/{}/address", a.vm.tap)).unwrap();
    let host_mac = host_mac.trim();
    let before = counter_in("arp", &chain, "arp saddr ether !=");
    assert!(a.run("ip link set eth0 address 02:fc:de:ad:be:ef"));
    assert!(a.run("ip neigh flush dev eth0"));
    assert!(!a.ping(&a_gw), "spoofed MAC resolved the gateway");
    assert!(counter_in("arp", &chain, "arp saddr ether !=") > before);
    // ...and IP frames from a forged MAC are dropped even with a static
    // neighbor entry (no ARP needed).
    let before = counter(&chain, "ether saddr !=");
    assert!(a.run(&format!("ip neigh replace {a_gw} lladdr {host_mac} dev eth0 nud permanent")));
    assert!(!a.ping(&a_gw), "spoofed MAC got through");
    assert!(counter(&chain, "ether saddr !=") > before);
    assert!(a.run(&format!("ip link set eth0 address {}", a.vm.mac)));
    assert!(a.run("ip neigh flush dev eth0 nud all"));
    assert!(a.ping(&a_gw), "VM did not recover after restoring its MAC");

    // Metadata / link-local and SMTP are blocked in forward.
    let before = counter("forward", "169.254.0.0/16");
    assert!(!a.tcp("169.254.169.254", 80));
    assert!(counter("forward", "169.254.0.0/16") > before);
    let before = counter("forward", "tcp dport 25");
    let _ = a.tcp("1.1.1.1", 25);
    assert!(counter("forward", "tcp dport 25") > before, "SMTP was not blocked");

    // No IPv6 on the host end.
    let v6 = std::fs::read_to_string(format!("/proc/sys/net/ipv6/conf/{}/disable_ipv6", a.vm.tap)).unwrap();
    assert_eq!(v6.trim(), "1");

    // Outbound internet works through masquerade, if the host has it.
    if sh("timeout 3 bash -c '</dev/tcp/1.1.1.1/443'").status.success() {
        assert!(a.tcp("1.1.1.1", 443), "VM has no outbound internet");
    } else {
        eprintln!("host has no internet; skipping the egress check");
    }

    // Teardown removes the VM's chain and registration.
    let chain_a = a.vm.chain();
    drop(a);
    assert!(!sh(&format!("nft list chain inet {TABLE} {chain_a}")).status.success());
    let ifaces = String::from_utf8_lossy(&sh(&format!("nft list set inet {TABLE} vm_ifaces")).stdout).into_owned();
    assert!(!ifaces.contains("tap-c1a00001"), "{ifaces}");
    assert!(b.ping(&b.vm.host_ip.to_string()), "tearing down A broke B");
}
