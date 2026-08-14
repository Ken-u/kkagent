//! KK plugin marketplace catalogs, managed installation, and persisted state.

use anyhow::{Context, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

const INSTALLED_VERSION: u32 = 1;
const MARKETPLACES_VERSION: u32 = 1;
const MAX_CATALOG_BYTES: usize = 4 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARCHIVE_FILES: usize = 10_000;

#[derive(Debug, Clone, Serialize)]
pub struct PluginMarketplace {
    pub source: String,
    pub version: Option<String>,
    pub plugins: Vec<PluginMarketplaceEntry>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginMarketplaceEntry {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    pub source: String,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    pub update_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPluginsFile {
    pub version: u32,
    #[serde(default)]
    pub plugins: Vec<InstalledPluginRecord>,
}

impl Default for InstalledPluginsFile {
    fn default() -> Self {
        Self {
            version: INSTALLED_VERSION,
            plugins: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledPluginRecord {
    pub id: String,
    pub root: String,
    pub source: String,
    pub enabled: bool,
    pub installed_at: String,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub original_source: Option<String>,
    #[serde(default)]
    pub marketplace_source: Option<String>,
    #[serde(default)]
    pub marketplace_version: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMarketplacesFile {
    pub version: u32,
    #[serde(default)]
    pub marketplaces: Vec<RegisteredPluginMarketplace>,
}

impl Default for PluginMarketplacesFile {
    fn default() -> Self {
        Self {
            version: MARKETPLACES_VERSION,
            marketplaces: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredPluginMarketplace {
    pub id: String,
    pub name: String,
    pub source: String,
    pub added_at: String,
}

#[derive(Debug, Deserialize)]
struct RawMarketplace {
    #[serde(default)]
    version: Option<String>,
    plugins: Vec<RawMarketplaceEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMarketplaceEntry {
    id: String,
    #[serde(default, alias = "name")]
    display_name: Option<String>,
    #[serde(default, alias = "url", alias = "downloadUrl")]
    source: Option<String>,
    #[serde(default, rename = "type")]
    entry_type: Option<String>,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default, alias = "shortDescription")]
    description: Option<String>,
    #[serde(default, alias = "websiteURL")]
    homepage: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
}

#[derive(Debug, Clone)]
enum CatalogLocation {
    Remote(reqwest::Url),
    Local(PathBuf),
}

pub async fn load_marketplace(source: &str, work_dir: &Path) -> Result<PluginMarketplace> {
    let location = resolve_catalog_location(source, work_dir)?;
    let (raw, resolved_source) = match &location {
        CatalogLocation::Remote(url) => {
            let response = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?
                .get(url.clone())
                .send()
                .await?
                .error_for_status()?;
            (
                read_response_limited(response, MAX_CATALOG_BYTES, "plugin marketplace").await?,
                url.to_string(),
            )
        }
        CatalogLocation::Local(path) => {
            let bytes = tokio::fs::read(path)
                .await
                .with_context(|| format!("cannot read plugin marketplace {}", path.display()))?;
            if bytes.len() > MAX_CATALOG_BYTES {
                anyhow::bail!("plugin marketplace exceeds 4 MiB")
            }
            (bytes, path.display().to_string())
        }
    };
    let parsed: RawMarketplace = serde_json::from_slice(&raw)
        .with_context(|| format!("invalid plugin marketplace {resolved_source}"))?;
    let mut plugins = Vec::with_capacity(parsed.plugins.len());
    let mut ids = std::collections::HashSet::new();
    for raw_entry in parsed.plugins {
        super::plugin::validate_plugin_name(&raw_entry.id)?;
        if !ids.insert(raw_entry.id.clone()) {
            anyhow::bail!("duplicate marketplace plugin id {}", raw_entry.id)
        }
        if let Some(entry_type) = raw_entry.entry_type.as_deref() {
            if entry_type != "plugin" {
                anyhow::bail!(
                    "marketplace plugin {} has unsupported type {entry_type}",
                    raw_entry.id
                )
            }
        }
        if let Some(tier) = raw_entry.tier.as_deref() {
            if !matches!(tier, "official" | "curated") {
                anyhow::bail!(
                    "marketplace plugin {} has unsupported tier {tier}",
                    raw_entry.id
                )
            }
        }
        if let Some(version) = raw_entry.version.as_deref() {
            semver::Version::parse(version.trim_start_matches('v')).with_context(|| {
                format!("marketplace plugin {} has invalid version", raw_entry.id)
            })?;
        }
        let source = raw_entry
            .source
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("marketplace plugin {} has no source", raw_entry.id))?;
        plugins.push(PluginMarketplaceEntry {
            display_name: raw_entry
                .display_name
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| raw_entry.id.clone()),
            source: resolve_entry_source(source, &location)?,
            id: raw_entry.id,
            tier: raw_entry.tier,
            version: raw_entry.version,
            description: raw_entry.description,
            homepage: raw_entry.homepage,
            keywords: raw_entry.keywords,
            installed: false,
            installed_version: None,
            update_available: false,
        });
    }
    plugins.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(PluginMarketplace {
        source: resolved_source,
        version: parsed.version,
        plugins,
    })
}

pub async fn read_installed(plugins_dir: &Path) -> Result<InstalledPluginsFile> {
    let path = plugins_dir.join("installed.json");
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(InstalledPluginsFile::default())
        }
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let installed: InstalledPluginsFile = serde_json::from_str(&text)
        .with_context(|| format!("invalid installed plugin state {}", path.display()))?;
    if installed.version != INSTALLED_VERSION {
        anyhow::bail!(
            "unsupported installed plugin state version {}",
            installed.version
        )
    }
    Ok(installed)
}

pub async fn read_marketplaces(plugins_dir: &Path) -> Result<PluginMarketplacesFile> {
    let path = plugins_dir.join("marketplaces.json");
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PluginMarketplacesFile::default());
        }
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let marketplaces: PluginMarketplacesFile = serde_json::from_str(&text)
        .with_context(|| format!("invalid plugin marketplace registry {}", path.display()))?;
    if marketplaces.version != MARKETPLACES_VERSION {
        anyhow::bail!(
            "unsupported plugin marketplace registry version {}",
            marketplaces.version
        )
    }
    Ok(marketplaces)
}

pub async fn add_marketplace(
    plugins_dir: &Path,
    source: &str,
    name: Option<&str>,
    work_dir: &Path,
) -> Result<RegisteredPluginMarketplace> {
    let catalog = load_marketplace(source, work_dir).await?;
    let mut marketplaces = read_marketplaces(plugins_dir).await?;
    if marketplaces
        .marketplaces
        .iter()
        .any(|marketplace| marketplace.source == catalog.source)
    {
        anyhow::bail!("plugin marketplace is already added")
    }
    let name = name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| marketplace_name(&catalog.source));
    if name.chars().count() > 80 {
        anyhow::bail!("plugin marketplace name must be at most 80 characters")
    }
    let record = RegisteredPluginMarketplace {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        source: catalog.source,
        added_at: chrono::Utc::now().to_rfc3339(),
    };
    marketplaces.marketplaces.push(record.clone());
    marketplaces
        .marketplaces
        .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    write_marketplaces(plugins_dir, &marketplaces).await?;
    Ok(record)
}

pub async fn remove_marketplace(plugins_dir: &Path, id: &str) -> Result<()> {
    let mut marketplaces = read_marketplaces(plugins_dir).await?;
    let old_len = marketplaces.marketplaces.len();
    marketplaces.marketplaces.retain(|entry| entry.id != id);
    if marketplaces.marketplaces.len() == old_len {
        anyhow::bail!("plugin marketplace {id} is not registered")
    }
    write_marketplaces(plugins_dir, &marketplaces).await
}

fn marketplace_name(source: &str) -> String {
    reqwest::Url::parse(source)
        .ok()
        .and_then(|url| {
            url.host_str().map(|host| {
                let tail = url
                    .path_segments()
                    .and_then(|mut segments| segments.next_back())
                    .filter(|value| !value.is_empty() && *value != "marketplace.json");
                tail.map_or_else(|| host.to_string(), |value| format!("{host}/{value}"))
            })
        })
        .or_else(|| {
            Path::new(source)
                .parent()
                .and_then(Path::file_name)
                .or_else(|| Path::new(source).file_stem())
                .and_then(|value| value.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "Plugin marketplace".into())
}

pub async fn install_plugin(
    plugins_dir: &Path,
    source: &str,
    marketplace: Option<(&str, &PluginMarketplaceEntry)>,
) -> Result<InstalledPluginRecord> {
    tokio::fs::create_dir_all(plugins_dir).await?;
    let download_staging = plugins_dir.join(format!(".download-{}", uuid::Uuid::new_v4()));
    let mut cleanup_download = false;
    let source_root_result: Result<PathBuf> = async {
        if is_http_source(source) {
            cleanup_download = true;
            let archive = download_archive(source).await?;
            extract_archive(archive, download_staging.clone()).await
        } else {
            let path = source_path(source)?;
            let metadata = tokio::fs::metadata(&path)
                .await
                .with_context(|| format!("cannot access plugin source {}", path.display()))?;
            if metadata.is_dir() {
                Ok(tokio::fs::canonicalize(path).await?)
            } else {
                cleanup_download = true;
                let archive = tokio::fs::read(&path).await?;
                if archive.len() > MAX_ARCHIVE_BYTES {
                    anyhow::bail!("plugin archive exceeds 64 MiB")
                }
                extract_archive(archive, download_staging.clone()).await
            }
        }
    }
    .await;

    let source_root = match source_root_result {
        Ok(source_root) => source_root,
        Err(error) => {
            if cleanup_download {
                let _ = tokio::fs::remove_dir_all(&download_staging).await;
            }
            return Err(error);
        }
    };

    let result = install_from_directory(plugins_dir, source, &source_root, marketplace).await;
    if cleanup_download {
        let _ = tokio::fs::remove_dir_all(&download_staging).await;
    }
    result
}

pub async fn set_plugin_enabled(plugins_dir: &Path, id: &str, enabled: bool) -> Result<()> {
    super::plugin::validate_plugin_name(id)?;
    let mut installed = read_installed(plugins_dir).await?;
    let record = installed
        .plugins
        .iter_mut()
        .find(|record| record.id == id)
        .ok_or_else(|| anyhow::anyhow!("plugin {id} is not managed by the marketplace"))?;
    record.enabled = enabled;
    record.updated_at = Some(chrono::Utc::now().to_rfc3339());
    write_installed(plugins_dir, &installed).await
}

pub async fn remove_plugin(plugins_dir: &Path, id: &str) -> Result<()> {
    super::plugin::validate_plugin_name(id)?;
    let mut installed = read_installed(plugins_dir).await?;
    let old_len = installed.plugins.len();
    installed.plugins.retain(|record| record.id != id);
    if installed.plugins.len() == old_len {
        anyhow::bail!("plugin {id} is not managed by the marketplace")
    }
    write_installed(plugins_dir, &installed).await
}

fn resolve_catalog_location(source: &str, work_dir: &Path) -> Result<CatalogLocation> {
    let source = source.trim();
    if source.is_empty() {
        anyhow::bail!("plugin marketplace source must not be empty")
    }
    if source.starts_with("http://") || source.starts_with("https://") {
        return Ok(CatalogLocation::Remote(reqwest::Url::parse(source)?));
    }
    if source.starts_with("file://") {
        let url = reqwest::Url::parse(source)?;
        let path = url
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("invalid file URL {source}"))?;
        return Ok(CatalogLocation::Local(path));
    }
    let path = PathBuf::from(source);
    Ok(CatalogLocation::Local(if path.is_absolute() {
        path
    } else {
        work_dir.join(path)
    }))
}

fn resolve_entry_source(source: &str, location: &CatalogLocation) -> Result<String> {
    let source = source.trim();
    if source.starts_with("http://") || source.starts_with("https://") {
        return Ok(reqwest::Url::parse(source)?.to_string());
    }
    if source.starts_with("file://") {
        return Ok(reqwest::Url::parse(source)?
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("invalid plugin file URL {source}"))?
            .display()
            .to_string());
    }
    let path = PathBuf::from(source);
    if path.is_absolute() {
        return Ok(path.display().to_string());
    }
    match location {
        CatalogLocation::Remote(url) => Ok(url.join(source)?.to_string()),
        CatalogLocation::Local(path) => Ok(path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path_from_urlish(source))
            .display()
            .to_string()),
    }
}

fn path_from_urlish(source: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(source.replace('/', "\\"))
    } else {
        PathBuf::from(source)
    }
}

fn is_http_source(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

fn source_path(source: &str) -> Result<PathBuf> {
    if source.starts_with("file://") {
        return reqwest::Url::parse(source)?
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("invalid plugin file URL {source}"));
    }
    let path = PathBuf::from(source);
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    })
}

