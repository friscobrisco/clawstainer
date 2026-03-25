use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

const FC_DIR: &str = "/var/lib/clawstainer/firecracker";
const FC_VERSION: &str = "v1.11.0";

fn fc_url(arch: &str) -> String {
    format!(
        "https://github.com/firecracker-microvm/firecracker/releases/download/{FC_VERSION}/firecracker-{FC_VERSION}-{arch}.tgz"
    )
}

/// Ensure the Firecracker binary and kernel are available
pub fn ensure_assets() -> Result<(PathBuf, PathBuf)> {
    let base = PathBuf::from(FC_DIR);
    let bin_path = base.join("bin").join("firecracker");
    let kernel_path = base.join("kernels").join("vmlinux");

    if bin_path.exists() && kernel_path.exists() {
        return Ok((bin_path, kernel_path));
    }

    std::fs::create_dir_all(base.join("bin"))?;
    std::fs::create_dir_all(base.join("kernels"))?;

    // Download Firecracker binary
    if !bin_path.exists() {
        let arch = match std::env::consts::ARCH {
            "aarch64" => "aarch64",
            "x86_64" => "x86_64",
            other => anyhow::bail!("Unsupported architecture for Firecracker: {other}"),
        };

        eprintln!("Downloading Firecracker {FC_VERSION} for {arch}...");

        let url = fc_url(arch);
        let status = Command::new("sh")
            .args([
                "-c",
                &format!(
                    "curl -fSL '{}' | tar xz -C '{}' --strip-components=1 && \
                     mv '{}/firecracker-{}-{}' '{}'",
                    url,
                    base.join("bin").display(),
                    base.join("bin").display(),
                    FC_VERSION,
                    arch,
                    bin_path.display(),
                ),
            ])
            .status()
            .context("Failed to download Firecracker")?;

        if !status.success() {
            anyhow::bail!("Failed to download Firecracker binary");
        }

        // Make executable
        Command::new("chmod")
            .args(["+x", bin_path.to_str().unwrap()])
            .status()?;

        eprintln!("Firecracker binary ready.");
    }

    // Download kernel
    if !kernel_path.exists() {
        let arch = match std::env::consts::ARCH {
            "aarch64" => "aarch64",
            "x86_64" => "x86_64",
            other => anyhow::bail!("Unsupported architecture: {other}"),
        };

        eprintln!("Downloading Firecracker kernel...");

        // Use the kernel from Firecracker's CI artifacts
        let kernel_url = format!(
            "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.11/{arch}/vmlinux-6.1.102"
        );

        let status = Command::new("curl")
            .args(["-fSL", "-o", kernel_path.to_str().unwrap(), &kernel_url])
            .status()
            .context("Failed to download kernel")?;

        if !status.success() {
            anyhow::bail!("Failed to download Firecracker kernel");
        }

        eprintln!("Kernel ready.");
    }

    Ok((bin_path, kernel_path))
}
