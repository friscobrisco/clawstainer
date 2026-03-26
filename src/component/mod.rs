pub mod provisioner;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub use provisioner::Provisioner;

const DEFAULT_COMPONENTS_YAML: &str = include_str!("../../components.yaml");

#[derive(Debug, Deserialize)]
pub struct ComponentsFile {
    pub components: HashMap<String, ComponentDef>,
    #[serde(default)]
    pub bundles: HashMap<String, BundleDef>,
}

#[derive(Debug, Deserialize)]
pub struct ComponentDef {
    pub install: String,
    pub verify: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct BundleDef {
    pub components: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ProvisionResult {
    pub machine_id: String,
    pub results: Vec<ComponentResult>,
}

#[derive(Debug, Serialize)]
pub struct ComponentResult {
    pub component: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: u64,
}

/// Load components from the default embedded YAML
pub fn load_components() -> Result<ComponentsFile> {
    serde_yaml::from_str(DEFAULT_COMPONENTS_YAML)
        .context("Failed to parse components.yaml")
}

/// Resolve component names, expanding bundles into individual components
pub fn resolve_components(names: &[String], file: &ComponentsFile) -> Result<Vec<String>> {
    let mut resolved = Vec::new();

    for name in names {
        if let Some(bundle) = file.bundles.get(name) {
            for component in &bundle.components {
                if !resolved.contains(component) {
                    resolved.push(component.clone());
                }
            }
        } else if file.components.contains_key(name) {
            if !resolved.contains(name) {
                resolved.push(name.clone());
            }
        } else {
            return Err(crate::error::ClawError::ProvisionFailed(
                format!("Unknown component or bundle: {name}")
            ).into());
        }
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_default_components() {
        let file = load_components().unwrap();
        assert!(file.components.contains_key("python3"));
        assert!(file.components.contains_key("nodejs"));
        assert!(file.components.contains_key("git"));
        assert!(file.components.contains_key("curl"));
        assert!(file.components.contains_key("jq"));
        assert!(file.components.contains_key("ripgrep"));
        assert!(file.components.contains_key("build-essential"));
        assert!(file.components.contains_key("docker-cli"));
    }

    #[test]
    fn test_components_have_install_and_verify() {
        let file = load_components().unwrap();
        for (name, def) in &file.components {
            assert!(!def.install.is_empty(), "{name} has empty install command");
            assert!(!def.verify.is_empty(), "{name} has empty verify command");
        }
    }

    #[test]
    fn test_bundles_exist() {
        let file = load_components().unwrap();
        assert!(file.bundles.contains_key("agent-default"));
        assert!(file.bundles.contains_key("web-dev"));
        assert!(file.bundles.contains_key("ml"));
    }

    #[test]
    fn test_bundle_components_are_valid() {
        let file = load_components().unwrap();
        for (bundle_name, bundle) in &file.bundles {
            for component in &bundle.components {
                assert!(
                    file.components.contains_key(component),
                    "Bundle '{bundle_name}' references unknown component '{component}'"
                );
            }
        }
    }

    #[test]
    fn test_resolve_single_component() {
        let file = load_components().unwrap();
        let resolved = resolve_components(&["git".to_string()], &file).unwrap();
        assert_eq!(resolved, vec!["git"]);
    }

    #[test]
    fn test_resolve_multiple_components() {
        let file = load_components().unwrap();
        let resolved = resolve_components(
            &["git".to_string(), "curl".to_string()],
            &file,
        ).unwrap();
        assert_eq!(resolved, vec!["git", "curl"]);
    }

    #[test]
    fn test_resolve_bundle() {
        let file = load_components().unwrap();
        let resolved = resolve_components(&["agent-default".to_string()], &file).unwrap();
        assert_eq!(resolved, vec!["python3", "nodejs", "git", "curl", "jq", "ripgrep"]);
    }

    #[test]
    fn test_resolve_deduplicates() {
        let file = load_components().unwrap();
        let resolved = resolve_components(
            &["git".to_string(), "agent-default".to_string()],
            &file,
        ).unwrap();
        // git should appear only once (it's in both the direct list and the bundle)
        assert_eq!(resolved.iter().filter(|c| *c == "git").count(), 1);
    }

    #[test]
    fn test_resolve_unknown_component_fails() {
        let file = load_components().unwrap();
        let result = resolve_components(&["nonexistent".to_string()], &file);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nonexistent"));
    }
}
