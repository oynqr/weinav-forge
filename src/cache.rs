use crate::{
    policy::{Flavor, Role, System},
    seed::MAX_DECODED_BYTES,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Coverage {
    pub system: System,
    pub first_sample_gps: f64,
    pub last_sample_gps: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub role: Role,
    pub provider: String,
    pub url: String,
    pub sha256: String,
    pub bytes: usize,
    pub fetched_at: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    #[serde(default)]
    pub coverage: Vec<Coverage>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub flavor: Flavor,
    pub systems: Vec<System>,
    pub agnss: bool,
    pub created_at: String,
    pub sources: Vec<Source>,
    pub attempts: Vec<String>,
}

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn read(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let length = file.metadata()?.len();
    ensure!(
        length <= MAX_DECODED_BYTES,
        "{} exceeds input size limit",
        path.display()
    );
    let mut bytes = Vec::with_capacity(length as usize + 1);
    file.take(MAX_DECODED_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_DECODED_BYTES,
        "{} exceeds input size limit",
        path.display()
    );
    Ok(bytes)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o640))?;
    }
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

pub fn object_path(root: &Path, digest: &str) -> Result<PathBuf> {
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid source hash"
    );
    Ok(root.join("objects").join(digest))
}

pub fn load_source(root: &Path, source: &Source) -> Result<Vec<u8>> {
    let bytes = read(&object_path(root, &source.sha256)?)?;
    ensure!(
        bytes.len() == source.bytes && hash(&bytes) == source.sha256,
        "cached source hash mismatch: {}",
        source.url
    );
    Ok(bytes)
}

pub fn store_source(root: &Path, source: &Source, bytes: &[u8]) -> Result<()> {
    ensure!(hash(bytes) == source.sha256, "incorrect object hash");
    let path = object_path(root, &source.sha256)?;
    if path.exists() {
        ensure!(read(&path)? == bytes, "cache object is corrupt");
    } else {
        atomic_write(&path, bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_reads_preserve_bytes_and_enforce_the_size_limit() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("source");
        fs::write(&path, [])?;
        assert!(read(&path)?.is_empty());
        let bytes: Vec<_> = (0..131_071).map(|i| (i % 256) as u8).collect();
        fs::write(&path, &bytes)?;
        assert_eq!(read(&path)?, bytes);
        fs::File::create(&path)?.set_len(MAX_DECODED_BYTES + 1)?;
        assert!(read(&path).unwrap_err().to_string().contains("size limit"));
        Ok(())
    }
}
