use anyhow::Result;
use std::time::Instant;

use crate::runtime::{ExecOpts, Runtime};
use crate::state::StateStore;

use super::{load_components, resolve_components, ComponentResult, ProvisionResult};

pub struct Provisioner {
    components_file: super::ComponentsFile,
}

impl Provisioner {
    pub fn new() -> Result<Self> {
        Ok(Self {
            components_file: load_components()?,
        })
    }

    pub fn provision(
        &self,
        machine_id: &str,
        component_names: &[String],
        timeout: u64,
        runtime: &dyn Runtime,
        state: &StateStore,
    ) -> Result<ProvisionResult> {
        let resolved = resolve_components(component_names, &self.components_file)?;
        let mut results = Vec::new();

        for name in &resolved {
            let def = self.components_file.components.get(name)
                .ok_or_else(|| anyhow::anyhow!("Unknown component: {name}"))?;
            let start = Instant::now();

            let component_timeout = def.timeout.unwrap_or(timeout);

            // Set environment variables to suppress interactive prompts:
            // - DEBIAN_FRONTEND=noninteractive for apt/dpkg
            // - NONINTERACTIVE=1 for common install scripts
            let mut env = std::collections::HashMap::new();
            env.insert("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string());
            env.insert("NONINTERACTIVE".to_string(), "1".to_string());

            let exec_result = runtime.exec(
                machine_id,
                ExecOpts {
                    command: def.install.clone(),
                    timeout: component_timeout,
                    workdir: "/root".to_string(),
                    env,
                    user: "root".to_string(),
                },
            );

            let duration_ms = start.elapsed().as_millis() as u64;

            match exec_result {
                Ok(r) if r.exit_code == 0 => {
                    // Verify
                    let verify_result = runtime.exec(
                        machine_id,
                        ExecOpts {
                            command: def.verify.clone(),
                            timeout: 30,
                            workdir: "/root".to_string(),
                            env: std::collections::HashMap::new(),
                            user: "root".to_string(),
                        },
                    );

                    match verify_result {
                        Ok(v) if v.exit_code == 0 => {
                            results.push(ComponentResult {
                                component: name.clone(),
                                status: "ok".to_string(),
                                error: None,
                                duration_ms,
                            });
                        }
                        _ => {
                            results.push(ComponentResult {
                                component: name.clone(),
                                status: "error".to_string(),
                                error: Some("verification failed".to_string()),
                                duration_ms,
                            });
                        }
                    }
                }
                Ok(r) => {
                    results.push(ComponentResult {
                        component: name.clone(),
                        status: "error".to_string(),
                        error: Some(format!("install failed (exit code {}): {}", r.exit_code, r.stderr.trim())),
                        duration_ms,
                    });
                }
                Err(e) => {
                    results.push(ComponentResult {
                        component: name.clone(),
                        status: "error".to_string(),
                        error: Some(format!("exec failed: {e}")),
                        duration_ms,
                    });
                }
            }
        }

        // Update state with installed components
        let installed: Vec<String> = results
            .iter()
            .filter(|r| r.status == "ok")
            .map(|r| r.component.clone())
            .collect();

        if !installed.is_empty() {
            state.with_lock(|s| {
                if let Some(machine) = s.machines.get_mut(machine_id) {
                    machine.components.extend(installed);
                }
                Ok(())
            })?;
        }

        Ok(ProvisionResult {
            machine_id: machine_id.to_string(),
            results,
        })
    }
}
