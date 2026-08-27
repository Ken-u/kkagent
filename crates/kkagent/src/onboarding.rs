use anyhow::{Context, Result};
use kkagent_config::{
    load_config, load_workspace_dotenv, AppConfig, BackgroundConfig, ModelConfig, ProviderConfig,
};
use kkagent_core::TranscriptDb;
use kkagent_tools::sandbox::{SandboxMode, SandboxPolicy};
use serde_json::{json, Value as JsonValue};
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use super::{write_private_config, ConfigCommands};

pub fn run_init(
    configured_path: Option<&Path>,
    preset: &str,
    provider_arg: Option<&str>,
    model_arg: Option<&str>,
    base_url_arg: Option<&str>,
    force: bool,
    non_interactive: bool,
) -> Result<()> {
    let _ = load_workspace_dotenv()?;
    let path = configured_path
        .map(Path::to_path_buf)
        .unwrap_or_else(kkagent_config::default_config_path);
    if path.exists() && !force {
        anyhow::bail!(
            "configuration already exists at {}; pass --force to replace it",
            path.display()
        );
    }

    let interactive = !non_interactive && io::stdin().is_terminal() && io::stdout().is_terminal();
    let inferred_provider = infer_provider_from_env();
    let provider = match provider_arg.or(inferred_provider) {
        Some(value) => normalize_provider(value)?,
        None if interactive => prompt_provider()?,
        None => anyhow::bail!(
            "provider is required in non-interactive mode; pass --provider or set a supported API key"
        ),
    };
    let model = match model_arg
        .map(str::to_owned)
        .or_else(|| std::env::var("KKAGENT_MODEL_ID").ok())
    {
        Some(value) if !value.trim().is_empty() => value,
        _ if interactive => prompt_required("Model id")?,
        _ => anyhow::bail!("model is required in non-interactive mode; pass --model"),
    };
    let base_url = match base_url_arg.map(str::to_owned) {
        Some(value) => Some(value),
        None if provider == "custom" && interactive => Some(prompt_required("Base URL")?),
        None if provider == "custom" => anyhow::bail!("custom provider requires --base-url"),
        None => None,
    };
    let api_key = provider_api_key(&provider).or_else(|| {
        if interactive {
            rpassword::prompt_password("API key (leave blank to use the environment later): ")
                .ok()
                .filter(|value| !value.is_empty())
        } else {
            None
        }
    });

    let provider_name = if provider == "custom" {
        "custom".to_owned()
    } else {
        provider.clone()
    };
    let alias = format!("{provider_name}/{model}");
    let mut config = AppConfig {
        default_model: Some(alias.clone()),
        ..AppConfig::default()
    };
    config.providers.insert(
        provider_name.clone(),
        ProviderConfig {
            provider_type: provider_type(&provider).to_owned(),
            api_key,
            api_key_env: None,
            base_url,
            custom_headers: HashMap::new(),
            oauth: None,
            first_token_timeout_ms: None,
            extra_fields: Default::default(),
        },
    );
    config.models.insert(
        alias,
        ModelConfig {
            provider: provider_name,
            model,
            max_context_size: None,
            max_output_size: None,
            capabilities: vec!["tool_use".into()],
            display_name: None,
            support_efforts: Vec::new(),
            default_effort: None,
            pricing: None,
            experimental_adaptive_thinking: false,
            experimental_vision_proxy: false,
            experimental_visible_empty_retries: 0,
            experimental_bad_toolcall_auto_retries: 0,
            first_token_timeout_ms: None,
        },
    );
    apply_preset(&mut config, preset)?;
    config.validate()?;
    write_config(&path, &config)?;
    println!("Wrote {}", path.display());
    println!("Run `kkagent doctor` to verify the installation, then run `kkagent`.");
    Ok(())
}

