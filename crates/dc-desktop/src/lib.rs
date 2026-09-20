//! Desktop capture and input-control orchestration.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameSize {
    pub width: u32,
    pub height: u32,
}
