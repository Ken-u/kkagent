//! `GenerateImage` — text-to-image tool backed by an OpenAI Images
//! compatible endpoint (the official API or a reverse proxy such as
//! CLIProxyAPI exposing a Codex subscription).
//!
//! Registration is opt-in, resolved by the host (`build_turn_tool_registry`)
//! from the effective service config: global `[services.image_gen]` in
//! `config.toml` enables the tool everywhere; a trusted workspace's
//! `<workspace>/.kk/config.toml` `[services.image_gen]` overrides it for
//! that project only.
//!
//! Generated images flow back through `ToolOutput::images`, so vision models
//! can see and iterate on the result; `output_path` saves files into the
//! workspace (multi-image calls get `-0`/`-1` suffixes).

use crate::{Tool, ToolContext, ToolOutput};
use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use kkagent_config::ImageGenServiceConfig;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct GenerateImageTool {
    config: ImageGenServiceConfig,
    client: reqwest::Client,
}

impl GenerateImageTool {
    /// Build the tool from a resolved service config. The host decides
    /// whether the tool registers at all; here we only validate that the
    /// config is usable (non-empty `base_url`).
    pub fn new(config: ImageGenServiceConfig) -> Result<Self> {
        if config.base_url_trimmed().is_none() {
            anyhow::bail!("image_gen service config is missing `base_url`");
        }
        let timeout = Duration::from_millis(config.timeout_ms.unwrap_or(180_000).max(1_000));
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(timeout)
            .build()?;
        Ok(Self { config, client })
    }
}

#[derive(Deserialize)]
struct GenerateImageArgs {
    prompt: String,
    size: Option<String>,
    n: Option<u32>,
    output_path: Option<String>,
}

#[derive(Deserialize)]
struct ImagesApiResponse {
    #[serde(default)]
    data: Vec<ImagesApiItem>,
}

#[derive(Deserialize)]
struct ImagesApiItem {
    b64_json: Option<String>,
    url: Option<String>,
}

impl GenerateImageTool {
    async fn generate(&self, args: &GenerateImageArgs) -> Result<Vec<(Vec<u8>, String)>> {
        let base_url = self
            .config
            .base_url
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string();
        let endpoint = format!("{base_url}/images/generations");
        let model = self
            .config
            .model
            .clone()
            .unwrap_or_else(|| "gpt-image-2".into());

        let mut body = json!({
            "model": model,
            "prompt": args.prompt,
            "n": args.n.unwrap_or(1).clamp(1, 4),
            "response_format": "b64_json",
        });
        let size = args
            .size
            .clone()
            .or_else(|| self.config.default_size.clone());
        if let Some(size) = size {
            body["size"] = json!(size);
        }

        let mut request = self.client.post(&endpoint).json(&body);
        if let Some(key) = self.config.api_key() {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("request {endpoint}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("image API returned {}: {}", status, truncate(&text, 512));
        }
        let parsed: ImagesApiResponse =
            serde_json::from_str(&text).context("parse image API response")?;

        let n_expected = args.n.unwrap_or(1).clamp(1, 4) as usize;
        let mut images = Vec::new();
        for (idx, item) in parsed.data.into_iter().take(n_expected.max(1)).enumerate() {
            if let Some(b64) = item.b64_json.as_deref().filter(|s| !s.is_empty()) {
                let bytes = BASE64
                    .decode(b64)
                    .with_context(|| format!("decode image #{idx} b64_json"))?;
                images.push((bytes, "image/png".to_string()));
            } else if let Some(url) = item.url.as_deref().filter(|s| !s.is_empty()) {
                if let Some(rest) = url.strip_prefix("data:") {
                    let (meta, b64) = rest.split_once(',').context("parse data URL")?;
                    let mime = meta
                        .split(';')
                        .next()
                        .filter(|m| !m.is_empty())
                        .unwrap_or("image/png")
                        .to_string();
                    let bytes = BASE64.decode(b64).context("decode data URL image")?;
                    images.push((bytes, mime));
                } else {
                    let bytes = self
                        .client
                        .get(url)
                        .send()
                        .await
                        .with_context(|| format!("download image #{idx} from url"))?
                        .error_for_status()?
                        .bytes()
                        .await
                        .context("read image bytes")?;
                    let mime = sniff_image_mime(&bytes);
                    images.push((bytes.to_vec(), mime));
                }
            }
        }
        if images.is_empty() {
            anyhow::bail!("image API returned no usable image data");
        }
        Ok(images)
    }
}

fn sniff_image_mime(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png".into()
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg".into()
    } else if bytes.starts_with(b"GIF8") {
        "image/gif".into()
    } else if bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        "image/webp".into()
    } else {
        "image/png".into()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}