pub fn run_config(command: &ConfigCommands, configured_path: Option<&Path>) -> Result<()> {
    let path = configured_path
        .map(Path::to_path_buf)
        .unwrap_or_else(kkagent_config::default_config_path);
    match command {
        ConfigCommands::Show => {
            let config = load_config(Some(&path))?;
            let mut value = serde_json::to_value(config)?;
            redact_json(&mut value, None);
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        ConfigCommands::Get { key } => {
            let config = load_config(Some(&path))?;
            let mut value = serde_json::to_value(config)?;
            redact_json(&mut value, None);
            let selected = json_get(&value, key)
                .with_context(|| format!("configuration key {key:?} does not exist"))?;
            if let Some(text) = selected.as_str() {
                println!("{text}");
            } else {
                println!("{}", serde_json::to_string_pretty(selected)?);
            }
        }
        ConfigCommands::Set { key, value } => {
            let content = std::fs::read_to_string(&path).with_context(|| {
                format!(
                    "failed to read {}; run `kkagent init` first",
                    path.display()
                )
            })?;
            let mut root: toml::Value = toml::from_str(&content)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            let parsed: toml::Value = toml::from_str::<toml::Table>(&format!("value = {value}"))?
                .remove("value")
                .expect("literal wrapper always has value");
            toml_set(&mut root, key, parsed)?;
            let config: AppConfig = root.clone().try_into()?;
            config.validate()?;
            write_private_config(&path, toml::to_string_pretty(&root)?.as_bytes())?;
            println!("Updated {key} in {}", path.display());
        }
        ConfigCommands::Preset { name } => {
            let mut config = load_config(Some(&path))?;
            apply_preset(&mut config, name)?;
            config.validate()?;
            write_config(&path, &config)?;
            println!("Applied preset {name:?} to {}", path.display());
        }
        ConfigCommands::Migrate { apply } => {
            let preview = kkagent_config::preview_migration(&path)?;
            println!("Config: {}", preview.path.display());
            println!("Backup would be: {}", preview.backup_path.display());
            for change in &preview.changes {
                println!("- {change}");
            }
            if *apply {
                if !path.exists() {
                    anyhow::bail!("nothing to migrate; run `kkagent init` first");
                }
                let raw = std::fs::read_to_string(&path)?;
                // Round-trip through typed config then pretty TOML (comments may be lost —
                // backup is required first).
                let cfg: AppConfig = toml::from_str(&raw)?;
                cfg.validate()?;
                let body = toml::to_string_pretty(&cfg)?;
                let bak = kkagent_config::atomic_write_with_backup(&path, &body)?;
                println!("Applied migration; backup at {}", bak.display());
            } else {
                println!("Dry-run only. Re-run with --apply to write after backup.");
            }
        }
        ConfigCommands::Repair { apply } => {
            let db = TranscriptDb::open_default()?;
            let report = if *apply {
                let bak = kkagent_config::default_config_dir().join(format!(
                    "transcripts.repair-{}.db",
                    chrono::Utc::now().format("%Y%m%d%H%M%S")
                ));
                println!(
                    "Backing up then quarantining corrupt rows → {}",
                    bak.display()
                );
                db.repair_with_backup(&bak)?
            } else {
                db.check_integrity()?
            };
            println!(
                "sessions ok={} bad={}",
                report.ok_sessions.len(),
                report.bad_sessions.len()
            );
            println!(
                "messages ok={} isolated={} repaired={}",
                report.ok_messages,
                report.isolated_messages.len(),
                report.repaired
            );
            for bad in &report.bad_sessions {
                println!("  session issue: {bad}");
            }
            for iso in report.isolated_messages.iter().take(20) {
                println!(
                    "  msg #{} session={} reason={}",
                    iso.id, iso.session_id, iso.reason
                );
            }
            if !*apply && !report.isolated_messages.is_empty() {
                println!("Dry-run only. Re-run with --apply to quarantine after backup.");
            }
        }
    }
    Ok(())
}

pub async fn run_doctor(configured_path: Option<&Path>, as_json: bool, live: bool) -> Result<()> {
    let path = configured_path
        .map(Path::to_path_buf)
        .unwrap_or_else(kkagent_config::default_config_path);
    let mut checks = Vec::new();
    let dotenv = load_workspace_dotenv();
    match dotenv {
        Ok(Some(path)) => checks.push(check(
            "dotenv",
            "ok",
            format!("found {}", path.display()),
            None,
        )),
        Ok(None) => checks.push(check("dotenv", "ok", "no workspace .env (optional)", None)),
        Err(error) => checks.push(check(
            "dotenv",
            "fail",
            error.to_string(),
            Some("fix or remove the malformed workspace .env"),
        )),
    }

    let config = if !path.is_file() {
        checks.push(check(
            "config",
            "fail",
            format!("{} does not exist", path.display()),
            Some("run `kkagent init`"),
        ));
        None
    } else {
        match load_config(Some(&path)) {
            Ok(config) => {
                checks.push(check(
                    "config",
                    "ok",
                    format!("{} is valid", path.display()),
                    None,
                ));
                Some(config)
            }
            Err(error) => {
                checks.push(check(
                    "config",
                    "fail",
                    error.to_string(),
                    Some("run `kkagent config show` and correct the reported field"),
                ));
                None
            }
        }
    };

    let cwd = std::env::current_dir()?;
    match std::fs::canonicalize(&cwd) {
        Ok(path) => checks.push(check(
            "workspace",
            "ok",
            format!("{} is accessible", path.display()),
            None,
        )),
        Err(error) => checks.push(check(
            "workspace",
            "fail",
            error.to_string(),
            Some("choose an accessible working directory"),
        )),
    }
    for tool in ["git", "rg"] {
        if find_on_path(tool).is_some() {
            checks.push(check(tool, "ok", "available on PATH", None));
        } else {
            checks.push(check(
                tool,
                "warn",
                "not found on PATH",
                Some(&format!(
                    "install {tool} for the best coding-agent experience"
                )),
            ));
        }
    }
    let shell_ok = if cfg!(windows) {
        find_on_path("pwsh").is_some() || find_on_path("powershell").is_some()
    } else {
        Path::new("/bin/sh").is_file()
    };
    checks.push(if shell_ok {
        check("shell", "ok", "command shell is available", None)
    } else {
        check(
            "shell",
            "fail",
            "no supported command shell found",
            Some("install PowerShell or provide /bin/sh"),
        )
    });

    let data_dir = kkagent_config::default_config_dir();
    match writable_probe(&data_dir) {
        Ok(()) => checks.push(check(
            "storage",
            "ok",
            format!("{} is writable", data_dir.display()),
            None,
        )),
        Err(error) => checks.push(check(
            "storage",
            "fail",
            error.to_string(),
            Some("fix directory ownership and write permissions"),
        )),
    }

    if let Some(config) = &config {
        doctor_model(config, &mut checks);
        doctor_sandbox(config, &mut checks);
        if live {
            checks.push(probe_provider(config).await);
        }
    }

    let failures = checks
        .iter()
        .filter(|item| item["status"] == "fail")
        .count();
    let warnings = checks
        .iter()
        .filter(|item| item["status"] == "warn")
        .count();
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "healthy": failures == 0,
                "summary": { "passed": checks.len() - failures - warnings, "warnings": warnings, "failures": failures },
                "checks": checks,
            }))?
        );
    } else {
        for item in &checks {
            println!(
                "[{:<4}] {:<12} {}",
                item["status"].as_str().unwrap_or("?"),
                item["name"].as_str().unwrap_or("?"),
                item["message"].as_str().unwrap_or("")
            );
            if let Some(fix) = item["fix"].as_str() {
                println!("       fix: {fix}");
            }
        }
        println!(
            "\n{} passed, {warnings} warnings, {failures} failures",
            checks.len() - failures - warnings
        );
    }
    if failures > 0 {
        anyhow::bail!("doctor found {failures} blocking issue(s)");
    }
    Ok(())
}

