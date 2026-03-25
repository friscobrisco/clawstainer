use std::collections::HashMap;
use std::process::Command;
use std::time::Instant;

use super::protocol::Response;

const MAX_OUTPUT_BYTES: usize = 1_048_576; // 1MB

pub fn exec(
    command: String,
    timeout: u64,
    workdir: String,
    env: HashMap<String, String>,
    user: String,
) -> Response {
    let start = Instant::now();

    let home = if user == "root" {
        "/root".to_string()
    } else {
        format!("/home/{user}")
    };

    // Build the command
    let mut cmd = if user == "root" {
        let mut c = Command::new("sh");
        c.args(["-c", &command]);
        c
    } else {
        let mut c = Command::new("su");
        c.args(["-", &user, "-c", &command]);
        c
    };

    cmd.current_dir(&workdir);

    // Set default environment
    cmd.env("HOME", &home);
    cmd.env("USER", &user);
    cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    cmd.env("LANG", "C.UTF-8");
    cmd.env("TERM", "xterm-256color");

    // Set custom environment
    for (k, v) in &env {
        cmd.env(k, v);
    }

    // Run with timeout
    let output = if timeout > 0 {
        // Use timeout command for simplicity
        let mut timeout_cmd = Command::new("timeout");
        timeout_cmd.arg(format!("{timeout}s"));

        if user == "root" {
            timeout_cmd.args(["sh", "-c", &command]);
        } else {
            timeout_cmd.args(["su", "-", &user, "-c", &command]);
        }

        timeout_cmd.current_dir(&workdir);
        timeout_cmd.env("HOME", &home);
        timeout_cmd.env("USER", &user);
        timeout_cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        timeout_cmd.env("LANG", "C.UTF-8");
        timeout_cmd.env("TERM", "xterm-256color");
        for (k, v) in &env {
            timeout_cmd.env(k, v);
        }

        timeout_cmd.output()
    } else {
        cmd.output()
    };

    let duration_ms = start.elapsed().as_millis() as u64;

    match output {
        Ok(output) => {
            let exit_code = output.status.code().unwrap_or(-1);
            let timed_out = exit_code == 124; // timeout command returns 124

            let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if stdout.len() > MAX_OUTPUT_BYTES {
                stdout.truncate(MAX_OUTPUT_BYTES);
            }
            if stderr.len() > MAX_OUTPUT_BYTES {
                stderr.truncate(MAX_OUTPUT_BYTES);
            }

            Response::ExecResult {
                exit_code,
                stdout,
                stderr,
                duration_ms,
                timed_out,
            }
        }
        Err(e) => Response::Error {
            message: format!("Failed to execute command: {e}"),
        },
    }
}
