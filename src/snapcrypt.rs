//! Encrypted, tamper-evident files for VM snapshots.
//!
//! Firecracker memory files can be many gigabytes, so they are encrypted as
//! a stream of AES-256-GCM chunks (the "STREAM" construction):
//!
//! ```text
//! "CLAWSNP1" | header_len: u32 BE | header JSON
//! chunk*  =  ct_len: u32 BE | ciphertext+tag
//! ```
//!
//! - Every chunk is authenticated with the full header bytes as associated
//!   data, so the metadata (sandbox id, kernel, Firecracker version, CPU
//!   model, key id) cannot be changed or swapped between files.
//! - The 96-bit nonce is `prefix(7) | counter(4) | last(1)`: chunks can't be
//!   reordered, and the final chunk is marked, so truncation and appended
//!   data are detected. The prefix is random per file.
//! - Files are written to a temp path, fsync'd, renamed into place, and the
//!   directory fsync'd, so a crash never leaves a half-written snapshot
//!   under the final name.
//!
//! Keys come from a [`KeyStore`]. M1 uses a host-local key file; M2 swaps in
//! per-org data keys unwrapped by OpenBao.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 8] = b"CLAWSNP1";
const TAG_LEN: usize = 16;
const MAX_HEADER: usize = 64 * 1024;
/// Plaintext bytes per chunk.
pub const DEFAULT_CHUNK: u32 = 1024 * 1024;
const MAX_CHUNK: u32 = 16 * 1024 * 1024;

/// What a snapshot file is bound to. Checked before restoring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub sandbox_id: String,
    /// "memory" or "vmstate".
    pub kind: String,
    pub firecracker_version: String,
    pub kernel_sha256: String,
    pub cpu_model: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Header {
    version: u32,
    key_id: String,
    nonce_prefix: String,
    chunk_size: u32,
    meta: SnapshotMeta,
}

/// Source of 256-bit keys by id.
pub trait KeyStore {
    fn key(&self, key_id: &str) -> Result<[u8; 32]>;
}

/// Keys stored as 32-byte files (mode 0600) in a 0700 directory. Creates a
/// key on first use.
pub struct LocalKeyStore {
    dir: PathBuf,
}

impl LocalKeyStore {
    pub const DEFAULT_KEY_ID: &'static str = "host-local-v1";

    pub fn new(dir: impl Into<PathBuf>) -> LocalKeyStore {
        LocalKeyStore { dir: dir.into() }
    }

    fn valid_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 64
            && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }
}

impl KeyStore for LocalKeyStore {
    fn key(&self, key_id: &str) -> Result<[u8; 32]> {
        if !Self::valid_id(key_id) {
            bail!("invalid key id {key_id:?}");
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .with_context(|| format!("cannot create key directory {}", self.dir.display()))?;
        let path = self.dir.join(key_id);
        match fs::read(&path) {
            Ok(bytes) => bytes
                .try_into()
                .map_err(|_| anyhow!("key file {} is not 32 bytes", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut key = [0u8; 32];
                rand::rngs::OsRng.fill_bytes(&mut key);
                let mut f = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                    .with_context(|| format!("cannot create key file {}", path.display()))?;
                f.write_all(&key)?;
                f.sync_all()?;
                Ok(key)
            }
            Err(e) => Err(e).with_context(|| format!("cannot read key file {}", path.display())),
        }
    }
}

fn nonce(prefix: &[u8; 7], counter: u32, last: bool) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..7].copy_from_slice(prefix);
    n[7..11].copy_from_slice(&counter.to_be_bytes());
    n[11] = last as u8;
    n
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Read until `buf` is full or EOF. Returns bytes read.
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Temp sibling of `dest`, created exclusively with mode 0600.
fn create_temp(dest: &Path) -> Result<(PathBuf, File)> {
    let name = dest
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", dest.display()))?;
    let dir = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let tmp = dir.join(format!(".{}.{:08x}.tmp", name.to_string_lossy(), rand::random::<u32>()));
    let f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("cannot create {}", tmp.display()))?;
    Ok((tmp, f))
}