fn normalize_provider(value: &str) -> Result<String> {
    let normalized = value.trim().to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "openai" | "anthropic" | "kimi" | "google" | "custom"
    ) {
        Ok(normalized)
    } else {
        anyhow::bail!(
            "unsupported provider {value:?}; expected openai, anthropic, kimi, google, or custom"
        )
    }
}

fn provider_type(provider: &str) -> &str {
    match provider {
        "anthropic" => "anthropic",
        "kimi" => "kimi",
        "google" => "google",
        _ => "openai-responses",
    }
}

fn infer_provider_from_env() -> Option<&'static str> {
    [
        ("OPENAI_API_KEY", "openai"),
        ("ANTHROPIC_API_KEY", "anthropic"),
        ("KIMI_API_KEY", "kimi"),
        ("MOONSHOT_API_KEY", "kimi"),
        ("GOOGLE_API_KEY", "google"),
    ]
    .into_iter()
    .find_map(|(key, provider)| {
        std::env::var_os(key)
            .filter(|v| !v.is_empty())
            .map(|_| provider)
    })
}

fn provider_api_key(provider: &str) -> Option<String> {
    let keys: &[&str] = match provider {
        "openai" => &["OPENAI_API_KEY"],
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "kimi" => &["KIMI_API_KEY", "MOONSHOT_API_KEY"],
        "google" => &["GOOGLE_API_KEY"],
        _ => &[],
    };
    keys.iter()
        .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
}

