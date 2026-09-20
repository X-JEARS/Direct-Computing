//! Authentication and authorization primitives.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Permissions {
    pub view_desktop: bool,
    pub control_input: bool,
    pub open_terminal: bool,
    pub execute_command: bool,
    pub transfer_files: bool,
    pub ssh_access: bool,
}
