//! Session export — directory bundle + manifest (zip-less portable export).

use crate::session::store::SessionSummary;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportManifest {
    pub version: String,
    pub session_id: String,
    pub work_dir: String,
    pub exported_at: String,
    pub title: Option<String>,
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_log: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExportResult {
    pub output_dir: PathBuf,
    pub manifest: ExportManifest,
    pub entries: usize,
}

pub fn export_session_directory(
    summary: &SessionSummary,
    output_dir: impl AsRef<Path>,
) -> anyhow::Result<ExportResult> {
    let output_dir = output_dir.as_ref().to_path_buf();
    std::fs::create_dir_all(&output_dir)?;
    let session_dir = PathBuf::from(&summary.session_dir);
    if !session_dir.is_dir() {
        anyhow::bail!("session directory missing: {}", session_dir.display());
    }
    let mut files = Vec::new();
    copy_tree(&session_dir, &output_dir.join("session"), &mut files)?;
    let manifest = ExportManifest {
        version: "1".into(),
        session_id: summary.id.clone(),
        work_dir: summary.work_dir.clone(),
        exported_at: chrono::Utc::now().to_rfc3339(),
        title: summary.title.clone(),
        files: files.clone(),
        session_log: files
            .iter()
            .find(|f| f.ends_with("kkagent.log") || f.ends_with("kimi-code.log"))
            .cloned(),
        notes: vec!["Exported by kkagent (directory bundle)".into()],
    };
    std::fs::write(
        output_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(ExportResult {
        entries: files.len() + 1,
        output_dir,
        manifest,
    })
}

fn copy_tree(src: &Path, dst: &Path, files: &mut Vec<String>) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let name = entry.file_name();
        let to = dst.join(&name);
        if ty.is_dir() {
            copy_tree(&entry.path(), &to, files)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &to)?;
            files.push(to.to_string_lossy().into());
        }
    }
    Ok(())
}

pub fn default_export_dir_name(session_id: &str) -> String {
    let short = &session_id[..session_id.len().min(8)];
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    format!("kkagent-debug-{short}-{ts}")
}