async fn download_archive(source: &str) -> Result<Vec<u8>> {
    let url = github_archive_url(source)?.unwrap_or_else(|| source.to_string());
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()?
        .get(&url)
        .send()
        .await?
        .error_for_status()?;
    read_response_limited(response, MAX_ARCHIVE_BYTES, "plugin archive").await
}

async fn read_response_limited(
    response: reqwest::Response,
    limit: usize,
    label: &str,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        anyhow::bail!("{label} exceeds {} MiB", limit / 1024 / 1024)
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            anyhow::bail!("{label} exceeds {} MiB", limit / 1024 / 1024)
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn github_archive_url(source: &str) -> Result<Option<String>> {
    let url = match reqwest::Url::parse(source) {
        Ok(url) if matches!(url.host_str(), Some("github.com" | "www.github.com")) => url,
        _ => return Ok(None),
    };
    let segments: Vec<_> = url
        .path_segments()
        .map(|segments| segments.filter(|part| !part.is_empty()).collect())
        .unwrap_or_default();
    if segments.len() < 2 {
        anyhow::bail!("GitHub plugin URL must include owner and repository")
    }
    let owner = segments[0];
    let repo = segments[1].trim_end_matches(".git");
    let reference = match segments.as_slice() {
        [_, _] => "HEAD".to_string(),
        [_, _, "tree", reference @ ..] if !reference.is_empty() => reference.join("/"),
        [_, _, "commit", sha] => (*sha).to_string(),
        [_, _, "releases", "tag", tag @ ..] if !tag.is_empty() => tag.join("/"),
        _ => anyhow::bail!("unsupported GitHub plugin URL {source}"),
    };
    Ok(Some(format!(
        "https://codeload.github.com/{owner}/{repo}/zip/{reference}"
    )))
}

async fn extract_archive(bytes: Vec<u8>, destination: PathBuf) -> Result<PathBuf> {
    tokio::task::spawn_blocking(move || extract_archive_blocking(bytes, &destination)).await?
}

fn extract_archive_blocking(bytes: Vec<u8>, destination: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(destination)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("invalid plugin ZIP")?;
    if archive.len() > MAX_ARCHIVE_FILES {
        anyhow::bail!("plugin archive contains too many files")
    }
    let mut extracted_bytes = 0u64;
    for index in 0..archive.len() {
        let file = archive.by_index(index)?;
        let enclosed = file
            .enclosed_name()
            .ok_or_else(|| anyhow::anyhow!("plugin archive contains an unsafe path"))?;
        if file
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            anyhow::bail!("plugin archive contains a symbolic link")
        }
        extracted_bytes = extracted_bytes.saturating_add(file.size());
        if extracted_bytes > MAX_EXTRACTED_BYTES {
            anyhow::bail!("plugin archive expands beyond 256 MiB")
        }
        let output = destination.join(enclosed);
        if file.is_dir() {
            std::fs::create_dir_all(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output_file = std::fs::File::create(&output)?;
        #[cfg(unix)]
        if let Some(mode) = file.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            output_file.set_permissions(std::fs::Permissions::from_mode(mode & 0o777))?;
        }
        std::io::copy(&mut file.take(MAX_EXTRACTED_BYTES + 1), &mut output_file)?;
    }
    detect_plugin_root(destination)
}

fn detect_plugin_root(extracted: &Path) -> Result<PathBuf> {
    let mut candidates = Vec::new();
    find_manifest_roots(extracted, extracted, 0, &mut candidates)?;
    candidates.sort_by_key(|path| path.components().count());
    let Some(root) = candidates.first() else {
        anyhow::bail!("plugin archive has no kk.plugin.json manifest")
    };
    let depth = root.components().count();
    if candidates
        .iter()
        .skip(1)
        .any(|candidate| candidate.components().count() == depth)
    {
        anyhow::bail!("plugin archive contains multiple plugin roots")
    }
    Ok(root.clone())
}

fn find_manifest_roots(
    root: &Path,
    current: &Path,
    depth: usize,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    if depth > 8 {
        return Ok(());
    }
    if super::plugin::select_manifest(current).is_some() {
        out.push(current.to_path_buf());
        return Ok(());
    }
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!("plugin archive contains a symbolic link")
        }
        if metadata.is_dir() && path.starts_with(root) {
            find_manifest_roots(root, &path, depth + 1, out)?;
        }
    }
    Ok(())
}

