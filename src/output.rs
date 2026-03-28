use serde::Serialize;
use std::io::{self, IsTerminal, Write};

/// Print a JSON value to stdout
pub fn print_json(value: &impl Serialize) {
    let json = serde_json::to_string_pretty(value).expect("failed to serialize JSON");
    println!("{json}");
}

/// Resolve the output format: if the user passed "auto" (the default),
/// use "table" when stdout is a TTY, "json" otherwise (piped/scripted/agent).
pub fn resolve_format(format: &str) -> &str {
    match format {
        "auto" => {
            if io::stdout().is_terminal() {
                "table"
            } else {
                "json"
            }
        }
        other => other,
    }
}

/// Structured CLI error returned as JSON to stderr
#[derive(Debug, Serialize)]
pub struct CliError {
    pub error: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl CliError {
    pub fn new(error: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Serialize to JSON string (for testing or custom output)
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("failed to serialize error")
    }

    /// Print the error as JSON to stderr and exit with a numeric code
    pub fn exit(self) -> ! {
        let code = exit_code_for(&self.error);
        let json = self.to_json();
        let _ = writeln!(io::stderr(), "{json}");
        std::process::exit(code);
    }
}

/// Map error codes to distinct numeric exit codes
///
/// 0  = success
/// 1  = general error
/// 2  = machine not found
/// 3  = machine not running
/// 4  = create failed
/// 5  = exec timeout
/// 6  = exec failed
/// 7  = provision failed
/// 8  = runtime unavailable
/// 9  = resource limit
/// 10 = permission denied
fn exit_code_for(error: &str) -> i32 {
    match error {
        "machine_not_found" => 2,
        "machine_not_running" => 3,
        "create_failed" => 4,
        "exec_timeout" => 5,
        "exec_failed" => 6,
        "provision_failed" => 7,
        "runtime_unavailable" => 8,
        "resource_limit" => 9,
        "permission_denied" => 10,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_error_json_format() {
        let err = CliError::new("machine_not_found", "No machine with ID 'sb-xyz' exists");
        let json: serde_json::Value = serde_json::from_str(&err.to_json()).unwrap();

        assert_eq!(json["error"], "machine_not_found");
        assert_eq!(json["message"], "No machine with ID 'sb-xyz' exists");
        assert!(json.get("hint").is_none()); // hint is None, should be skipped
    }

    #[test]
    fn test_cli_error_with_hint() {
        let err = CliError::new("machine_not_found", "No machine with ID 'sb-xyz' exists")
            .with_hint("Run 'clawstainer list' to see active machines");
        let json: serde_json::Value = serde_json::from_str(&err.to_json()).unwrap();

        assert_eq!(json["error"], "machine_not_found");
        assert_eq!(json["hint"], "Run 'clawstainer list' to see active machines");
    }

    #[test]
    fn test_exec_result_json_format() {
        use crate::runtime::ExecResult;

        let result = ExecResult {
            machine_id: "sb-a1b2c3d4".to_string(),
            exit_code: 0,
            stdout: "Hello, world!\n".to_string(),
            stderr: String::new(),
            duration_ms: 42,
            timed_out: false,
            truncated: false,
            total_bytes: None,
            peak_memory_bytes: None,
            cpu_time_us: None,
        };

        let json_str = serde_json::to_string_pretty(&result).unwrap();
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert_eq!(json["machine_id"], "sb-a1b2c3d4");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["stdout"], "Hello, world!\n");
        assert_eq!(json["duration_ms"], 42);
        assert_eq!(json["timed_out"], false);
        // truncated=false should be skipped, total_bytes=None should be skipped
        assert!(json.get("truncated").is_none());
        assert!(json.get("total_bytes").is_none());
    }

    #[test]
    fn test_machine_info_json_format() {
        use crate::runtime::MachineInfo;

        let info = MachineInfo {
            id: "sb-a1b2c3d4".to_string(),
            name: "bold-parrot".to_string(),
            status: "running".to_string(),
            ip: Some("10.0.0.2".to_string()),
            created_at: "2026-03-24T10:30:00Z".to_string(),
        };

        let json_str = serde_json::to_string_pretty(&info).unwrap();
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert_eq!(json["id"], "sb-a1b2c3d4");
        assert_eq!(json["name"], "bold-parrot");
        assert_eq!(json["status"], "running");
        assert_eq!(json["ip"], "10.0.0.2");
    }
}
