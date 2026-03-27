use anyhow::Result;

use crate::cli::ListArgs;
use crate::output;
use crate::state::StateStore;

pub fn run(args: ListArgs, state: &StateStore) -> Result<()> {
    if args.watch > 0 {
        loop {
            eprint!("\x1b[2J\x1b[H");
            print_list(&args, state)?;
            std::thread::sleep(std::time::Duration::from_secs(args.watch));
        }
    } else {
        print_list(&args, state)?;
    }
    Ok(())
}

fn print_list(args: &ListArgs, state: &StateStore) -> Result<()> {
    let machines = state.with_read_lock(|s| {
        let mut machines: Vec<_> = s.machines.values().cloned().collect();
        if args.status != "all" {
            machines.retain(|m| m.status == args.status);
        }
        Ok(machines)
    })?;

    if args.format == "json" {
        output::print_json(&machines);
    } else {
        // Table format
        if machines.is_empty() {
            println!("No sandboxes found.");
            return Ok(());
        }
        println!(
            "{:<14} {:<14} {:<10} {:<8} {:<6} {:<12} {}",
            "ID", "NAME", "STATUS", "MEMORY", "CPUS", "UPTIME", "IP"
        );
        for m in &machines {
            let uptime = if m.status == "running" {
                let elapsed = chrono::Utc::now().signed_duration_since(m.created_at);
                format_duration(elapsed)
            } else {
                "-".to_string()
            };
            println!(
                "{:<14} {:<14} {:<10} {:<8} {:<6} {:<12} {}",
                m.id,
                m.name,
                m.status,
                format!("{}MB", m.memory_mb),
                m.cpus,
                uptime,
                m.ip.as_deref().unwrap_or("-"),
            );
        }
    }

    Ok(())
}

fn format_duration(d: chrono::Duration) -> String {
    let total_secs = d.num_seconds();
    if total_secs < 60 {
        format!("{total_secs}s")
    } else if total_secs < 3600 {
        format!("{}m", total_secs / 60)
    } else {
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        format!("{h}h {m}m")
    }
}
