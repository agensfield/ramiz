use serde::Serialize;

use crate::output;

#[derive(Debug, Serialize)]
pub struct ErrorData {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_created: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_output: Option<HookOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct HookOutput {
    pub encoding: &'static str,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub struct RamizError {
    data: Box<ErrorData>,
    json: bool,
    exit_code: u8,
}

impl RamizError {
    pub fn new(code: &'static str, message: impl Into<String>, json: bool) -> Self {
        Self {
            data: Box::new(ErrorData {
                code,
                message: message.into(),
                worktree_created: None,
                cleanup: None,
                hook_output: None,
                update: None,
            }),
            json,
            exit_code: 1,
        }
    }

    pub fn worktree_created(mut self) -> Self {
        self.data.worktree_created = Some(true);
        self
    }

    pub fn with_cleanup(mut self, cleanup: impl Into<String>) -> Self {
        self.data.cleanup = Some(cleanup.into());
        self
    }

    pub fn with_hook_output(mut self, stdout: String, stderr: String) -> Self {
        self.data.hook_output = Some(HookOutput {
            encoding: "base64",
            stdout,
            stderr,
        });
        self
    }

    pub fn with_update_details(mut self, details: serde_json::Value) -> Self {
        self.data.update = Some(details);
        self
    }

    pub fn exit_code(&self) -> u8 {
        self.exit_code
    }

    pub fn emit(&self, command: &'static str) {
        if self.json {
            output::failure(command, &self.data);
        } else {
            eprintln!("ramiz: {}", self.data.message);
            if let Some(cleanup) = &self.data.cleanup {
                eprintln!("cleanup: {cleanup}");
            }
            if let Some(update) = &self.data.update {
                if let Some(rollback) = update.get("rollback").and_then(|value| value.as_str()) {
                    eprintln!("rollback: {rollback}");
                }
                if let Some(cleanup) = update.get("cleanup").and_then(|value| value.as_str()) {
                    eprintln!("cleanup: {cleanup}");
                }
            }
        }
    }
}

impl From<std::io::Error> for RamizError {
    fn from(error: std::io::Error) -> Self {
        Self::new("io_error", error.to_string(), false)
    }
}
