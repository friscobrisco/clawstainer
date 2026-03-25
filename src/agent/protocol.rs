use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    #[serde(rename = "exec")]
    Exec {
        command: String,
        #[serde(default = "default_timeout")]
        timeout: u64,
        #[serde(default = "default_workdir")]
        workdir: String,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default = "default_user")]
        user: String,
    },

    #[serde(rename = "health")]
    Health,

    #[serde(rename = "shutdown")]
    Shutdown,
}

fn default_timeout() -> u64 {
    30
}

fn default_workdir() -> String {
    "/root".to_string()
}

fn default_user() -> String {
    "root".to_string()
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum Response {
    #[serde(rename = "exec_result")]
    ExecResult {
        exit_code: i32,
        stdout: String,
        stderr: String,
        duration_ms: u64,
        timed_out: bool,
    },

    #[serde(rename = "health_result")]
    HealthResult { status: String },

    #[serde(rename = "error")]
    Error { message: String },

    #[serde(rename = "ok")]
    Ok,
}