fn insert_index_suffix(path: &str, idx: usize) -> String {
    let dot = path.rfind('.').filter(|pos| *pos > 0);
    match dot {
        Some(pos) => format!("{}-{idx}{}", &path[..pos], &path[pos..]),
        None => format!("{path}-{idx}"),
    }
}

fn save_image(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
    }
    std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}

#[async_trait]
impl Tool for GenerateImageTool {
    fn name(&self) -> &str {
        "GenerateImage"
    }

    fn description(&self) -> &str {
        "Generate a raster image from a text prompt via an OpenAI Images compatible API. \
         Returns the image inline (visible to vision models) and optionally saves it to \
         output_path within the workspace. Prefer editing existing SVG or code-native \
         assets when the task is better served deterministically."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Text description of the image to generate. Be specific: subject, style, composition, lighting, palette."
                },
                "size": {
                    "type": "string",
                    "description": "Output size, e.g. \"1024x1024\", \"1536x1024\", \"1024x1536\". Optional; backend default applies."
                },
                "n": {
                    "type": "integer",
                    "description": "Number of images to generate (1-4). Optional, default 1."
                },
                "output_path": {
                    "type": "string",
                    "description": "Optional file path (relative to the workspace) to save the image to. Multi-image calls append -0/-1 suffixes."
                }
            },
            "required": ["prompt"]
        })
    }

    fn read_only(&self) -> bool {
        // Generates files only when output_path is set; the API call itself
        // spends quota, so it stays out of the default-approve set.
        false
    }

    fn accesses(&self, input: &Value, working_dir: &Path) -> crate::ToolAccesses {
        match input
            .get("output_path")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            Some(output) => {
                let target = if Path::new(output).is_absolute() {
                    PathBuf::from(output)
                } else {
                    working_dir.join(output)
                };
                crate::tool_accesses::write_file(target.display().to_string())
            }
            None => crate::tool_accesses::none(),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let args: GenerateImageArgs =
            serde_json::from_value(input).context("invalid GenerateImage arguments")?;
        if args.prompt.trim().is_empty() {
            anyhow::bail!("prompt must not be empty");
        }

        let images = self.generate(&args).await?;
        let mut output = ToolOutput::success(format!(
            "Generated {} image(s){}.",
            images.len(),
            if images.len() == 1 {
                ""
            } else {
                " (appended inline)"
            }
        ));
        let mut saved = Vec::new();
        for (idx, (bytes, mime)) in images.iter().enumerate() {
            if let Some(path) = args.output_path.as_deref() {
                let target = if images.len() == 1 {
                    path.to_string()
                } else {
                    insert_index_suffix(path, idx)
                };
                let resolved = if Path::new(&target).is_absolute() {
                    PathBuf::from(&target)
                } else {
                    ctx.working_dir.join(&target)
                };
                if !ctx.is_path_allowed(&resolved) {
                    anyhow::bail!(
                        "output_path `{}` is outside the workspace",
                        resolved.display()
                    );
                }
                save_image(&resolved, bytes)
                    .with_context(|| format!("save image #{idx} to {}", resolved.display()))?;
                saved.push(target);
            }
            output = output.with_image(mime.clone(), BASE64.encode(bytes));
        }
        if !saved.is_empty() {
            output.content = format!(
                "Generated {} image(s). Saved to: {}.",
                images.len(),
                saved.join(", ")
            );
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kkagent_config::{load_project_config, ProjectConfigFile};

    fn tmp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("kk-imagegen-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn parses_full_config() {
        let config: ImageGenServiceConfig = toml::from_str(
            r#"
            base_url = "http://127.0.0.1:8317/v1"
            api_key_env = "CPA_KEY"
            model = "gpt-image-2"
            timeout_ms = 60000
            default_size = "1536x1024"
            "#,
        )
        .unwrap();
        assert_eq!(config.base_url.as_deref(), Some("http://127.0.0.1:8317/v1"));
        assert_eq!(config.model.as_deref(), Some("gpt-image-2"));
        assert_eq!(config.default_size.as_deref(), Some("1536x1024"));
    }

    #[test]
    fn project_config_overlay_parses_image_gen() {
        let dir = tmp_dir();
        std::fs::create_dir_all(dir.join(".kk")).unwrap();
        std::fs::write(
            dir.join(".kk").join("config.toml"),
            "[services.image_gen]\nbase_url = \"http://127.0.0.1:8317/v1\"\nmodel = \"gpt-image-2\"\n",
        )
        .unwrap();
        let project: ProjectConfigFile = load_project_config(&dir).unwrap().unwrap();
        let config = project
            .services
            .expect("services section")
            .image_gen
            .expect("image_gen override");
        assert_eq!(config.base_url.as_deref(), Some("http://127.0.0.1:8317/v1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn project_config_absent_means_no_overrides() {
        let dir = tmp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_project_config(&dir).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn project_config_rejects_unknown_sections() {
        let dir = tmp_dir();
        std::fs::create_dir_all(dir.join(".kk")).unwrap();
        std::fs::write(
            dir.join(".kk").join("config.toml"),
            "[providers.x]\ny = 1\n",
        )
        .unwrap();
        assert!(load_project_config(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tool_rejects_config_without_base_url() {
        let config = ImageGenServiceConfig {
            model: Some("gpt-image-2".into()),
            ..Default::default()
        };
        let err = GenerateImageTool::new(config)
            .err()
            .expect("missing base_url must fail");
        assert!(err.to_string().contains("base_url"));
    }

    #[test]
    fn tool_builds_from_valid_config() {
        let config = ImageGenServiceConfig {
            base_url: Some("http://127.0.0.1:8317/v1".into()),
            ..Default::default()
        };
        assert!(GenerateImageTool::new(config).is_ok());
    }

    #[test]
    fn api_key_env_takes_precedence() {
        std::env::set_var("KK_TEST_IMAGEGEN_KEY", "from-env");
        let config = ImageGenServiceConfig {
            api_key: Some("inline".into()),
            api_key_env: Some("KK_TEST_IMAGEGEN_KEY".into()),
            ..Default::default()
        };
        assert_eq!(config.api_key().as_deref(), Some("from-env"));
        std::env::remove_var("KK_TEST_IMAGEGEN_KEY");
    }

    #[test]
    fn sniffs_mimes() {
        assert_eq!(
            sniff_image_mime(&[0x89, b'P', b'N', b'G', 0, 0]),
            "image/png"
        );
        assert_eq!(sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
        assert_eq!(
            sniff_image_mime(&[0, 0, 0, 0, 0, 0, 0, 0, b'W', b'E', b'B', b'P']),
            "image/webp"
        );
        assert_eq!(sniff_image_mime(b"garbage"), "image/png");
    }

    #[test]
    fn suffix_insertion_keeps_extension() {
        assert_eq!(insert_index_suffix("out/img.png", 0), "out/img-0.png");
        assert_eq!(insert_index_suffix("out/img", 2), "out/img-2");
        assert_eq!(insert_index_suffix(".hidden", 1), ".hidden-1");
    }
}