async fn install_from_directory(
    plugins_dir: &Path,
    original_source: &str,
    source_root: &Path,
    marketplace: Option<(&str, &PluginMarketplaceEntry)>,
) -> Result<InstalledPluginRecord> {
    let manifest_path = super::plugin::select_manifest(source_root)
        .ok_or_else(|| anyhow::anyhow!("plugin source has no kk.plugin.json manifest"))?;
    let manifest_text = tokio::fs::read_to_string(&manifest_path).await?;
    let manifest: super::plugin::PluginManifest = serde_json::from_str(&manifest_text)?;
    super::plugin::validate_plugin_name(&manifest.name)?;
    if let Some((_, entry)) = marketplace {
        if entry.id != manifest.name {
            anyhow::bail!(
                "marketplace id {} does not match plugin manifest name {}",
                entry.id,
                manifest.name
            )
        }
    }

    let managed_dir = plugins_dir.join("managed");
    tokio::fs::create_dir_all(&managed_dir).await?;
    let target = managed_dir.join(&manifest.name);
    let staging = managed_dir.join(format!(
        ".staging-{}-{}",
        manifest.name,
        uuid::Uuid::new_v4()
    ));
    if let Err(error) = copy_directory(source_root, &staging).await {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(error);
    }

    let backup = managed_dir.join(format!(
        ".backup-{}-{}",
        manifest.name,
        uuid::Uuid::new_v4()
    ));
    let had_target = tokio::fs::metadata(&target).await.is_ok();
    if had_target {
        tokio::fs::rename(&target, &backup).await?;
    }
    if let Err(error) = tokio::fs::rename(&staging, &target).await {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        if had_target {
            let _ = tokio::fs::rename(&backup, &target).await;
        }
        return Err(error.into());
    }

    let mut installed = read_installed(plugins_dir).await?;
    let previous = installed
        .plugins
        .iter()
        .find(|record| record.id == manifest.name)
        .cloned();
    let now = chrono::Utc::now().to_rfc3339();
    let record = InstalledPluginRecord {
        id: manifest.name.clone(),
        root: target.display().to_string(),
        source: source_kind(original_source).into(),
        enabled: previous
            .as_ref()
            .map(|record| record.enabled)
            .unwrap_or(true),
        installed_at: previous
            .as_ref()
            .map(|record| record.installed_at.clone())
            .unwrap_or_else(|| now.clone()),
        updated_at: Some(now),
        original_source: Some(original_source.to_string()),
        marketplace_source: marketplace
            .map(|(source, _)| source.to_string())
            .or_else(|| {
                previous
                    .as_ref()
                    .and_then(|record| record.marketplace_source.clone())
            }),
        marketplace_version: marketplace
            .and_then(|(_, entry)| entry.version.clone())
            .or_else(|| {
                previous
                    .as_ref()
                    .and_then(|record| record.marketplace_version.clone())
            }),
        version: (!manifest.version.is_empty()).then_some(manifest.version),
    };
    installed
        .plugins
        .retain(|existing| existing.id != record.id);
    installed.plugins.push(record.clone());
    installed.plugins.sort_by(|a, b| a.id.cmp(&b.id));
    if let Err(error) = write_installed(plugins_dir, &installed).await {
        let _ = tokio::fs::remove_dir_all(&target).await;
        if had_target {
            let _ = tokio::fs::rename(&backup, &target).await;
        }
        return Err(error);
    }
    if had_target {
        let _ = tokio::fs::remove_dir_all(backup).await;
    }
    Ok(record)
}

