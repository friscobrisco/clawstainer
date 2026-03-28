use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::thread;

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
    /// Security profile: "strict" (default) or "standard"
    #[serde(default = "default_security")]
    security: String,
    /// Capabilities to add back on top of the security profile
    #[serde(default)]
    cap_add: Vec<String>,
    /// Capabilities to drop on top of the security profile
    #[serde(default)]
    cap_drop: Vec<String>,
    /// Path to a .env file to inject into the sandbox
    #[serde(default)]
    env_file: Option<String>,
}

fn default_count() -> u32 { 1 }
fn default_memory() -> u32 { 512 }
fn default_cpus() -> u32 { 1 }
fn default_security() -> String { "strict".to_string() }

// --- Result types ---

#[derive(Debug, Serialize)]
struct FleetCreateResult {
    fleet: Vec<FleetMachineResult>,
    total: u32,
    created: u32,
    provisioned: u32,
    failed: u32,
}

#[derive(Debug, Serialize, Clone)]
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

// Info needed for provisioning pass
struct PendingProvision {
    machine_name: String,
    machine_id: String,
    provision: String,
    runtime_name: String,
    result_index: usize,
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

    let mut results: Vec<FleetMachineResult> = Vec::new();
    let mut pending_provisions: Vec<PendingProvision> = Vec::new();
    let mut created: u32 = 0;
    let mut failed: u32 = 0;
    let mut current: u32 = 0;

    // ── Pass 1: Create all machines ──
    eprintln!("=== Creating {} machines ===", total);
    eprintln!();

    for def in &fleet.machines {
        for index in 0..def.count {
            current += 1;
            let machine_name = if def.count == 1 {
                def.name.clone()
            } else {
                format!("{}-{}", def.name, index)
            };

            eprintln!("[{}/{}] Creating {}...", current, total, machine_name);

            let rt = crate::make_runtime(&args.runtime);
            let create_opts = CreateOpts {
                name: Some(machine_name.clone()),
                memory_mb: def.memory,
                cpus: def.cpus,
                network: args.network.clone(),
                timeout: 0,
                runtime: args.runtime.clone(),
                security: def.security.clone(),
                cap_add: def.cap_add.clone(),
                cap_drop: def.cap_drop.clone(),
                env_file: def.env_file.clone(),
                from_snapshot: None,
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

            created += 1;
            let result_index = results.len();
            results.push(FleetMachineResult {
                name: machine_name.clone(),
                id: Some(info.id.clone()),
                status: "running".to_string(),
                ip: info.ip,
                provision_status: None,
                error: None,
            });

            // Queue for provisioning
            if let Some(ref provision) = def.provision {
                pending_provisions.push(PendingProvision {
                    machine_name,
                    machine_id: info.id,
                    provision: provision.clone(),
                    runtime_name: args.runtime.clone(),
                    result_index,
                });
            }
        }
    }

    // ── Pass 2: Provision machines ──
    if pending_provisions.is_empty() {
        eprintln!();
        eprintln!("Fleet ready: {} created, {} failed, no provisioning needed", created, failed);
        output::print_json(&FleetCreateResult {
            fleet: results,
            total,
            created,
            provisioned: 0,
            failed,
        });
        return Ok(());
    }

    let prov_total = pending_provisions.len();
    let parallel = if args.parallel == 0 { 1 } else { args.parallel };

    eprintln!();
    eprintln!("=== Provisioning {} machines (parallel: {}) ===", prov_total, parallel);
    eprintln!();

    // Shared state for parallel provisioning
    let results = Arc::new(Mutex::new(results));
    let provisioned_count = Arc::new(Mutex::new(0u32));
    let prov_failed_count = Arc::new(Mutex::new(0u32));

    // Process in chunks of `parallel`
    for chunk in pending_provisions.chunks(parallel) {
        let mut handles = Vec::new();

        for pending in chunk {
            let machine_name = pending.machine_name.clone();
            let machine_id = pending.machine_id.clone();
            let provision = pending.provision.clone();
            let runtime_name = pending.runtime_name.clone();
            let result_index = pending.result_index;
            let results = Arc::clone(&results);
            let provisioned_count = Arc::clone(&provisioned_count);
            let prov_failed_count = Arc::clone(&prov_failed_count);

            let handle = thread::spawn(move || {
                eprintln!("  Provisioning {} with {}...", machine_name, provision);

                let provisioner = match Provisioner::new() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("  Failed to create provisioner for {}: {}", machine_name, e);
                        let mut results = results.lock().unwrap();
                        results[result_index].provision_status = Some("error".to_string());
                        results[result_index].error = Some(format!("{e}"));
                        *prov_failed_count.lock().unwrap() += 1;
                        return;
                    }
                };

                let rt = crate::make_runtime(&runtime_name);

                // We need a StateStore for provisioning — create a fresh one
                let prov_state = match StateStore::new() {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("  Failed to open state for {}: {}", machine_name, e);
                        let mut results = results.lock().unwrap();
                        results[result_index].provision_status = Some("error".to_string());
                        results[result_index].error = Some(format!("{e}"));
                        *prov_failed_count.lock().unwrap() += 1;
                        return;
                    }
                };

                let start = std::time::Instant::now();
                let prov_result = provisioner.provision(
                    &machine_id,
                    &[provision.clone()],
                    120,
                    rt.as_ref(),
                    &prov_state,
                );

                let elapsed = start.elapsed();
                let mut results = results.lock().unwrap();

                match prov_result {
                    Ok(result) => {
                        let all_ok = result.results.iter().all(|r| r.status == "ok");
                        if all_ok {
                            eprintln!(
                                "  Provisioned {} ({:.0}s)",
                                machine_name, elapsed.as_secs_f64()
                            );
                            results[result_index].provision_status = Some("ok".to_string());
                            *provisioned_count.lock().unwrap() += 1;
                        } else {
                            let errors: Vec<_> = result.results.iter()
                                .filter(|r| r.status != "ok")
                                .map(|r| format!("{}: {}", r.component, r.error.as_deref().unwrap_or("unknown")))
                                .collect();
                            eprintln!(
                                "  Provision partial for {}: {}",
                                machine_name, errors.join(", ")
                            );
                            results[result_index].provision_status = Some("partial".to_string());
                            *provisioned_count.lock().unwrap() += 1;
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "  Provision failed for {}: {}",
                            machine_name, e
                        );
                        results[result_index].provision_status = Some("error".to_string());
                        results[result_index].error = Some(format!("{e}"));
                        *prov_failed_count.lock().unwrap() += 1;
                    }
                }
            });

            handles.push(handle);
        }

        // Wait for this chunk to finish before starting next
        for handle in handles {
            let _ = handle.join();
        }
    }

    let results = Arc::try_unwrap(results).unwrap().into_inner().unwrap();
    let provisioned = *provisioned_count.lock().unwrap();
    let prov_failed = *prov_failed_count.lock().unwrap();

    // Summary
    eprintln!();
    eprintln!(
        "Fleet ready: {} created, {} provisioned, {} failed",
        created, provisioned, failed + prov_failed
    );

    output::print_json(&FleetCreateResult {
        fleet: results,
        total,
        created,
        provisioned,
        failed: failed + prov_failed,
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
