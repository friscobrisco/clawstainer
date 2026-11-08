use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

const FC_DIR: &str = "/var/lib/clawstainer/firecracker";
pub const FC_VERSION: &str = "v1.11.0";

fn fc_url(arch: &str) -> String {
    format!(
        "https://github.com/firecracker-microvm/firecracker/releases/download/{FC_VERSION}/firecracker-{FC_VERSION}-{arch}.tgz"
    )
}

/// Paths to the binaries and kernel a Firecracker VM needs.
pub struct Assets {
    pub firecracker: PathBuf,
    pub jailer: PathBuf,
    pub kernel: PathBuf,
}

/// Ensure the Firecracker and jailer binaries and the kernel are available
pub fn ensure_assets() -> Result<Assets> {
    let base = PathBuf::from(FC_DIR);
    let bin_path = base.join("bin").join("firecracker");
    let jailer_path = base.join("bin").join("jailer");
    let kernel_path = base.join("kernels").join("vmlinux");
    let assets = Assets {
        firecracker: bin_path.clone(),
        jailer: jailer_path.clone(),
        kernel: kernel_path.clone(),
    };

    if bin_path.exists() && jailer_path.exists() && kernel_path.exists() {
        return Ok(assets);
    }

    std::fs::create_dir_all(base.join("bin"))?;
    std::fs::create_dir_all(base.join("kernels"))?;

    // Download the Firecracker release (firecracker + jailer)
    if !bin_path.exists() || !jailer_path.exists() {
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
                    "curl -fSL '{url}' | tar xz -C '{bin}' --strip-components=1 && \
                     mv '{bin}/firecracker-{FC_VERSION}-{arch}' '{fc}' && \
                     mv '{bin}/jailer-{FC_VERSION}-{arch}' '{jailer}'",
                    bin = base.join("bin").display(),
                    fc = bin_path.display(),
                    jailer = jailer_path.display(),
                ),
            ])
            .status()
            .context("Failed to download Firecracker")?;

        if !status.success() {
            anyhow::bail!("Failed to download Firecracker binary");
        }

        // Make executable
        Command::new("chmod")
            .args(["+x", bin_path.to_str().unwrap(), jailer_path.to_str().unwrap()])
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

    Ok(assets)
}