fn source_kind(source: &str) -> &'static str {
    if github_archive_url(source).ok().flatten().is_some() {
        "github"
    } else if is_http_source(source) || source.ends_with(".zip") {
        "zip-url"
    } else {
        "local-path"
    }
}

async fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || copy_directory_blocking(&source, &destination)).await?
}

fn copy_directory_blocking(source: &Path, destination: &Path) -> Result<()> {
    let source = std::fs::canonicalize(source)?;
    std::fs::create_dir_all(destination)?;
    let mut stack = vec![(source.clone(), destination.to_path_buf())];
    let mut files = 0usize;
    let mut bytes = 0u64;
    while let Some((from, to)) = stack.pop() {
        for entry in std::fs::read_dir(&from)? {
            let entry = entry?;
            let from_path = entry.path();
            let to_path = to.join(entry.file_name());
            let metadata = std::fs::symlink_metadata(&from_path)?;
            if metadata.file_type().is_symlink() {
                anyhow::bail!("plugin source contains a symbolic link")
            }
            if metadata.is_dir() {
                std::fs::create_dir_all(&to_path)?;
                stack.push((from_path, to_path));
            } else if metadata.is_file() {
                files += 1;
                bytes = bytes.saturating_add(metadata.len());
                if files > MAX_ARCHIVE_FILES || bytes > MAX_EXTRACTED_BYTES {
                    anyhow::bail!("plugin source is too large")
                }
                std::fs::copy(from_path, to_path)?;
            }
        }
    }
    Ok(())
}

