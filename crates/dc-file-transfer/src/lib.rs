//! Chunked file-transfer primitives.

use dc_common::{DcError, Result};
use dc_protocol::WireMessage;
use dc_transport::FramedStream;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

pub const DEFAULT_CHUNK_SIZE: u32 = 256 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChunkDescriptor {
    pub offset: u64,
    pub length: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileManifest {
    pub name: String,
    pub size: u64,
    pub chunk_size: u32,
    pub sha256: [u8; 32],
}

impl FileManifest {
    pub fn from_bytes(name: impl Into<String>, bytes: &[u8], chunk_size: u32) -> Result<Self> {
        validate_chunk_size(chunk_size)?;
        Ok(Self {
            name: name.into(),
            size: bytes.len() as u64,
            chunk_size,
            sha256: sha256(bytes),
        })
    }

    pub fn from_file(path: &Path, chunk_size: u32) -> Result<Self> {
        validate_chunk_size(chunk_size)?;
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| DcError::InvalidInput("file name is not valid UTF-8".into()))?;
        let mut file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; chunk_size as usize];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(Self {
            name: name.to_owned(),
            size,
            chunk_size,
            sha256: hasher.finalize().into(),
        })
    }

    pub fn chunks(&self) -> Vec<ChunkDescriptor> {
        let mut chunks = Vec::new();
        let mut offset = 0_u64;
        while offset < self.size {
            let remaining = self.size - offset;
            let length = remaining.min(u64::from(self.chunk_size)) as u32;
            chunks.push(ChunkDescriptor { offset, length });
            offset += u64::from(length);
        }
        chunks
    }

    pub fn to_wire(&self, transfer_id: [u8; 16]) -> WireMessage {
        WireMessage::FileOffer {
            transfer_id,
            name: self.name.clone(),
            size: self.size,
            chunk_size: self.chunk_size,
            sha256: self.sha256,
        }
    }

    pub fn from_wire(message: WireMessage) -> Result<(FileManifest, [u8; 16])> {
        let WireMessage::FileOffer {
            transfer_id,
            name,
            size,
            chunk_size,
            sha256,
        } = message
        else {
            return Err(DcError::InvalidInput("expected file offer message".into()));
        };
        validate_chunk_size(chunk_size)?;
        Ok((
            FileManifest {
                name,
                size,
                chunk_size,
                sha256,
            },
            transfer_id,
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChunk {
    pub offset: u64,
    pub data: Vec<u8>,
    pub sha256: [u8; 32],
}

impl FileChunk {
    pub fn new(offset: u64, data: Vec<u8>) -> Self {
        let sha256 = sha256(&data);
        Self {
            offset,
            data,
            sha256,
        }
    }

    pub fn verify(&self) -> bool {
        constant_time_equal(&self.sha256, &sha256(&self.data))
    }

    pub fn to_wire(&self, transfer_id: [u8; 16]) -> WireMessage {
        WireMessage::FileChunk {
            transfer_id,
            offset: self.offset,
            data: self.data.clone(),
            sha256: self.sha256,
        }
    }

    pub fn from_wire(message: WireMessage) -> Result<(Self, [u8; 16])> {
        let WireMessage::FileChunk {
            transfer_id,
            offset,
            data,
            sha256,
        } = message
        else {
            return Err(DcError::InvalidInput("expected file chunk message".into()));
        };
        Ok((
            Self {
                offset,
                data,
                sha256,
            },
            transfer_id,
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResumeState {
    pub next_offset: u64,
}

impl ResumeState {
    pub const fn new(next_offset: u64) -> Self {
        Self { next_offset }
    }

    pub fn validate(self, manifest: &FileManifest) -> Result<()> {
        validate_chunk_size(manifest.chunk_size)?;
        if self.next_offset > manifest.size
            || self.next_offset % u64::from(manifest.chunk_size) != 0
                && self.next_offset != manifest.size
        {
            return Err(DcError::InvalidInput(
                "resume offset is not a valid chunk boundary".into(),
            ));
        }
        Ok(())
    }
}

pub fn chunk_bytes(bytes: &[u8], chunk_size: u32) -> Result<Vec<FileChunk>> {
    validate_chunk_size(chunk_size)?;
    Ok(bytes
        .chunks(chunk_size as usize)
        .enumerate()
        .map(|(index, chunk)| FileChunk::new(index as u64 * u64::from(chunk_size), chunk.to_vec()))
        .collect())
}

pub fn read_chunk(path: &Path, descriptor: ChunkDescriptor) -> Result<FileChunk> {
    if descriptor.length == 0 {
        return Err(DcError::InvalidInput(
            "file chunk length must be non-zero".into(),
        ));
    }
    let mut file = std::fs::File::open(path)?;
    let file_size = file.metadata()?.len();
    let end = descriptor
        .offset
        .checked_add(u64::from(descriptor.length))
        .ok_or_else(|| DcError::InvalidInput("file chunk range overflows u64".into()))?;
    if end > file_size {
        return Err(DcError::InvalidInput("file chunk exceeds file size".into()));
    }
    file.seek(SeekFrom::Start(descriptor.offset))?;
    let mut data = vec![0_u8; descriptor.length as usize];
    file.read_exact(&mut data)?;
    Ok(FileChunk::new(descriptor.offset, data))
}

/// Send a file over a dedicated authenticated QUIC stream. The receiver sends an initial
/// `FileAck`, allowing an interrupted transfer to resume from its existing `.part` file.
pub async fn send_file(
    stream: &mut FramedStream,
    path: &Path,
    transfer_id: [u8; 16],
    chunk_size: u32,
) -> Result<FileManifest> {
    let manifest = FileManifest::from_file(path, chunk_size)?;
    stream.send(&manifest.to_wire(transfer_id)).await?;
    let resume = receive_ack(stream, transfer_id).await?;
    resume.validate(&manifest)?;
    for descriptor in manifest
        .chunks()
        .into_iter()
        .filter(|chunk| chunk.offset >= resume.next_offset)
    {
        let chunk = read_chunk(path, descriptor)?;
        stream.send(&chunk.to_wire(transfer_id)).await?;
        let acknowledged = receive_ack(stream, transfer_id).await?;
        if acknowledged.next_offset != descriptor.offset + u64::from(descriptor.length) {
            return Err(DcError::Codec(
                "receiver acknowledged an unexpected file offset".into(),
            ));
        }
    }
    Ok(manifest)
}

/// Receive a file into `root`, writing to a hidden temporary file until its manifest checksum
/// matches. The final rename is atomic on the same filesystem, so consumers never observe a
/// partially-written destination file.
pub async fn receive_file(stream: &mut FramedStream, root: &Path) -> Result<PathBuf> {
    let (manifest, transfer_id) = FileManifest::from_wire(stream.receive().await?)?;
    let destination = safe_join(root, Path::new(&manifest.name))?;
    std::fs::create_dir_all(root)?;
    let temporary = root.join(format!(
        ".direct-computing-{}.part",
        transfer_id_hex(transfer_id)
    ));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&temporary)?;
    let existing = file.metadata()?.len();
    let resume = ResumeState::new(existing);
    resume.validate(&manifest)?;
    stream
        .send(&WireMessage::FileAck {
            transfer_id,
            next_offset: existing,
        })
        .await?;
    let mut next_offset = existing;
    while next_offset < manifest.size {
        let (chunk, chunk_id) = FileChunk::from_wire(stream.receive().await?)?;
        if chunk_id != transfer_id
            || chunk.offset != next_offset
            || !chunk.verify()
            || chunk.data.len() > manifest.chunk_size as usize
        {
            return Err(DcError::Codec("invalid file chunk received".into()));
        }
        let end = next_offset
            .checked_add(chunk.data.len() as u64)
            .ok_or_else(|| DcError::Codec("file offset overflow".into()))?;
        if end > manifest.size {
            return Err(DcError::Codec("file chunk exceeds manifest size".into()));
        }
        file.seek(SeekFrom::Start(next_offset))?;
        file.write_all(&chunk.data)?;
        file.flush()?;
        next_offset = end;
        stream
            .send(&WireMessage::FileAck {
                transfer_id,
                next_offset,
            })
            .await?;
    }
    file.sync_all()?;
    let completed = FileManifest::from_file(&temporary, manifest.chunk_size)?;
    if completed.size != manifest.size || completed.sha256 != manifest.sha256 {
        return Err(DcError::Codec(
            "received file does not match manifest checksum".into(),
        ));
    }
    std::fs::rename(&temporary, &destination)?;
    Ok(destination)
}

async fn receive_ack(stream: &mut FramedStream, transfer_id: [u8; 16]) -> Result<ResumeState> {
    let WireMessage::FileAck {
        transfer_id: acknowledged_id,
        next_offset,
    } = stream.receive().await?
    else {
        return Err(DcError::Codec("expected file acknowledgement".into()));
    };
    if acknowledged_id != transfer_id {
        return Err(DcError::Codec(
            "file acknowledgement has the wrong transfer id".into(),
        ));
    }
    Ok(ResumeState::new(next_offset))
}

fn transfer_id_hex(transfer_id: [u8; 16]) -> String {
    transfer_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn assemble_chunks(manifest: &FileManifest, chunks: &[FileChunk]) -> Result<Vec<u8>> {
    let output_size = usize::try_from(manifest.size)
        .map_err(|_| DcError::InvalidInput("manifest size exceeds addressable memory".into()))?;
    let mut output = vec![0_u8; output_size];
    let mut expected = 0_u64;
    for chunk in chunks {
        if chunk.offset != expected
            || !chunk.verify()
            || chunk.data.len() > manifest.chunk_size as usize
        {
            return Err(DcError::Codec("invalid or out-of-order file chunk".into()));
        }
        let end = expected + chunk.data.len() as u64;
        if end > manifest.size {
            return Err(DcError::Codec("file chunk exceeds manifest size".into()));
        }
        output[expected as usize..end as usize].copy_from_slice(&chunk.data);
        expected = end;
    }
    if expected != manifest.size || sha256(&output) != manifest.sha256 {
        return Err(DcError::Codec(
            "assembled file does not match manifest checksum".into(),
        ));
    }
    Ok(output)
}

pub fn safe_join(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.as_os_str().is_empty() {
        return Err(DcError::InvalidInput("relative path is empty".into()));
    }
    for component in relative.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(DcError::InvalidInput("path escapes transfer root".into()))
            }
        }
    }
    Ok(root.join(relative))
}

fn validate_chunk_size(chunk_size: u32) -> Result<()> {
    if chunk_size == 0 || chunk_size > 4 * 1024 * 1024 {
        return Err(DcError::InvalidInput(
            "chunk size must be between 1 and 4 MiB".into(),
        ));
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn constant_time_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_round_trip_and_verify_manifest() {
        let bytes = b"0123456789abcdef";
        let manifest = FileManifest::from_bytes("sample.bin", bytes, 4).unwrap();
        let chunks = chunk_bytes(bytes, 4).unwrap();
        assert_eq!(chunks.len(), 4);
        assert_eq!(assemble_chunks(&manifest, &chunks).unwrap(), bytes);
    }

    #[test]
    fn rejects_tampered_or_out_of_order_chunks() {
        let bytes = b"abcdefgh";
        let manifest = FileManifest::from_bytes("sample.bin", bytes, 4).unwrap();
        let mut chunks = chunk_bytes(bytes, 4).unwrap();
        chunks[0].data[0] = b'X';
        assert!(assemble_chunks(&manifest, &chunks).is_err());
        let mut chunks = chunk_bytes(bytes, 4).unwrap();
        chunks.swap(0, 1);
        assert!(assemble_chunks(&manifest, &chunks).is_err());
    }

    #[test]
    fn safe_join_rejects_absolute_and_parent_paths() {
        let root = Path::new("/tmp/transfers");
        assert_eq!(
            safe_join(root, Path::new("nested/file.bin")).unwrap(),
            root.join("nested/file.bin")
        );
        assert!(safe_join(root, Path::new("../outside")).is_err());
        assert!(safe_join(root, Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn resume_state_requires_chunk_boundary() {
        let manifest = FileManifest::from_bytes("sample.bin", b"abcdefgh", 4).unwrap();
        assert!(ResumeState::new(4).validate(&manifest).is_ok());
        assert!(ResumeState::new(3).validate(&manifest).is_err());
        assert!(ResumeState::new(8).validate(&manifest).is_ok());
    }

    #[test]
    fn manifest_and_chunks_convert_to_wire_messages() {
        let manifest = FileManifest::from_bytes("sample.bin", b"abcdefgh", 4).unwrap();
        let transfer_id = [4; 16];
        let (decoded, decoded_id) = FileManifest::from_wire(manifest.to_wire(transfer_id)).unwrap();
        assert_eq!(decoded, manifest);
        assert_eq!(decoded_id, transfer_id);
        let chunk = FileChunk::new(0, b"abcd".to_vec());
        let (decoded_chunk, decoded_id) = FileChunk::from_wire(chunk.to_wire(transfer_id)).unwrap();
        assert_eq!(decoded_chunk, chunk);
        assert_eq!(decoded_id, transfer_id);
    }

    #[test]
    fn transfers_a_file_over_a_quic_stream() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let unique = format!("dc-file-transfer-test-{}", std::process::id());
            let source_root = std::env::temp_dir().join(format!("{unique}-source"));
            let destination_root = std::env::temp_dir().join(format!("{unique}-destination"));
            std::fs::create_dir_all(&source_root).unwrap();
            std::fs::create_dir_all(&destination_root).unwrap();
            let source = source_root.join("payload.bin");
            std::fs::write(&source, b"network file transfer").unwrap();

            let server = dc_transport::QuicServer::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let address = server.local_addr().unwrap();
            let server_destination_root = destination_root.clone();
            let server_task = tokio::spawn(async move {
                let connection = server.accept().await.unwrap();
                let mut stream = connection.accept_stream().await.unwrap();
                let destination = receive_file(&mut stream, &server_destination_root)
                    .await
                    .unwrap();
                // Keep the connection alive briefly so the final ACK is flushed before the
                // server-side stream and connection are dropped.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                destination
            });
            let client = dc_transport::QuicClient::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let connection = client.connect(address).await.unwrap();
            let mut stream = connection.open_stream().await.unwrap();
            send_file(&mut stream, &source, [42; 16], 4).await.unwrap();
            let destination = server_task.await.unwrap();
            assert_eq!(
                std::fs::read(destination).unwrap(),
                b"network file transfer"
            );
            std::fs::remove_dir_all(source_root).unwrap();
            std::fs::remove_dir_all(destination_root).unwrap();
        });
    }
}
