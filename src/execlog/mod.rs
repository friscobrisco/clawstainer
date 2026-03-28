pub mod logger;
pub mod reader;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecLogEntry {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub command: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub timed_out: bool,
}

#[cfg(test)]
mod tests {
    use crate::runtime::ExecResult;

    fn fake_exec_result(exit_code: i32, duration_ms: u64) -> ExecResult {
        ExecResult {
            machine_id: "sb-test".to_string(),
            exit_code,
            stdout: "hello\n".to_string(),
            stderr: String::new(),
            duration_ms,
            timed_out: false,
            truncated: false,
            total_bytes: None,
            peak_memory_bytes: None,
            cpu_time_us: None,
        }
    }

    #[test]
    fn test_append_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();

        // Append some entries
        super::logger::append_to_dir(&dir_path, "sb-test", "echo hello", &fake_exec_result(0, 42)).unwrap();
        super::logger::append_to_dir(&dir_path, "sb-test", "ls -la", &fake_exec_result(0, 15)).unwrap();
        super::logger::append_to_dir(&dir_path, "sb-test", "false", &fake_exec_result(1, 5)).unwrap();

        // Read all
        let entries = super::reader::read_last_from_dir(&dir_path, "sb-test", 100).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].command, "echo hello");
        assert_eq!(entries[0].exit_code, 0);
        assert_eq!(entries[1].command, "ls -la");
        assert_eq!(entries[2].command, "false");
        assert_eq!(entries[2].exit_code, 1);
    }

    #[test]
    fn test_read_last_n() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();

        for i in 0..10 {
            super::logger::append_to_dir(&dir_path, "sb-test", &format!("cmd-{i}"), &fake_exec_result(0, 10)).unwrap();
        }

        let entries = super::reader::read_last_from_dir(&dir_path, "sb-test", 3).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].command, "cmd-7");
        assert_eq!(entries[1].command, "cmd-8");
        assert_eq!(entries[2].command, "cmd-9");
    }

    #[test]
    fn test_read_nonexistent_machine() {
        let dir = tempfile::tempdir().unwrap();
        let entries = super::reader::read_last_from_dir(&dir.path().to_path_buf(), "sb-nope", 10).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_separate_machine_logs() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();

        super::logger::append_to_dir(&dir_path, "sb-aaa", "cmd-a", &fake_exec_result(0, 10)).unwrap();
        super::logger::append_to_dir(&dir_path, "sb-bbb", "cmd-b", &fake_exec_result(0, 20)).unwrap();

        let a = super::reader::read_last_from_dir(&dir_path, "sb-aaa", 10).unwrap();
        let b = super::reader::read_last_from_dir(&dir_path, "sb-bbb", 10).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        assert_eq!(a[0].command, "cmd-a");
        assert_eq!(b[0].command, "cmd-b");
    }
}
