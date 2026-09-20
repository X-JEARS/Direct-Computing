//! Chunked file-transfer primitives.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChunkDescriptor {
    pub offset: u64,
    pub length: u32,
}
