//! Durable acknowledgements for Fleet Attention items.
//!
//! Item identities are opaque SHA-256 values produced by the fleet index. The
//! store contains no prompts, paths, or agent output. TUI callers use the lazy
//! acknowledgement worker; CLI and HTTP callers load it while building their
//! one-shot projection off the interactive path.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

pub const ATTENTION_ACK_SCHEMA_VERSION: u32 = 1;

const STORE_SUBDIR: &str = "fleet";
const STORE_FILE: &str = "attention-acknowledgements.json";
const LOCK_FILE: &str = ".attention-acknowledgements.lock";
const MAX_STORE_BYTES: u64 = 1024 * 1024;
const MAX_ACKNOWLEDGEMENTS: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Acknowledgement {
    item_id: String,
    acknowledged_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Manifest {
    schema_version: u32,
    acknowledgements: Vec<Acknowledgement>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            schema_version: ATTENTION_ACK_SCHEMA_VERSION,
            acknowledgements: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AttentionAcknowledgementStore {
    root: PathBuf,
}

impl AttentionAcknowledgementStore {
    pub fn open() -> Result<Self> {
        Self::at_root(crate::session::get_app_dir()?.join(STORE_SUBDIR))
    }

    pub fn at_root(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn load(&self) -> Result<HashSet<String>> {
        Ok(self
            .read_manifest()?
            .acknowledgements
            .into_iter()
            .map(|acknowledgement| acknowledgement.item_id)
            .collect())
    }

    pub fn acknowledge(&self, item_id: &str) -> Result<()> {
        validate_item_id(item_id)?;
        let _lock = self.exclusive_lock()?;
        let mut manifest = self.read_manifest()?;
        if manifest
            .acknowledgements
            .iter()
            .any(|acknowledgement| acknowledgement.item_id == item_id)
        {
            return Ok(());
        }
        manifest.acknowledgements.push(Acknowledgement {
            item_id: item_id.to_string(),
            acknowledged_at_ms: crate::trials::now_ms(),
        });
        manifest
            .acknowledgements
            .sort_by_key(|acknowledgement| acknowledgement.acknowledged_at_ms);
        let excess = manifest
            .acknowledgements
            .len()
            .saturating_sub(MAX_ACKNOWLEDGEMENTS);
        if excess > 0 {
            manifest.acknowledgements.drain(..excess);
        }
        crate::session::atomic_write(&self.path(), &serde_json::to_vec_pretty(&manifest)?)
    }

    fn read_manifest(&self) -> Result<Manifest> {
        let path = self.path();
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Manifest::default());
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.len() > MAX_STORE_BYTES {
            bail!("attention acknowledgement store exceeds 1 MiB");
        }
        let manifest: Manifest = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("could not read {}", path.display()))?;
        validate_manifest(&manifest)?;
        Ok(manifest)
    }

    fn path(&self) -> PathBuf {
        self.root.join(STORE_FILE)
    }

    fn exclusive_lock(&self) -> Result<fs::File> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.root.join(LOCK_FILE))?;
        file.lock_exclusive()?;
        Ok(file)
    }
}

fn validate_manifest(manifest: &Manifest) -> Result<()> {
    if manifest.schema_version != ATTENTION_ACK_SCHEMA_VERSION {
        bail!(
            "unsupported attention acknowledgement schema {}",
            manifest.schema_version
        );
    }
    if manifest.acknowledgements.len() > MAX_ACKNOWLEDGEMENTS {
        bail!("attention acknowledgement store has too many entries");
    }
    let mut seen = HashSet::new();
    for acknowledgement in &manifest.acknowledgements {
        validate_item_id(&acknowledgement.item_id)?;
        if !seen.insert(acknowledgement.item_id.as_str()) {
            bail!("attention acknowledgement store contains duplicate entries");
        }
    }
    Ok(())
}

fn validate_item_id(item_id: &str) -> Result<()> {
    let Some(digest) = item_id.strip_prefix("sha256:") else {
        bail!("attention item id has an invalid prefix");
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("attention item id has an invalid digest");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    #[test]
    fn acknowledgement_is_idempotent_and_survives_reopen() {
        let root = tempfile::tempdir().unwrap();
        let store = AttentionAcknowledgementStore::at_root(root.path().join("fleet")).unwrap();
        let id = item('a');
        store.acknowledge(&id).unwrap();
        store.acknowledge(&id).unwrap();

        let reopened = AttentionAcknowledgementStore::at_root(root.path().join("fleet")).unwrap();
        assert_eq!(reopened.load().unwrap(), HashSet::from([id]));
    }

    #[test]
    fn invalid_or_future_data_is_rejected_without_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let store = AttentionAcknowledgementStore::at_root(root.path().join("fleet")).unwrap();
        fs::write(
            store.path(),
            r#"{"schema_version":2,"acknowledgements":[]}"#,
        )
        .unwrap();
        assert!(store.load().unwrap_err().to_string().contains("schema 2"));
        assert!(store.acknowledge(&item('b')).is_err());
        assert!(fs::read_to_string(store.path())
            .unwrap()
            .contains("schema_version\":2"));
    }
}
