use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::{FleetCreateArgs, FleetDestroyArgs};
use crate::component::Provisioner;
use crate::error::ClawError;
use crate::output;
use crate::runtime::CreateOpts;
use crate::state::StateStore;

// --- Fleet YAML types ---

#[derive(Debug, Deserialize)]
struct FleetFile {
    machines: Vec<FleetMachineDef>,
}

#[derive(Debug, Deserialize)]
struct FleetMachineDef {
    name: String,
    #[serde(default = "default_count")]
    count: u32,
    #[serde(default = "default_memory")]
    memory: u32,
    #[serde(default = "default_cpus")]
    cpus: u32,
    /// Component or bundle name to provision after creation
    provision: Option<String>,
}

fn default_count() -> u32 { 1 }
fn default_memory() -> u32 { 512 }
fn default_cpus() -> u32 { 1 }

// --- Result types ---

#[derive(Debug, Serialize)]
struct FleetCreateResult {
    fleet: Vec<FleetMachineResult>,
    total: u32,
    succeeded: u32,
    failed: u32,
}

#[derive(Debug, Serialize)]
struct FleetMachineResult {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provision_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct FleetDestroyResult {
    destroyed: u32,
    failed: u32,
    machines: Vec<String>,
}

// --- Fleet Create ---

pub fn run_create(args: FleetCreateArgs, state: &StateStore) -> Result<()> {
    // Read and parse fleet YAML
    let yaml_content = std::fs::read_to_string(&args.file)
        .with_context(|| format!("Failed to read fleet file: {}", args.file))?;
    let fleet: FleetFile = serde_yaml::from_str(&yaml_content)
        .with_context(|| format!("Failed to parse fleet file: {}", args.file))?;

    // Calculate total machine count
    let total: u32 = fleet.machines.iter().map(|d| d.count).sum();
    if total == 0 {
        eprintln!("No machines defined in fleet file.");
        return Ok(());
    }

    // Create provisioner upfront (reused for all machines)
    let provisioner = Provisioner::new()?;

    let mut results = Vec::new();
    let mut succeeded: u32 = 0;
    let mut failed: u32 = 0;
    let mut current: u32 = 0;

    for def in &fleet.machines {
        for index in 0..def.count {
            current += 1;
            let machine_name = if def.count == 1 {
                def.name.clone()
            } else {
                format!("{}-{}", def.name, index)
            };

            // Progress
            eprintln!("[{}/{}] Creating {}...", current, total, machine_name);

            // Create the machine
            let rt = crate::make_runtime(&args.runtime);
            let create_opts = CreateOpts {
                name: Some(machine_name.clone()),
                memory_mb: def.memory,
                cpus: def.cpus,
                network: args.network.clone(),
                timeout: 0,
                runtime: args.runtime.clone(),
            };

            let info = match rt.create(create_opts, state) {
                Ok(info) => {
                    eprintln!("[{}/{}] Created {} ({})", current, total, machine_name, info.id);
                    info
                }
                Err(e) => {
                    eprintln!("[{}/{}] Failed to create {}: {}", current, total, machine_name, e);
                    failed += 1;
                    results.push(FleetMachineResult {
                        name: machine_name,
                        id: None,
                        status: "error".to_string(),
                        ip: None,
                        provision_status: None,
                        error: Some(format!("{e}")),
                    });
                    continue;
                }
            };

            // Set fleet_name in state
            let fleet_group = def.name.clone();
            let machine_id = info.id.clone();
            state.with_lock(|s| {
                if let Some(m) = s.machines.get_mut(&machine_id) {
                    m.fleet_name = Some(fleet_group);
                }
                Ok(())
            })?;

            // Provision if specified
            let provision_status = if let Some(ref provision) = def.provision {
                eprintln!("[{}/{}] Provisioning {} with {}...", current, total, machine_name, provision);

                let start = std::time::Instant::now();
                let prov_result = provisioner.provision(
                    &info.id,
                    &[provision.clone()],
                    120, // default timeout, per-component timeouts override
                    rt.as_ref(),
                    state,
                );

                let elapsed = start.elapsed();
                match prov_result {
                    Ok(result) => {
                        let all_ok = result.results.iter().all(|r| r.status == "ok");
                        if all_ok {
                            eprintln!(
                                "[{}/{}] Provisioned {} ({:.0}s)",
                                current, total, machine_name, elapsed.as_secs_f64()
                            );
                            Some("ok".to_string())
                        } else {
                            let errors: Vec<_> = result.results.iter()
                                .filter(|r| r.status != "ok")
                                .map(|r| format!("{}: {}", r.component, r.error.as_deref().unwrap_or("unknown")))
                                .collect();
                            eprintln!(
                                "[{}/{}] Provision partial failure for {}: {}",
                                current, total, machine_name, errors.join(", ")
                            );
                            Some("partial".to_string())
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "[{}/{}] Provision failed for {}: {}",
                            current, total, machine_name, e
                        );
                        Some("error".to_string())
                    }
                }
            } else {
                None
            };

            succeeded += 1;
            results.push(FleetMachineResult {
                name: machine_name,
                id: Some(info.id),
                status: "running".to_string(),
                ip: info.ip,
                provision_status,
                error: None,
            });
        }
    }

    // Summary
    eprintln!();
    eprintln!(
        "Fleet ready: {} created, {} failed",
        succeeded, failed
    );

    // JSON output
    output::print_json(&FleetCreateResult {
        fleet: results,
        total,
        succeeded,
        failed,
    });

    Ok(())
}

// --- Fleet Destroy ---

pub fn run_destroy(args: FleetDestroyArgs, state: &StateStore) -> Result<()> {
    if !args.all && args.name.is_none() {
        return Err(ClawError::ExecFailed(
            "Either --all or --name must be specified".to_string()
        ).into());
    }

    // Find matching machines
    let targets: Vec<(String, String)> = state.with_read_lock(|s| {
        let mut matches = Vec::new();
        for (id, machine) in &s.machines {
            if let Some(ref fleet_name) = machine.fleet_name {
                if args.all || args.name.as_deref() == Some(fleet_name) {
                    matches.push((id.clone(), machine.runtime.clone()));
                }
            }
        }
        Ok(matches)
    })?;

    if targets.is_empty() {
        eprintln!("No fleet machines found.");
        return Ok(());
    }

    let total = targets.len();
    let mut destroyed: u32 = 0;
    let mut failed: u32 = 0;
    let mut destroyed_ids = Vec::new();

    for (i, (id, runtime_name)) in targets.iter().enumerate() {
        eprintln!("[{}/{}] Destroying {}...", i + 1, total, id);
        let rt = crate::make_runtime(runtime_name);
        match rt.destroy(id, state) {
            Ok(_) => {
                destroyed += 1;
                destroyed_ids.push(id.clone());
            }
            Err(e) => {
                eprintln!("[{}/{}] Failed to destroy {}: {}", i + 1, total, id, e);
                failed += 1;
            }
        }
    }

    eprintln!();
    eprintln!("Fleet destroy: {} destroyed, {} failed", destroyed, failed);

    output::print_json(&FleetDestroyResult {
        destroyed,
        failed,
        machines: destroyed_ids,
    });

    Ok(())
}
