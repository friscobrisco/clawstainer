use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

const BASE_DIR: &str = "/var/lib/clawstainer/base-images";
const IMAGE_NAME: &str = "ubuntu-24.04";

// LXC image server — minimal Ubuntu rootfs tarballs
const ROOTFS_URL_ARM64: &str =
    "https://images.linuxcontainers.org/images/ubuntu/noble/arm64/default";
const ROOTFS_URL_AMD64: &str =
    "https://images.linuxcontainers.org/images/ubuntu/noble/amd64/default";

/// Ensure the base image exists, downloading it if necessary.
/// Returns the path to the base image root filesystem.
pub fn ensure_base_image() -> Result<PathBuf> {
    let image_path = PathBuf::from(BASE_DIR).join(IMAGE_NAME);

    if image_path.exists() && image_path.join("bin").exists() {
        return Ok(image_path);
    }

    // Try downloading a pre-built rootfs first, fall back to debootstrap
    eprintln!("Base image not found. Downloading Ubuntu 24.04 minimal rootfs...");

    std::fs::create_dir_all(&image_path)
        .context("Failed to create base image directory")?;

    match download_rootfs(&image_path) {
        Ok(()) => {
            eprintln!("Download complete. Applying fixups...");
            apply_fixups(&image_path)?;
            eprintln!("Base image ready.");
            Ok(image_path)
        }
        Err(download_err) => {
            eprintln!("Download failed: {download_err:#}");
            eprintln!("Falling back to debootstrap...");
            // Clean up failed download
            let _ = std::fs::remove_dir_all(&image_path);
            std::fs::create_dir_all(&image_path)?;
            debootstrap(&image_path)?;
            apply_fixups(&image_path)?;
            eprintln!("Base image ready.");
            Ok(image_path)
        }
    }
}

/// Download a pre-built rootfs tarball from the LXC image server
fn download_rootfs(image_path: &PathBuf) -> Result<()> {
    let arch = std::env::consts::ARCH;
    let base_url = match arch {
        "aarch64" => ROOTFS_URL_ARM64,
        "x86_64" => ROOTFS_URL_AMD64,
        _ => anyhow::bail!("Unsupported architecture: {arch}"),
    };

    // The LXC image server has date-versioned directories.
    // First, find the latest build by listing the directory.
    // We use a simpler approach: fetch the rootfs.tar.xz from the latest symlink-like path.
    // The server doesn't have a "latest" symlink, so we query the index page to find
    // the most recent build date.

    eprintln!("Fetching image index for {arch}...");

    let index_output = Command::new("curl")
        .args(["-fsSL", base_url])
        .output()
        .context("Failed to fetch image index (is curl installed?)")?;

    if !index_output.status.success() {
        anyhow::bail!("Failed to fetch image index from {base_url}");
    }

    let index_html = String::from_utf8_lossy(&index_output.stdout);

    // Parse out the most recent date directory (format: YYYYMMDD_HH:MM)
    // The HTML contains links like <a href="20260301_07:42/">
    let latest_build = index_html
        .lines()
        .filter_map(|line| {
            let start = line.find("href=\"")? + 6;
            let end = line[start..].find("\"")? + start;
            let href = &line[start..end];
            // Build dirs look like "20260301_07:42/"
            if href.len() > 8 && href.ends_with('/') && href.chars().next()?.is_ascii_digit() {
                Some(href.trim_end_matches('/').to_string())
            } else {
                None
            }
        })
        .max()
        .ok_or_else(|| anyhow::anyhow!("Could not find any builds in image index"))?;

    let rootfs_url = format!("{base_url}/{latest_build}/rootfs.tar.xz");
    eprintln!("Downloading {rootfs_url}...");

    // Download and extract in one pipeline: curl | tar
    let status = Command::new("sh")
        .args([
            "-c",
            &format!(
                "curl -fSL '{}' | xz -d | tar -xf - -C '{}'",
                rootfs_url,
                image_path.display()
            ),
        ])
        .status()
        .context("Failed to download and extract rootfs")?;

    if !status.success() {
        anyhow::bail!("Failed to download/extract rootfs from {rootfs_url}");
    }

    // Verify we got a valid rootfs
    if !image_path.join("bin").exists() {
        anyhow::bail!("Downloaded rootfs appears invalid (no /bin directory)");
    }

    Ok(())
}

/// Fallback: use debootstrap to create the rootfs locally
fn debootstrap(image_path: &PathBuf) -> Result<()> {
    let which = Command::new("which")
        .arg("debootstrap")
        .output()
        .context("Failed to check for debootstrap")?;

    if !which.status.success() {
        anyhow::bail!(
            "Neither image download nor debootstrap succeeded.\n\
             Install debootstrap with: apt-get install -y debootstrap"
        );
    }

    let status = Command::new("debootstrap")
        .args([
            "--variant=minbase",
            "--include=systemd,dbus,iproute2,systemd-resolved",
            "noble",
            image_path.to_str().unwrap(),
        ])
        .status()
        .context("Failed to run debootstrap")?;

    if !status.success() {
        let _ = std::fs::remove_dir_all(image_path);
        anyhow::bail!("debootstrap failed with exit code: {status}");
    }

    Ok(())
}

fn apply_fixups(image_path: &PathBuf) -> Result<()> {
    // Set root password to empty (passwordless login)
    let shadow_path = image_path.join("etc/shadow");
    if shadow_path.exists() {
        let content = std::fs::read_to_string(&shadow_path)?;
        let patched = content.replace("root:*:", "root::");
        std::fs::write(&shadow_path, patched)?;
    }

    // Write resolv.conf pointing to bridge gateway
    let resolv_path = image_path.join("etc/resolv.conf");
    // Remove if it's a symlink (common in Ubuntu)
    if resolv_path.is_symlink() {
        let _ = std::fs::remove_file(&resolv_path);
    }
    std::fs::write(&resolv_path, "nameserver 10.0.0.1\nnameserver 8.8.8.8\n")?;

    // Write a basic systemd-networkd config for the container's host0 interface
    let networkd_dir = image_path.join("etc/systemd/network");
    std::fs::create_dir_all(&networkd_dir)?;
    // This will be overridden per-container with the actual IP at create time,
    // but having a default DHCP config helps if networkd is running
    std::fs::write(
        networkd_dir.join("80-container-host0.network"),
        "[Match]\nName=host0\n\n[Network]\nDHCP=ipv4\n",
    )?;

    // Disable unnecessary services for faster boot
    let disable_services = ["apt-daily.timer", "apt-daily-upgrade.timer"];
    for svc in &disable_services {
        let link_path = image_path
            .join("etc/systemd/system/multi-user.target.wants")
            .join(svc);
        let _ = std::fs::remove_file(link_path);
    }

    Ok(())
}
