//! Non-interactive command execution types.

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}
