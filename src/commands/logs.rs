use anyhow::Result;

use crate::cli::LogsArgs;
use crate::execlog;
use crate::output;

pub fn run(args: LogsArgs) -> Result<()> {
    let entries = execlog::reader::read_last(&args.machine_id, args.last)?;

    if output::resolve_format(&args.format) == "json" {
        output::print_json(&entries);
    } else {
        if entries.is_empty() {
            println!("No exec history for {}.", args.machine_id);
            return Ok(());
        }
        println!(
            "{:<24} {:<40} {:<6} {:<10}",
            "TIMESTAMP", "COMMAND", "EXIT", "DURATION"
        );
        for entry in &entries {
            println!(
                "{:<24} {:<40} {:<6} {:<10}",
                entry.timestamp.format("%Y-%m-%d %H:%M:%S"),
                truncate_str(&entry.command, 38),
                entry.exit_code,
                format!("{}ms", entry.duration_ms),
            );
        }
    }

    Ok(())
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max - 3])
    }
}