/// fsync, rename into place, fsync the directory.
fn commit(tmp: &Path, file: File, dest: &Path) -> Result<()> {
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, dest).with_context(|| format!("cannot move into {}", dest.display()))?;
    if let Some(dir) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Encrypt `src` into `dest` (atomically). Returns plaintext bytes written.
pub fn encrypt_file(
    src: &Path,
    dest: &Path,
    keys: &dyn KeyStore,
    key_id: &str,
    meta: &SnapshotMeta,
    chunk_size: u32,
) -> Result<u64> {
    if chunk_size == 0 || chunk_size > MAX_CHUNK {
        bail!("chunk size must be between 1 and {MAX_CHUNK}");
    }
    let key = keys.key(key_id)?;
    let cipher = Aes256Gcm::new_from_slice(&key).expect("32-byte key");

    let mut prefix = [0u8; 7];
    rand::rngs::OsRng.fill_bytes(&mut prefix);
    let header = Header {
        version: 1,
        key_id: key_id.to_string(),
        nonce_prefix: crate::wire::messages::hex(&prefix),
        chunk_size,
        meta: meta.clone(),
    };
    let json = serde_json::to_vec(&header)?;
    let mut aad = Vec::with_capacity(12 + json.len());
    aad.extend_from_slice(MAGIC);
    aad.extend_from_slice(&(json.len() as u32).to_be_bytes());
    aad.extend_from_slice(&json);

    let mut input = BufReader::new(
        File::open(src).with_context(|| format!("cannot open {}", src.display()))?,
    );
    let (tmp, file) = create_temp(dest)?;
    let result = (|| -> Result<(u64, File)> {
        let mut out = BufWriter::new(file);
        out.write_all(&aad)?;
        let mut buf = vec![0u8; chunk_size as usize + TAG_LEN];
        let mut counter: u32 = 0;
        let mut total: u64 = 0;
        loop {
            let n = read_full(&mut input, &mut buf[..chunk_size as usize])?;
            let last = n < chunk_size as usize;
            let mut chunk = buf[..n].to_vec();
            cipher
                .encrypt_in_place(Nonce::from_slice(&nonce(&prefix, counter, last)), &aad, &mut chunk)
                .map_err(|_| anyhow!("encryption failed"))?;
            out.write_all(&(chunk.len() as u32).to_be_bytes())?;
            out.write_all(&chunk)?;
            total += n as u64;
            if last {
                break;
            }
            counter = counter
                .checked_add(1)
                .ok_or_else(|| anyhow!("file too large for this chunk size"))?;
        }
        let file = out.into_inner().map_err(|e| e.into_error())?;
        Ok((total, file))
    })();
    match result {
        Ok((total, file)) => {
            commit(&tmp, file, dest)?;
            Ok(total)
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Read and parse a snapshot file's header (not authenticated until the
/// chunks are decrypted, so only use it to decide whether to try).
pub fn read_meta(path: &Path) -> Result<(String, SnapshotMeta)> {
    let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let (_, header) = read_header(&mut f)?;
    Ok((header.key_id, header.meta))
}

fn read_header<R: Read>(r: &mut R) -> Result<(Vec<u8>, Header)> {
    let mut fixed = [0u8; 12];
    if read_full(r, &mut fixed)? != 12 || &fixed[..8] != MAGIC {
        bail!("not a clawstainer snapshot file");
    }
    let len = u32::from_be_bytes(fixed[8..12].try_into().unwrap()) as usize;
    if len > MAX_HEADER {
        bail!("snapshot header too large");
    }
    let mut json = vec![0u8; len];
    if read_full(r, &mut json)? != len {
        bail!("truncated snapshot header");
    }
    let header: Header = serde_json::from_slice(&json).context("corrupt snapshot header")?;
    if header.version != 1 {
        bail!("unsupported snapshot format version {}", header.version);
    }
    if header.chunk_size == 0 || header.chunk_size > MAX_CHUNK {
        bail!("invalid chunk size in snapshot header");
    }
    let mut aad = fixed.to_vec();
    aad.extend_from_slice(&json);
    Ok((aad, header))
}

/// Decrypt `src` into `dest` (atomically), verifying every chunk. Nothing is
/// left at `dest` unless the whole file authenticates. Returns the metadata.
pub fn decrypt_file(src: &Path, dest: &Path, keys: &dyn KeyStore) -> Result<SnapshotMeta> {
    let mut input = BufReader::new(
        File::open(src).with_context(|| format!("cannot open {}", src.display()))?,
    );
    let (aad, header) = read_header(&mut input)?;
    let prefix: [u8; 7] = hex_decode(&header.nonce_prefix)
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| anyhow!("corrupt nonce prefix"))?;
    let key = keys.key(&header.key_id)?;
    let cipher = Aes256Gcm::new_from_slice(&key).expect("32-byte key");
    let max_ct = header.chunk_size as usize + TAG_LEN;

    let (tmp, file) = create_temp(dest)?;
    let result = (|| -> Result<File> {
        let mut out = BufWriter::new(file);
        let mut counter: u32 = 0;
        loop {
            let mut len = [0u8; 4];
            match read_full(&mut input, &mut len)? {
                0 => bail!("snapshot is truncated (no final chunk)"),
                4 => {}
                _ => bail!("snapshot is truncated mid-chunk"),
            }
            let ct_len = u32::from_be_bytes(len) as usize;
            if !(TAG_LEN..=max_ct).contains(&ct_len) {
                bail!("corrupt chunk length");
            }
            let mut chunk = vec![0u8; ct_len];
            if read_full(&mut input, &mut chunk)? != ct_len {
                bail!("snapshot is truncated mid-chunk");
            }
            // A full-size chunk is never final; a short one always is.
            let last = ct_len < max_ct;
            cipher
                .decrypt_in_place(Nonce::from_slice(&nonce(&prefix, counter, last)), &aad, &mut chunk)
                .map_err(|_| anyhow!("snapshot failed authentication (corrupt, tampered, or wrong key)"))?;
            out.write_all(&chunk)?;
            if last {
                let mut extra = [0u8; 1];
                if read_full(&mut input, &mut extra)? != 0 {
                    bail!("unexpected data after the final chunk");
                }
                break;
            }
            counter = counter.checked_add(1).ok_or_else(|| anyhow!("too many chunks"))?;
        }
        out.into_inner().map_err(|e| e.into_error().into())
    })();
    match result {
        Ok(file) => {
            commit(&tmp, file, dest)?;
            Ok(header.meta)
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedKeys([u8; 32]);
    impl KeyStore for FixedKeys {
        fn key(&self, _: &str) -> Result<[u8; 32]> {
            Ok(self.0)
        }
    }

    fn meta() -> SnapshotMeta {
        SnapshotMeta {
            sandbox_id: "sb-12345678".into(),
            kind: "memory".into(),
            firecracker_version: "v1.11.0".into(),
            kernel_sha256: "ab".repeat(32),
            cpu_model: "AMD Ryzen 7 PRO 8700GE".into(),
            created_at: "2026-11-07T00:00:00Z".into(),
        }
    }

    fn roundtrip(len: usize, chunk: u32) {
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..len).map(|i| (i * 7 % 256) as u8).collect();
        let (src, enc, dec) = (dir.path().join("p"), dir.path().join("e"), dir.path().join("d"));
        fs::write(&src, &data).unwrap();
        let keys = FixedKeys([7; 32]);
        assert_eq!(encrypt_file(&src, &enc, &keys, "k", &meta(), chunk).unwrap(), len as u64);
        assert_ne!(fs::read(&enc).unwrap()[100..].to_vec(), data.get(88..).unwrap_or(&[]).to_vec());
        assert_eq!(decrypt_file(&enc, &dec, &keys).unwrap(), meta());
        assert_eq!(fs::read(&dec).unwrap(), data, "len={len} chunk={chunk}");
    }

    #[test]
    fn roundtrips_at_chunk_boundaries() {
        for len in [0, 1, 63, 64, 65, 128, 1000] {
            roundtrip(len, 64);
        }
        roundtrip(3 * 1024 * 1024 + 5, DEFAULT_CHUNK);
    }

    fn encrypted(dir: &Path) -> (PathBuf, Vec<u8>) {
        let src = dir.join("p");
        fs::write(&src, vec![42u8; 1000]).unwrap();
        let enc = dir.join("e");
        encrypt_file(&src, &enc, &FixedKeys([1; 32]), "k", &meta(), 100).unwrap();
        let bytes = fs::read(&enc).unwrap();
        (enc, bytes)
    }

    fn rejects(bytes: &[u8], keys: &dyn KeyStore) -> String {
        let dir = tempfile::tempdir().unwrap();
        let (bad, out) = (dir.path().join("bad"), dir.path().join("out"));
        fs::write(&bad, bytes).unwrap();
        let err = decrypt_file(&bad, &out, keys).unwrap_err().to_string();
        assert!(!out.exists(), "partial output left behind");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "temp file left behind");
        err
    }

    #[test]
    fn detects_tampering_truncation_and_reordering() {
        let dir = tempfile::tempdir().unwrap();
        let (_, good) = encrypted(dir.path());
        let keys = FixedKeys([1; 32]);
        let header_len = 12 + u32::from_be_bytes(good[8..12].try_into().unwrap()) as usize;
        let chunk_ct = 4 + 100 + TAG_LEN;

        // Flip one ciphertext bit.
        let mut b = good.clone();
        b[header_len + 10] ^= 1;
        assert!(rejects(&b, &keys).contains("authentication"));

        // Change the metadata (it is authenticated as AAD).
        let mut b = good.clone();
        let pos = b.windows(11).position(|w| w == b"sb-12345678").unwrap();
        b[pos + 3] = b'9';
        assert!(rejects(&b, &keys).contains("authentication"));

        // Drop the final chunk.
        let b = good[..good.len() - (4 + TAG_LEN)].to_vec();
        assert!(rejects(&b, &keys).contains("truncated"));

        // Cut mid-chunk.
        let b = good[..header_len + chunk_ct + 7].to_vec();
        assert!(rejects(&b, &keys).contains("truncated"));

        // Swap the first two chunks.
        let mut b = good[..header_len].to_vec();
        b.extend_from_slice(&good[header_len + chunk_ct..header_len + 2 * chunk_ct]);
        b.extend_from_slice(&good[header_len..header_len + chunk_ct]);
        b.extend_from_slice(&good[header_len + 2 * chunk_ct..]);
        assert!(rejects(&b, &keys).contains("authentication"));

        // Append junk.
        let mut b = good.clone();
        b.push(0);
        assert!(rejects(&b, &keys).contains("after the final chunk"));

        // Wrong key.
        assert!(rejects(&good, &FixedKeys([2; 32])).contains("authentication"));

        // Not a snapshot at all.
        assert!(rejects(b"hello", &keys).contains("not a clawstainer snapshot"));
    }

    #[test]
    fn local_keystore_creates_private_stable_keys() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = LocalKeyStore::new(dir.path().join("keys"));
        let k1 = store.key("host-local-v1").unwrap();
        assert_eq!(store.key("host-local-v1").unwrap(), k1);
        assert_ne!(store.key("other").unwrap(), k1);
        let mode = |p: PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir.path().join("keys")), 0o700);
        assert_eq!(mode(dir.path().join("keys/host-local-v1")), 0o600);
        assert!(store.key("../escape").is_err());
    }

    #[test]
    fn read_meta_reports_binding() {
        let dir = tempfile::tempdir().unwrap();
        let (enc, _) = encrypted(dir.path());
        let (key_id, m) = read_meta(&enc).unwrap();
        assert_eq!(key_id, "k");
        assert_eq!(m, meta());
    }
}
