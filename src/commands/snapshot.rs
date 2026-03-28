use anyhow::Result;

use crate::cli::{SnapshotCreateArgs, SnapshotDeleteArgs, SnapshotListArgs};
use crate::image;
use crate::output;
use crate::state::StateStore;

pub fn run_create(args: SnapshotCreateArgs, state: &StateStore) -> Result<()> {
    state.get_running_machine(&args.machine_id)?;

    let info = image::snapshot::create(&args.machine_id, &args.name)?;
    output::print_json(&info);
    Ok(())
}

pub fn run_list(args: SnapshotListArgs) -> Result<()> {
    let snapshots = image::snapshot::list()?;

    if args.format == "json" {
        output::print_json(&snapshots);
    } else {
        if snapshots.is_empty() {
            println!("No snapshots.");
        } else {
            println!("{:<24} {:>12} {}", "NAME", "SIZE", "CREATED");
            for s in &snapshots {
                let size_mb = s.size_bytes as f64 / 1024.0 / 1024.0;
                println!("{:<24} {:>9.1} MB {}", s.name, size_mb, s.created_at);
            }
        }
    }

    Ok(())
}

pub fn run_delete(args: SnapshotDeleteArgs) -> Result<()> {
    image::snapshot::delete(&args.name)?;
    eprintln!("Deleted snapshot '{}'", args.name);
    Ok(())
}