fn prompt_provider() -> Result<String> {
    println!("Provider: 1) OpenAI  2) Anthropic  3) Kimi  4) Google  5) Custom");
    let value = prompt_optional("Select provider [1]")?;
    normalize_provider(match value.as_str() {
        "" | "1" => "openai",
        "2" => "anthropic",
        "3" => "kimi",
        "4" => "google",
        "5" => "custom",
        other => other,
    })
}

fn prompt_required(label: &str) -> Result<String> {
    loop {
        let value = prompt_optional(label)?;
        if !value.trim().is_empty() {
            return Ok(value.trim().to_owned());
        }
        eprintln!("A value is required.");
    }
}

fn prompt_optional(label: &str) -> Result<String> {
    print!("{label}: ");
    io::stdout().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_owned())
}

fn apply_preset(config: &mut AppConfig, name: &str) -> Result<()> {
    config.sandbox.mode = "auto".into();
    let background = config.background.get_or_insert(BackgroundConfig {
        max_running_tasks: 4,
        keep_alive_on_exit: false,
        bash_auto_background_on_timeout: None,
        bash_task_timeout_s: None,
        approval_timeout_s: None,
    });
    match name.trim().to_ascii_lowercase().as_str() {
        "safe" => {
            config.default_permission_mode = Some("manual".into());
            config.sandbox.network = false;
            background.bash_auto_background_on_timeout = Some(false);
        }
        "default" => {
            config.default_permission_mode = Some("manual".into());
            config.sandbox.network = true;
            background.bash_auto_background_on_timeout = Some(true);
        }
        "full-auto" | "full_auto" | "auto" => {
            config.default_permission_mode = Some("auto".into());
            config.sandbox.network = true;
            background.bash_auto_background_on_timeout = Some(true);
        }
        other => anyhow::bail!("unknown preset {other:?}; expected safe, default, or full-auto"),
    }
    Ok(())
}

fn write_config(path: &Path, config: &AppConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_private_config(path, toml::to_string_pretty(config)?.as_bytes())
}

fn redact_json(value: &mut JsonValue, parent_key: Option<&str>) {
    if !value.is_null() && parent_key.is_some_and(is_secret_key) {
        *value = JsonValue::String("<redacted>".into());
        return;
    }
    match value {
        JsonValue::Object(map) => {
            for (key, value) in map {
                redact_json(value, Some(key));
            }
        }
        JsonValue::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_json(item, parent_key)),
        _ => {}
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("api_key")
        || key.contains("token")
        || key.contains("secret")
        || key == "custom_headers"
        || key == "headers"
        || key == "env"
}

fn json_get<'a>(value: &'a JsonValue, key: &str) -> Option<&'a JsonValue> {
    key.split('.')
        .try_fold(value, |current, segment| current.get(segment))
}

fn toml_set(root: &mut toml::Value, key: &str, value: toml::Value) -> Result<()> {
    let segments: Vec<_> = key.split('.').collect();
    if segments.is_empty() || segments.len() > 16 || segments.iter().any(|part| part.is_empty()) {
        anyhow::bail!("invalid dotted configuration key {key:?}");
    }
    let mut current = root;
    for segment in &segments[..segments.len() - 1] {
        let table = current
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("{segment:?} is not a table"))?;
        current = table
            .entry((*segment).to_owned())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    }
    let table = current
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("parent of {key:?} is not a table"))?;
    table.insert(segments.last().unwrap().to_string(), value);
    Ok(())
}

fn check(name: &str, status: &str, message: impl Into<String>, fix: Option<&str>) -> JsonValue {
    json!({ "name": name, "status": status, "message": message.into(), "fix": fix })
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).find_map(|directory| {
            candidates
                .iter()
                .map(|suffix| directory.join(format!("{name}{suffix}")))
                .find(|candidate| candidate.is_file())
        })
    })
}

