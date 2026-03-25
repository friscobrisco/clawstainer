use anyhow::Result;

use crate::cli::ProvisionArgs;
use crate::component::Provisioner;
use crate::error::ClawError;
use crate::output;
use crate::runtime::Runtime;
use crate::state::StateStore;

pub fn run(args: ProvisionArgs, runtime: &dyn Runtime, state: &StateStore) -> Result<()> {
    let component_names: Vec<String> = if let Some(ref components) = args.components {
        components.split(',').map(|s| s.trim().to_string()).collect()
    } else if let Some(ref file) = args.file {
        let content = std::fs::read_to_string(file)?;
        let list: Vec<String> = serde_yaml::from_str(&content)?;
        list
    } else {
        return Err(ClawError::ProvisionFailed(
            "Either --components or --file must be specified".to_string()
        ).into());
    };

    let provisioner = Provisioner::new()?;
    let results = provisioner.provision(&args.machine_id, &component_names, args.timeout, runtime, state)?;
    output::print_json(&results);
    Ok(())
}
