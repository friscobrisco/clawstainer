use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::StatsArgs;
use crate::error::ClawError;
use crate::output;
use crate::runtime::{ExecOpts, Runtime};
use crate::state::StateStore;
use std::collections::HashMap;
use std::process::Command;

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
    let machine_id = args.machine_id.as_ref().unwrap();

    // Verify machine exists and is running
    let machine = state.with_read_lock(|s| {
        let m = s.machines.get(machine_id)
            .ok_or_else(|| ClawError::MachineNotFound(machine_id.clone()))?;
        if m.status != "running" {
            return Err(ClawError::MachineNotRunning(
                machine_id.clone(),
                m.status.clone(),
            ).into());
        }
        Ok(m.clone())
    })?;

    if args.watch > 0 {
        loop {
            // Clear screen
            eprint!("\x1b[2J\x1b[H");
            let stats = collect_stats(machine_id, &machine.memory_mb, runtime)?;
            print_stats(&stats, &args.format);
            std::thread::sleep(std::time::Duration::from_secs(args.watch));
        }
    } else {
        let stats = collect_stats(machine_id, &machine.memory_mb, runtime)?;
        print_stats(&stats, &args.format);
    }

    Ok(())
}

#[derive(Debug, Serialize)]
pub struct GlobalStats {
    pub host_disk_total_mb: f64,
    pub host_disk_used_mb: f64,
    pub host_disk_avail_mb: f64,
    pub host_disk_percent: f64,
    pub sandboxes: Vec<SandboxSummary>,
}

#[derive(Debug, Serialize)]
pub struct SandboxSummary {
    pub machine_id: String,
    pub name: String,
    pub status: String,
    pub disk_used_mb: f64,
}

pub fn run_global(args: &StatsArgs, state: &StateStore) -> Result<()> {
    // Get host disk usage (works both on native Linux and inside Lima VM)
    let df_output = Command::new("df")
        .args(["-k", "/var/lib/clawstainer"])
        .output()
        .or_else(|_| Command::new("df").args(["-k", "/"]).output())
        .context("Failed to get disk usage")?;

    let df_str = String::from_utf8_lossy(&df_output.stdout);
    let (host_total, host_used, host_avail) = parse_df_output(&df_str);

    let host_percent = if host_total > 0.0 {
        (host_used / host_total * 100.0 * 10.0).round() / 10.0
    } else {
        0.0
    };

    // Get per-sandbox disk usage
    let machines = state.with_read_lock(|s| Ok(s.machines.clone()))?;
    let mut sandboxes = Vec::new();

    for (id, machine) in &machines {
        let machine_dir = format!("/var/lib/clawstainer/machines/{}", id);
        let du_output = Command::new("du")
            .args(["-sk", &machine_dir])
            .output();

        let disk_used_mb = match du_output {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout);
                s.split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0) / 1024.0
            }
            _ => 0.0,
        };

        sandboxes.push(SandboxSummary {
            machine_id: id.clone(),
            name: machine.name.clone(),
            status: machine.status.clone(),
            disk_used_mb: (disk_used_mb * 10.0).round() / 10.0,
        });
    }

    let global = GlobalStats {
        host_disk_total_mb: host_total,
        host_disk_used_mb: host_used,
        host_disk_avail_mb: host_avail,
        host_disk_percent: host_percent,
        sandboxes,
    };

    if args.format == "json" {
        output::print_json(&global);
    } else {
        println!("Host Disk: {:.0} MB / {:.0} MB ({:.1}%) — {:.0} MB available",
            global.host_disk_used_mb, global.host_disk_total_mb,
            global.host_disk_percent, global.host_disk_avail_mb);
        println!();

        if global.sandboxes.is_empty() {
            println!("No sandboxes running.");
        } else {
            println!("{:<16} {:<16} {:<10} {:>10}",
                "ID", "NAME", "STATUS", "DISK");
            for sb in &global.sandboxes {
                println!("{:<16} {:<16} {:<10} {:>8.0} MB",
                    sb.machine_id, sb.name, sb.status, sb.disk_used_mb);
            }
        }
    }

    Ok(())
}

fn parse_df_output(output: &str) -> (f64, f64, f64) {
    if let Some(line) = output.lines().nth(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 4 {
            let total = parts[1].parse::<f64>().unwrap_or(0.0) / 1024.0;
            let used = parts[2].parse::<f64>().unwrap_or(0.0) / 1024.0;
            let avail = parts[3].parse::<f64>().unwrap_or(0.0) / 1024.0;
            return (total, used, avail);
        }
    }
    (0.0, 0.0, 0.0)
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