fn writable_probe(directory: &Path) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    let probe = directory.join(format!(".doctor-{}", uuid::Uuid::new_v4()));
    std::fs::write(&probe, b"ok")?;
    std::fs::remove_file(&probe)?;
    Ok(())
}

fn doctor_model(config: &AppConfig, checks: &mut Vec<JsonValue>) {
    // Provider-level diagnostics: unknown keys (typo / key landed under the
    // wrong TOML table) and `api_key_env` variables that fail to resolve.
    for (name, provider) in &config.providers {
        for key in provider.extra_fields.keys() {
            checks.push(check(
                "provider",
                "warn",
                format!(
                    "providers.{name}: unknown configuration key {key:?} — likely a typo, or \
                     the key belongs to the next TOML table (keys are assigned to the nearest \
                     preceding table header)"
                ),
                Some("move the key under the intended [providers.*] header"),
            ));
        }
        if let Some(env_name) = provider.api_key_env.as_deref() {
            match std::env::var(env_name) {
                Ok(value) if value.trim().is_empty() => checks.push(check(
                    "provider",
                    "warn",
                    format!("providers.{name}: api_key_env={env_name:?} is set but empty"),
                    Some("set a non-empty value for the variable"),
                )),
                Ok(_) => {}
                Err(std::env::VarError::NotPresent) => checks.push(check(
                    "provider",
                    "warn",
                    format!(
                        "providers.{name}: api_key_env={env_name:?} is not set in the \
                         environment"
                    ),
                    Some("export the variable before starting kkagent"),
                )),
                Err(std::env::VarError::NotUnicode(_)) => checks.push(check(
                    "provider",
                    "warn",
                    format!("providers.{name}: api_key_env={env_name:?} holds a non-UTF-8 value"),
                    Some("fix the value of the variable to be valid UTF-8"),
                )),
            }
        }
    }

    let Some(alias) = config.default_model.as_deref() else {
        return;
    };
    let Some((model, provider)) = config.resolve_model(alias) else {
        return;
    };
    checks.push(check(
        "model",
        "ok",
        format!("{alias} -> {}", model.model),
        None,
    ));
    let has_auth = provider
        .api_key
        .as_deref()
        .is_some_and(|value| !value.is_empty())
        || provider.oauth.is_some();
    checks.push(if has_auth {
        check("credentials", "ok", format!("credentials configured for provider {}", model.provider), None)
    } else {
        check("credentials", "warn", format!("no credentials found for provider {}", model.provider), Some("set the provider API key in .env, the environment, or config; Kimi also supports `kkagent auth login`"))
    });
}

fn doctor_sandbox(config: &AppConfig, checks: &mut Vec<JsonValue>) {
    match SandboxPolicy::from_config(&config.sandbox) {
        Err(error) => checks.push(check(
            "sandbox",
            "fail",
            error.to_string(),
            Some("correct [sandbox] limits or mode"),
        )),
        Ok(policy) => match policy.mode {
            SandboxMode::Workspace
                if cfg!(target_os = "linux") && find_on_path("bwrap").is_none() =>
            {
                checks.push(check(
                    "sandbox",
                    "fail",
                    "workspace isolation needs bubblewrap (bwrap)",
                    Some("install bubblewrap or set sandbox.mode = \"process\""),
                ))
            }
            SandboxMode::Workspace
                if cfg!(target_os = "macos") && !Path::new("/usr/bin/sandbox-exec").is_file() =>
            {
                checks.push(check(
                    "sandbox",
                    "fail",
                    "workspace isolation needs /usr/bin/sandbox-exec",
                    Some("set sandbox.mode = \"process\" on systems without sandbox-exec"),
                ))
            }
            _ => checks.push(check(
                "sandbox",
                "ok",
                format!(
                    "{} isolation, network={}",
                    policy.mode_name(),
                    policy.network
                ),
                None,
            )),
        },
    }
    doctor_toolchain(config, checks);
}

