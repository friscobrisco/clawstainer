use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClawError {
    #[error("No machine with ID '{0}'")]
    MachineNotFound(String),

    #[error("Machine '{0}' is not running (status: {1})")]
    MachineNotRunning(String, String),

    #[error("Failed to create sandbox: {0}")]
    CreateFailed(String),

    #[error("Command timed out after {0}s")]
    ExecTimeout(u64),

    #[error("Failed to execute command: {0}")]
    ExecFailed(String),

    #[error("Provisioning failed: {0}")]
    ProvisionFailed(String),

    #[error("clawstainer runtime requires Linux (current OS: {0})")]
    RuntimeUnavailable(String),

    #[error("Host resources exhausted: {0}")]
    ResourceLimit(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),
}

impl ClawError {
    /// Error code string for JSON output
    pub fn code(&self) -> &'static str {
        match self {
            Self::MachineNotFound(_) => "machine_not_found",
            Self::MachineNotRunning(_, _) => "machine_not_running",
            Self::CreateFailed(_) => "create_failed",
            Self::ExecTimeout(_) => "exec_timeout",
            Self::ExecFailed(_) => "exec_failed",
            Self::ProvisionFailed(_) => "provision_failed",
            Self::RuntimeUnavailable(_) => "runtime_unavailable",
            Self::ResourceLimit(_) => "resource_limit",
            Self::PermissionDenied(_) => "permission_denied",
        }
    }

    /// Numeric exit code
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::MachineNotFound(_) => 2,
            Self::MachineNotRunning(_, _) => 3,
            Self::CreateFailed(_) => 4,
            Self::ExecTimeout(_) => 5,
            Self::ExecFailed(_) => 6,
            Self::ProvisionFailed(_) => 7,
            Self::RuntimeUnavailable(_) => 8,
            Self::ResourceLimit(_) => 9,
            Self::PermissionDenied(_) => 10,
        }
    }

    /// Helpful hint for the user
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::MachineNotFound(_) => Some("Run 'clawstainer list' to see active machines"),
            Self::MachineNotRunning(_, _) => Some("The machine may have been stopped or destroyed"),
            Self::RuntimeUnavailable(_) => Some("clawstainer requires Linux with systemd-nspawn"),
            Self::ExecTimeout(_) => Some("Use --timeout to increase the limit"),
            Self::ProvisionFailed(_) => Some("Check component names with 'clawstainer provision --help'"),
            _ => None,
        }
    }
}