async fn write_installed(plugins_dir: &Path, installed: &InstalledPluginsFile) -> Result<()> {
    tokio::fs::create_dir_all(plugins_dir).await?;
    let target = plugins_dir.join("installed.json");
    let staging = plugins_dir.join(format!(".installed-{}.tmp", uuid::Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(installed)?;
    tokio::fs::write(&staging, bytes).await?;
    let backup = plugins_dir.join(format!(".installed-{}.backup", uuid::Uuid::new_v4()));
    let had_target = tokio::fs::metadata(&target).await.is_ok();
    if had_target {
        if let Err(error) = tokio::fs::rename(&target, &backup).await {
            let _ = tokio::fs::remove_file(&staging).await;
            return Err(error.into());
        }
    }
    if let Err(error) = tokio::fs::rename(&staging, &target).await {
        let _ = tokio::fs::remove_file(&staging).await;
        if had_target {
            let _ = tokio::fs::rename(&backup, &target).await;
        }
        return Err(error.into());
    }
    if had_target {
        let _ = tokio::fs::remove_file(backup).await;
    }
    Ok(())
}

async fn write_marketplaces(
    plugins_dir: &Path,
    marketplaces: &PluginMarketplacesFile,
) -> Result<()> {
    tokio::fs::create_dir_all(plugins_dir).await?;
    let target = plugins_dir.join("marketplaces.json");
    let staging = plugins_dir.join(format!(".marketplaces-{}.tmp", uuid::Uuid::new_v4()));
    tokio::fs::write(&staging, serde_json::to_vec_pretty(marketplaces)?).await?;
    if let Err(error) = replace_file(&staging, &target, plugins_dir, "marketplaces").await {
        let _ = tokio::fs::remove_file(&staging).await;
        return Err(error);
    }
    Ok(())
}

async fn replace_file(staging: &Path, target: &Path, dir: &Path, prefix: &str) -> Result<()> {
    let backup = dir.join(format!(".{prefix}-{}.backup", uuid::Uuid::new_v4()));
    let had_target = tokio::fs::metadata(target).await.is_ok();
    if had_target {
        tokio::fs::rename(target, &backup).await?;
    }
    if let Err(error) = tokio::fs::rename(staging, target).await {
        if had_target {
            let _ = tokio::fs::rename(&backup, target).await;
        }
        return Err(error.into());
    }
    if had_target {
        let _ = tokio::fs::remove_file(backup).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("kkagent-marketplace-{}", uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn resolves_local_marketplace_sources() {
        let root = temp_dir();
        tokio::fs::create_dir_all(root.join("plugins/demo"))
            .await
            .unwrap();
        tokio::fs::write(
            root.join("plugins/demo/kk.plugin.json"),
            r#"{"name":"demo","version":"1.0.0"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            root.join("marketplace.json"),
            r#"{"version":"1","plugins":[{"id":"demo","source":"./plugins/demo"}]}"#,
        )
        .await
        .unwrap();
        let catalog = load_marketplace("marketplace.json", &root).await.unwrap();
        assert_eq!(catalog.plugins.len(), 1);
        assert_eq!(catalog.plugins[0].display_name, "demo");
        assert_eq!(
            PathBuf::from(&catalog.plugins[0].source),
            root.join("./plugins/demo")
        );
        let manager = crate::plugin::PluginManager::discover(&root.join("plugins")).await;
        let catalog = manager
            .marketplace("marketplace.json", &root)
            .await
            .unwrap();
        assert!(catalog.plugins[0].installed);
        assert_eq!(
            catalog.plugins[0].installed_version.as_deref(),
            Some("1.0.0")
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn adds_and_removes_a_marketplace_registry_entry() {
        let root = temp_dir();
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(root.join("marketplace.json"), r#"{"plugins":[]}"#)
            .await
            .unwrap();

        let added = add_marketplace(
            &root.join("plugins"),
            "marketplace.json",
            Some("Local test"),
            &root,
        )
        .await
        .unwrap();
        assert_eq!(added.name, "Local test");
        assert!(
            add_marketplace(&root.join("plugins"), "marketplace.json", None, &root,)
                .await
                .unwrap_err()
                .to_string()
                .contains("already added")
        );
        assert_eq!(
            read_marketplaces(&root.join("plugins"))
                .await
                .unwrap()
                .marketplaces
                .len(),
            1
        );
        remove_marketplace(&root.join("plugins"), &added.id)
            .await
            .unwrap();
        assert!(read_marketplaces(&root.join("plugins"))
            .await
            .unwrap()
            .marketplaces
            .is_empty());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn installs_and_disables_a_local_plugin() {
        let root = temp_dir();
        let source = root.join("source");
        let plugins = root.join("home/plugins");
        tokio::fs::create_dir_all(&source).await.unwrap();
        tokio::fs::write(
            source.join("kk.plugin.json"),
            r#"{"name":"demo","version":"1.2.3"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            root.join("marketplace.json"),
            format!(
                r#"{{"plugins":[{{"id":"demo","version":"1.2.3","source":{}}}]}}"#,
                serde_json::to_string(source.to_str().unwrap()).unwrap()
            ),
        )
        .await
        .unwrap();
        let initial_catalog = load_marketplace("marketplace.json", &root).await.unwrap();
        let record = install_plugin(
            &plugins,
            &initial_catalog.plugins[0].source,
            Some((&initial_catalog.source, &initial_catalog.plugins[0])),
        )
        .await
        .unwrap();
        assert_eq!(record.id, "demo");
        assert!(plugins.join("managed/demo/kk.plugin.json").is_file());
        set_plugin_enabled(&plugins, "demo", false).await.unwrap();
        assert!(!read_installed(&plugins).await.unwrap().plugins[0].enabled);
        let manager = crate::plugin::PluginManager::discover(&plugins).await;
        let listed = manager.list().await;
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].enabled);
        assert!(listed[0].managed);
        tokio::fs::write(
            source.join("kk.plugin.json"),
            r#"{"name":"demo","version":"2.0.0"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            root.join("marketplace.json"),
            format!(
                r#"{{"plugins":[{{"id":"demo","version":"2.0.0","source":{}}}]}}"#,
                serde_json::to_string(source.to_str().unwrap()).unwrap()
            ),
        )
        .await
        .unwrap();
        let catalog = manager
            .marketplace("marketplace.json", &root)
            .await
            .unwrap();
        assert!(catalog.plugins[0].installed);
        assert_eq!(
            catalog.plugins[0].installed_version.as_deref(),
            Some("1.2.3")
        );
        assert!(catalog.plugins[0].update_available);
        let updated = manager.update("demo").await.unwrap();
        assert_eq!(updated.version.as_deref(), Some("2.0.0"));
        assert!(!updated.enabled);
        assert_eq!(
            updated.marketplace_source.as_deref(),
            Some(initial_catalog.source.as_str())
        );
        remove_plugin(&plugins, "demo").await.unwrap();
        assert!(read_installed(&plugins).await.unwrap().plugins.is_empty());
        assert!(plugins.join("managed/demo").is_dir());
        manager.reload().await.unwrap();
        assert!(manager.list().await.is_empty());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn rejects_archive_path_traversal() {
        let root = temp_dir();
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("../escaped", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"unsafe").unwrap();
        let archive = writer.finish().unwrap().into_inner();
        let error = extract_archive(archive, root.join("extract"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unsafe path"));
        assert!(!root.join("escaped").exists());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn installs_a_nested_zip_plugin() {
        let root = temp_dir();
        tokio::fs::create_dir_all(&root).await.unwrap();
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(
                "demo-release/kk.plugin.json",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
        writer
            .write_all(br#"{"name":"zip-demo","version":"1.0.0"}"#)
            .unwrap();
        let archive = writer.finish().unwrap().into_inner();
        let archive_path = root.join("demo.zip");
        tokio::fs::write(&archive_path, archive).await.unwrap();
        let plugins = root.join("plugins");
        let record = install_plugin(&plugins, archive_path.to_str().unwrap(), None)
            .await
            .unwrap();
        assert_eq!(record.id, "zip-demo");
        assert!(plugins.join("managed/zip-demo/kk.plugin.json").is_file());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn marketplace_installs_multiple_plugins() {
        let root = temp_dir();
        let workspace = root.join("marketplace-root");
        for (name, version) in [("alpha-demo", "1.0.0"), ("beta-demo", "2.0.0")] {
            tokio::fs::create_dir_all(workspace.join(format!("plugins/{name}")))
                .await
                .unwrap();
            tokio::fs::write(
                workspace.join(format!("plugins/{name}/kk.plugin.json")),
                format!(r#"{{"name":"{name}","version":"{version}"}}"#),
            )
            .await
            .unwrap();
        }
        tokio::fs::write(
            workspace.join("marketplace.json"),
            r#"{
  "version": "1",
  "plugins": [
    {"id": "alpha-demo", "source": "./plugins/alpha-demo"},
    {"id": "beta-demo", "source": "./plugins/beta-demo"}
  ]
}"#,
        )
        .await
        .unwrap();

        let catalog_path = workspace.join("marketplace.json");
        let catalog = load_marketplace(catalog_path.to_str().unwrap(), &workspace)
            .await
            .unwrap();
        assert_eq!(catalog.plugins.len(), 2);

        let plugins_dir = root.join("plugins");
        for entry in &catalog.plugins {
            install_plugin(&plugins_dir, &entry.source, Some((&catalog.source, entry)))
                .await
                .unwrap();
        }
        let manager = crate::plugin::PluginManager::discover(&plugins_dir).await;
        let listed = manager.list().await;
        assert_eq!(
            listed
                .iter()
                .map(|plugin| plugin.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha-demo", "beta-demo"]
        );
        assert!(listed.iter().all(|plugin| plugin.managed && plugin.enabled));
        let refreshed = manager
            .marketplace(catalog_path.to_str().unwrap(), &workspace)
            .await
            .unwrap();
        assert!(refreshed.plugins.iter().all(|entry| entry.installed));
        assert!(refreshed
            .plugins
            .iter()
            .all(|entry| !entry.update_available));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[test]
    fn resolves_supported_github_sources_without_api_calls() {
        assert_eq!(
            github_archive_url("https://github.com/acme/demo")
                .unwrap()
                .unwrap(),
            "https://codeload.github.com/acme/demo/zip/HEAD"
        );
        assert_eq!(
            github_archive_url("https://github.com/acme/demo/tree/release/1.x")
                .unwrap()
                .unwrap(),
            "https://codeload.github.com/acme/demo/zip/release/1.x"
        );
        assert_eq!(
            github_archive_url("https://github.com/acme/demo/releases/tag/v1.2.3")
                .unwrap()
                .unwrap(),
            "https://codeload.github.com/acme/demo/zip/v1.2.3"
        );
    }
}
