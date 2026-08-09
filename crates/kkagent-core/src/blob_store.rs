//! Simple blob store for media/attachments (uuid-keyed files).

use std::path::{Path, PathBuf};

pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn session_store(working_dir: &Path) -> Self {
        Self::new(working_dir.join(".kkagent").join("blobs"))
    }

    pub async fn put(&self, bytes: &[u8], ext: &str) -> anyhow::Result<(String, PathBuf)> {
        tokio::fs::create_dir_all(&self.root).await?;
        let id = uuid::Uuid::new_v4().to_string();
        let short = id[..8].to_string();
        let path = self.root.join(format!("{short}.{ext}"));
        tokio::fs::write(&path, bytes).await?;
        Ok((short, path))
    }

    pub fn path_for(&self, id: &str, ext: &str) -> PathBuf {
        self.root.join(format!("{id}.{ext}"))
    }

    pub async fn get(&self, id: &str, ext: &str) -> anyhow::Result<Vec<u8>> {
        Ok(tokio::fs::read(self.path_for(id, ext)).await?)
    }
}

/// Resolve media path references in user text (`@file` / absolute paths).
pub fn resolve_media_refs(text: &str, cwd: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        let t = token.trim_matches(|c: char| c == '`' || c == '"' || c == '\'');
        if let Some(rest) = t.strip_prefix('@') {
            let p = PathBuf::from(rest);
            let full = if p.is_absolute() { p } else { cwd.join(p) };
            if full.exists() {
                out.push(full);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_at_ref() {
        // May be empty if @path missing; just ensure no panic.
        let _ = resolve_media_refs("see @Cargo.toml please", Path::new("."));
    }
}
