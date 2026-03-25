use anyhow::Result;
use serde::Serialize;

use crate::cli::StatsArgs;
use crate::error::ClawError;
use crate::output;
use crate::runtime::{ExecOpts, Runtime};
use crate::state::StateStore;
use std::collections::HashMap;

#[derive(Debug, Serialize)]
pub struct MachineStats {
    pub machine_id: String,
    pub cpu_percent: f64,
    pub memory_used_mb: f64,
    pub memory_limit_mb: u32,
    pub memory_percent: f64,
    pub disk_used_mb: f64,
    pub disk_total_mb: f64,
    pub processes: u32,
    pub uptime: String,
}

pub fn run(args: StatsArgs, runtime: &dyn Runtime, state: &StateStore) -> Result<()> {
    // Verify machine exists and is running
    let machine = state.with_read_lock(|s| {
        let m = s.machines.get(&args.machine_id)
            .ok_or_else(|| ClawError::MachineNotFound(args.machine_id.clone()))?;
        if m.status != "running" {
            return Err(ClawError::MachineNotRunning(
                args.machine_id.clone(),
                m.status.clone(),
            ).into());
        }
        Ok(m.clone())
    })?;

    if args.watch > 0 {
        loop {
            // Clear screen
            eprint!("\x1b[2J\x1b[H");
            let stats = collect_stats(&args.machine_id, &machine.memory_mb, runtime)?;
            print_stats(&stats, &args.format);
            std::thread::sleep(std::time::Duration::from_secs(args.watch));
        }
    } else {
        let stats = collect_stats(&args.machine_id, &machine.memory_mb, runtime)?;
        print_stats(&stats, &args.format);
    }

    Ok(())
}

fn collect_stats(machine_id: &str, memory_limit: &u32, runtime: &dyn Runtime) -> Result<MachineStats> {
    let script = r#"
echo "MEM_TOTAL:$(grep MemTotal /proc/meminfo | awk '{print $2}')"
echo "MEM_AVAIL:$(grep MemAvailable /proc/meminfo | awk '{print $2}')"
echo "CPU:$(head -1 /proc/stat)"
echo "PROCS:$(ls -1d /proc/[0-9]* 2>/dev/null | wc -l)"
echo "DISK:$(df -k / | tail -1)"
echo "UPTIME:$(uptime -p 2>/dev/null || cat /proc/uptime | awk '{printf "up %d min", $1/60}')"
sleep 0.1
echo "CPU2:$(head -1 /proc/stat)"
"#;

    let result = runtime.exec(machine_id, ExecOpts {
        command: script.to_string(),
        timeout: 10,
        workdir: "/root".to_string(),
        env: HashMap::new(),
        user: "root".to_string(),
    })?;

    let stdout = &result.stdout;

    // Parse memory
    let mem_total_kb = parse_field(stdout, "MEM_TOTAL:");
    let mem_avail_kb = parse_field(stdout, "MEM_AVAIL:");
    let mem_used_kb = mem_total_kb - mem_avail_kb;
    let mem_used_mb = mem_used_kb as f64 / 1024.0;
    let mem_percent = if mem_total_kb > 0 {
        (mem_used_kb as f64 / mem_total_kb as f64) * 100.0
    } else {
        0.0
    };

    // Parse CPU (two samples for delta)
    let cpu1 = parse_cpu_line(stdout, "CPU:");
    let cpu2 = parse_cpu_line(stdout, "CPU2:");
    let cpu_percent = calculate_cpu_percent(&cpu1, &cpu2);

    // Parse processes
    let procs = parse_field(stdout, "PROCS:") as u32;

    // Parse disk
    let (disk_used_mb, disk_total_mb) = parse_disk(stdout);

    // Parse uptime
    let uptime = stdout.lines()
        .find(|l| l.starts_with("UPTIME:"))
        .map(|l| l.trim_start_matches("UPTIME:").trim().to_string())
        .unwrap_or_else(|| "-".to_string());

    Ok(MachineStats {
        machine_id: machine_id.to_string(),
        cpu_percent: (cpu_percent * 10.0).round() / 10.0,
        memory_used_mb: (mem_used_mb * 10.0).round() / 10.0,
        memory_limit_mb: *memory_limit,
        memory_percent: (mem_percent * 10.0).round() / 10.0,
        disk_used_mb,
        disk_total_mb,
        processes: procs,
        uptime,
    })
}

fn print_stats(stats: &MachineStats, format: &str) {
    if format == "json" {
        output::print_json(stats);
        return;
    }

    println!("Sandbox: {}", stats.machine_id);
    println!("Uptime:  {}", stats.uptime);
    println!();
    println!(
        "CPU:     {:.1}%",
        stats.cpu_percent
    );
    println!(
        "Memory:  {:.1} MB / {} MB ({:.1}%)",
        stats.memory_used_mb, stats.memory_limit_mb, stats.memory_percent
    );
    println!(
        "Disk:    {:.0} MB / {:.0} MB",
        stats.disk_used_mb, stats.disk_total_mb
    );
    println!("Procs:   {}", stats.processes);
}

fn parse_field(stdout: &str, prefix: &str) -> u64 {
    stdout.lines()
        .find(|l| l.starts_with(prefix))
        .and_then(|l| l.trim_start_matches(prefix).trim().parse().ok())
        .unwrap_or(0)
}

fn parse_cpu_line(stdout: &str, prefix: &str) -> Vec<u64> {
    stdout.lines()
        .find(|l| l.starts_with(prefix))
        .map(|l| {
            l.trim_start_matches(prefix)
                .split_whitespace()
                .skip(1) // skip "cpu"
                .filter_map(|v| v.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn calculate_cpu_percent(cpu1: &[u64], cpu2: &[u64]) -> f64 {
    if cpu1.len() < 4 || cpu2.len() < 4 {
        return 0.0;
    }
    let total1: u64 = cpu1.iter().sum();
    let total2: u64 = cpu2.iter().sum();
    let idle1 = cpu1[3]; // 4th field is idle
    let idle2 = cpu2[3];

    let total_delta = total2.saturating_sub(total1) as f64;
    let idle_delta = idle2.saturating_sub(idle1) as f64;

    if total_delta == 0.0 {
        return 0.0;
    }

    ((total_delta - idle_delta) / total_delta) * 100.0
}

fn parse_disk(stdout: &str) -> (f64, f64) {
    stdout.lines()
        .find(|l| l.starts_with("DISK:"))
        .map(|l| {
            let parts: Vec<&str> = l.trim_start_matches("DISK:")
                .split_whitespace()
                .collect();
            if parts.len() >= 4 {
                let total: f64 = parts[1].parse().unwrap_or(0.0) / 1024.0;
                let used: f64 = parts[2].parse().unwrap_or(0.0) / 1024.0;
                (used, total)
            } else {
                (0.0, 0.0)
            }
        })
        .unwrap_or((0.0, 0.0))
}