fn doctor_toolchain(config: &AppConfig, checks: &mut Vec<JsonValue>) {
    let tc = &config.toolchain;
    if !tc.enabled {
        checks.push(check(
            "toolchain",
            "ok",
            "toolchain sandbox disabled",
            Some("set [toolchain] enabled = true to isolate language caches"),
        ));
        return;
    }
    let report = kkagent_tools::doctor_report(tc);
    // Warn when agent-owned caches approach the configured quota.
    let cache_bytes: u64 = tc
        .all_resolved()
        .iter()
        .flat_map(|p| p.agent_cache_read_write.iter())
        .map(|p| dir_size_approx(p))
        .sum();
    let status = if cache_bytes > tc.max_cache_bytes.saturating_mul(9) / 10 {
        "warn"
    } else {
        "ok"
    };
    let hint = if status == "warn" {
        Some(&format!(
            "prune {} (≈{} MiB of {} MiB quota)",
            tc.cache_root().display(),
            cache_bytes / (1024 * 1024),
            tc.max_cache_bytes / (1024 * 1024)
        ))
    } else {
        None
    };
    checks.push(check("toolchain", status, report, hint.map(String::as_str)));
}

/// Best-effort on-disk size of `path` (depth-limited, non-fatal on errors).
fn dir_size_approx(path: &Path) -> u64 {
    fn walk(path: &Path, depth: u32, total: &mut u64) {
        if depth > 6 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_file() {
                *total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
            } else if file_type.is_dir() {
                walk(&entry.path(), depth + 1, total);
            }
        }
    }
    let mut total = 0u64;
    walk(path, 0, &mut total);
    total
}

async fn probe_provider(config: &AppConfig) -> JsonValue {
    let Some(alias) = config.default_model.as_deref() else {
        return check("provider-live", "fail", "default model is missing", None);
    };
    let Some((_, provider)) = config.resolve_model(alias) else {
        return check("provider-live", "fail", "default provider is missing", None);
    };
    let base = provider
        .base_url
        .as_deref()
        .unwrap_or(match provider.provider_type.as_str() {
            "anthropic" => "https://api.anthropic.com/v1",
            "kimi" => "https://api.moonshot.cn/v1",
            "google" | "google-genai" | "gemini" => {
                "https://generativelanguage.googleapis.com/v1beta"
            }
            _ => "https://api.openai.com/v1",
        })
        .trim_end_matches('/');
    let url = format!("{base}/models");
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
    {
        Ok(client) => client,
        Err(error) => return check("provider-live", "fail", error.to_string(), None),
    };
    let mut request = client.get(&url);
    if let Some(key) = provider.api_key.as_deref() {
        if provider.provider_type == "anthropic" {
            request = request
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01");
        } else if matches!(
            provider.provider_type.as_str(),
            "google" | "google-genai" | "gemini"
        ) {
            request = request.query(&[("key", key)]);
        } else {
            request = request.bearer_auth(key);
        }
    }
    match request.send().await {
        Ok(response) if response.status().is_success() => check(
            "provider-live",
            "ok",
            format!("{} responded successfully", response.url()),
            None,
        ),
        Ok(response) => check(
            "provider-live",
            "fail",
            format!("{} returned HTTP {}", response.url(), response.status()),
            Some("verify the API key, base URL, and provider account"),
        ),
        Err(error) => check(
            "provider-live",
            "fail",
            error.to_string(),
            Some("check DNS, proxy, firewall, and provider base URL"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_change_only_runtime_defaults() {
        let mut config = AppConfig::default();
        apply_preset(&mut config, "safe").unwrap();
        assert_eq!(config.effective_permission_mode(), "manual");
        assert!(!config.sandbox.network);
        apply_preset(&mut config, "full-auto").unwrap();
        assert_eq!(config.effective_permission_mode(), "auto");
        assert!(config.sandbox.network);
    }

    #[test]
    fn dotted_toml_set_creates_tables() {
        let mut value = toml::Value::Table(toml::Table::new());
        toml_set(&mut value, "sandbox.network", toml::Value::Boolean(false)).unwrap();
        assert_eq!(value["sandbox"]["network"].as_bool(), Some(false));
    }

    #[test]
    fn redaction_covers_nested_secrets() {
        let mut value = json!({"providers": {"x": {"api_key": "secret", "base_url": "safe"}}});
        redact_json(&mut value, None);
        assert_eq!(value["providers"]["x"]["api_key"], "<redacted>");
        assert_eq!(value["providers"]["x"]["base_url"], "safe");
    }
}
