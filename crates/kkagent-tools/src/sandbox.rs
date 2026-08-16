use std::path::{Path, PathBuf};
use tokio::process::Command;

#[cfg(windows)]
pub struct SandboxProcessGuard(Option<windows_sys::Win32::Foundation::HANDLE>);

#[cfg(windows)]
unsafe impl Send for SandboxProcessGuard {}

#[cfg(windows)]
unsafe impl Sync for SandboxProcessGuard {}

#[cfg(not(windows))]
pub struct SandboxProcessGuard;

#[cfg(windows)]
impl Drop for SandboxProcessGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0 {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    Disabled,
    Process,
    Workspace,
}

/// Environment variables forwarded into workspace-mode sandboxes.
/// Keep this list minimal — secrets loaded into the kkagent process must not leak.
pub const WORKSPACE_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "TERM",
    "TZ",
    "LANG",
    "LANGUAGE",
    "TMPDIR",
    "TEMP",
    "TMP",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "KKAGENT_SANDBOX",
];

#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    /// Configured mode string before auto-resolution (`auto` / `workspace` / …).
    pub configured_mode: String,
    pub network: bool,
    pub memory_mb: u64,
    pub cpu_seconds: u64,
    pub max_processes: u32,
    pub extra_read_paths: Vec<PathBuf>,
    pub extra_write_paths: Vec<PathBuf>,
    pub allow_sensitive_extra_paths: bool,
    /// Additional read-only bind roots for the Linux bwrap sandbox
    /// (`sandbox.system_read_paths`, `~` expanded).
    pub system_read_paths: Vec<PathBuf>,
    /// Set when `auto` fell back because workspace tooling was missing.
    pub auto_fallback_warning: Option<String>,
    /// Toolchain-derived mounts/env applied in workspace mode.
    pub toolchain_overlay: crate::toolchain::ToolchainSandboxOverlay,
    workspace_trust: Vec<kkagent_config::WorkspaceTrust>,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            mode: SandboxMode::Process,
            configured_mode: "process".into(),
            network: false,
            memory_mb: 4096,
            cpu_seconds: 600,
            max_processes: 128,
            extra_read_paths: Vec::new(),
            extra_write_paths: Vec::new(),
            allow_sensitive_extra_paths: false,
            system_read_paths: Vec::new(),
            auto_fallback_warning: None,
            toolchain_overlay: crate::toolchain::ToolchainSandboxOverlay::default(),
            workspace_trust: Vec::new(),
        }
    }
}

impl SandboxPolicy {
    pub fn from_config(config: &kkagent_config::SandboxConfig) -> anyhow::Result<Self> {
        config.validate_extra_paths()?;
        let configured = config.mode.trim().to_ascii_lowercase();
        let mut auto_fallback_warning = None;
        let mode = match configured.as_str() {
            "auto" => resolve_auto_mode(&mut auto_fallback_warning),
            "disabled" | "off" | "none" => SandboxMode::Disabled,
            "process" => SandboxMode::Process,
            "workspace" | "strict" => SandboxMode::Workspace,
            other => anyhow::bail!(
                "invalid sandbox.mode {other:?}; expected auto, disabled, process, or workspace"
            ),
        };
        if config.memory_mb > 0 && config.memory_mb < 64 {
            anyhow::bail!("sandbox memory_mb must be zero (unlimited) or at least 64");
        }
        if mode != SandboxMode::Disabled
            && (config.memory_mb == 0 || config.cpu_seconds == 0 || config.max_processes == 0)
        {
            anyhow::bail!("sandbox limits must be positive and memory_mb must be at least 64");
        }
        if let Some(warning) = auto_fallback_warning.as_ref() {
            tracing::warn!("{warning}");
        }
        Ok(Self {
            mode,
            configured_mode: configured,
            network: config.network,
            memory_mb: config.memory_mb,
            cpu_seconds: config.cpu_seconds,
            max_processes: config.max_processes,
            extra_read_paths: config
                .extra_read_paths
                .iter()
                .map(|raw| kkagent_config::expand_user_path(raw))
                .collect(),
            extra_write_paths: config
                .extra_write_paths
                .iter()
                .map(|raw| kkagent_config::expand_user_path(raw))
                .collect(),
            allow_sensitive_extra_paths: config.allow_sensitive_extra_paths,
            system_read_paths: config
                .system_read_paths
                .iter()
                .map(|raw| kkagent_config::expand_user_path(raw))
                .collect(),
            auto_fallback_warning,
            toolchain_overlay: crate::toolchain::ToolchainSandboxOverlay::default(),
            workspace_trust: Vec::new(),
        })
    }

    pub fn from_app_config(config: &kkagent_config::AppConfig) -> anyhow::Result<Self> {
        let mut policy = Self::from_config(&config.sandbox)?;
        policy.workspace_trust = config.workspace_trust.workspaces.clone();
        policy.toolchain_overlay =
            crate::toolchain::toolchain_sandbox_overlay(&config.toolchain, &[]);
        if policy.toolchain_overlay.force_network {
            policy.network = true;
        }
        Ok(policy)
    }

    pub fn refresh_toolchain(
        &mut self,
        config: &kkagent_config::ToolchainConfig,
        grants: &[crate::toolchain::ToolchainGrant],
    ) {
        self.toolchain_overlay = crate::toolchain::toolchain_sandbox_overlay(config, grants);
        if self.toolchain_overlay.force_network {
            self.network = true;
        }
    }

    pub fn upsert_workspace_trust(
        &mut self,
        trust: kkagent_config::WorkspaceTrust,
    ) -> anyhow::Result<()> {
        trust.validate()?;
        let workspace = canonical_or_owned(&trust.workspace_path());
        if let Some(existing) = self
            .workspace_trust
            .iter_mut()
            .find(|item| canonical_or_owned(&item.workspace_path()) == workspace)
        {
            *existing = trust;
        } else {
            self.workspace_trust.push(trust);
        }
        Ok(())
    }

    /// Build a sandboxed command. `session_root` is the session working directory
    /// (writable root in workspace mode). Prefer this over [`Self::command`].
    pub fn command_for_session(
        &self,
        shell: &str,
        flag: &str,
        script: &str,
        cwd: &Path,
        session_root: &Path,
    ) -> anyhow::Result<Command> {
        let (cwd, writable_root) = self.resolve_workspace_paths(cwd, session_root)?;
        let trust = self.workspace_trust_for(&writable_root);
        let mut command = match self.mode {
            SandboxMode::Disabled | SandboxMode::Process => shell_command(shell, flag, script),
            SandboxMode::Workspace => {
                workspace_command(self, trust, shell, flag, script, &cwd, &writable_root)?
            }
        };
        command.current_dir(&cwd);
        if self.mode == SandboxMode::Workspace {
            apply_workspace_env_whitelist(&mut command);
            #[cfg(unix)]
            command.env("HOME", "/tmp");
            #[cfg(windows)]
            command.env("HOME", std::env::temp_dir());
            for (key, value) in &self.toolchain_overlay.env {
                command.env(key, value);
            }
        }
        command.env("KKAGENT_SANDBOX", self.mode_name());
        apply_git_environment(&mut command, self.mode, trust);
        apply_resource_limits(&mut command, self)?;
        Ok(command)
    }

    /// Convenience wrapper that treats `cwd` as both the chdir target and the
    /// session writable root (tests / simple callers).
    pub fn command(
        &self,
        shell: &str,
        flag: &str,
        script: &str,
        cwd: &Path,
    ) -> anyhow::Result<Command> {
        self.command_for_session(shell, flag, script, cwd, cwd)
    }

    /// Resolve and validate `cwd` against the session writable root.
    pub fn resolve_workspace_paths(
        &self,
        cwd: &Path,
        session_root: &Path,
    ) -> anyhow::Result<(PathBuf, PathBuf)> {
        let session_root = match std::fs::canonicalize(session_root) {
            Ok(p) => p,
            Err(error) => {
                if self.mode == SandboxMode::Workspace {
                    anyhow::bail!(
                        "cannot resolve session working dir {}: {error}",
                        session_root.display()
                    );
                }
                return Ok((cwd.to_path_buf(), session_root.to_path_buf()));
            }
        };
        let cwd = match std::fs::canonicalize(cwd) {
            Ok(p) => p,
            Err(error) => {
                let kind = match self.mode {
                    SandboxMode::Workspace => "sandbox cwd",
                    SandboxMode::Process => "process cwd",
                    SandboxMode::Disabled => "working directory",
                };
                anyhow::bail!("cannot resolve {kind} {}: {error}", cwd.display());
            }
        };

        if self.mode != SandboxMode::Workspace {
            return Ok((cwd, session_root));
        }

        let writable_root = self.writable_root_for(&session_root);
        if path_is_within(&cwd, &writable_root) {
            return Ok((cwd, writable_root));
        }

        // Ancestor of session root is allowed only when listed in extra_write_paths.
        if path_is_within(&writable_root, &cwd) {
            let cwd_allowed = self.extra_write_paths.iter().any(|p| {
                let extra = canonical_or_owned(p);
                cwd == extra || path_is_within(&cwd, &extra)
            });
            if cwd_allowed {
                return Ok((cwd, writable_root));
            }
        }

        anyhow::bail!(
            "cwd `{}` escapes the sandbox writable root `{}`. \
Use a path inside the session working directory (relative paths preferred).",
            cwd.display(),
            writable_root.display()
        );
    }

    fn writable_root_for(&self, session_root: &Path) -> PathBuf {
        // Prefer the more precise (deeper) of session root vs matching trust workspace.
        if let Some(trust) = self.workspace_trust_for(session_root) {
            let trust_root = canonical_or_owned(&trust.workspace_path());
            if path_is_within(session_root, &trust_root) {
                // session is inside trust → session is more precise
                return session_root.to_path_buf();
            }
        }
        session_root.to_path_buf()
    }

    pub fn mode_name(&self) -> &'static str {
        match self.mode {
            SandboxMode::Disabled => "disabled",
            SandboxMode::Process => "process",
            SandboxMode::Workspace => "workspace",
        }
    }

    fn workspace_trust_for(&self, cwd: &Path) -> Option<&kkagent_config::WorkspaceTrust> {
        self.workspace_trust
            .iter()
            .filter(|entry| cwd.starts_with(canonical_or_owned(&entry.workspace_path())))
            .max_by_key(|entry| {
                canonical_or_owned(&entry.workspace_path())
                    .components()
                    .count()
            })
    }

    pub fn contain_child(
        &self,
        child: &tokio::process::Child,
    ) -> anyhow::Result<SandboxProcessGuard> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::*;
            use windows_sys::Win32::System::Threading::{
                OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
            };
            if self.mode == SandboxMode::Disabled {
                // `disabled` skips Job Object containment entirely; the child
                // is spawned without CREATE_SUSPENDED, so there is nothing to
                // resume either.
                return Ok(SandboxProcessGuard(None));
            }
            if self.memory_mb == 0 && self.max_processes == 0 {
                return Ok(SandboxProcessGuard(None));
            }
            let pid = child
                .id()
                .ok_or_else(|| anyhow::anyhow!("spawned process has no id"))?;
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(std::io::Error::last_os_error().into());
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if self.max_processes > 0 {
                    info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
                    info.BasicLimitInformation.ActiveProcessLimit = self.max_processes;
                }
                if self.memory_mb > 0 {
                    info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
                    info.ProcessMemoryLimit = self.memory_mb.saturating_mul(1024 * 1024) as usize;
                }
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const std::ffi::c_void,
                    std::mem::size_of_val(&info) as u32,
                ) == 0
                {
                    windows_sys::Win32::Foundation::CloseHandle(job);
                    return Err(std::io::Error::last_os_error().into());
                }
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
                if process.is_null() {
                    windows_sys::Win32::Foundation::CloseHandle(job);
                    return Err(std::io::Error::last_os_error().into());
                }
                let assigned = AssignProcessToJobObject(job, process);
                windows_sys::Win32::Foundation::CloseHandle(process);
                if assigned == 0 {
                    windows_sys::Win32::Foundation::CloseHandle(job);
                    return Err(std::io::Error::last_os_error().into());
                }
                if let Err(error) = resume_process_thread(pid) {
                    windows_sys::Win32::Foundation::CloseHandle(job);
                    return Err(error);
                }
                Ok(SandboxProcessGuard(Some(job)))
            }
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Ok(SandboxProcessGuard)
        }
    }
}

fn shell_command(shell: &str, flag: &str, script: &str) -> Command {
    let mut command = Command::new(shell);
    command.arg(flag).arg(script);
    command
}

#[cfg(target_os = "macos")]
fn workspace_command(
    policy: &SandboxPolicy,
    trust: Option<&kkagent_config::WorkspaceTrust>,
    shell: &str,
    flag: &str,
    script: &str,
    cwd: &Path,
    writable_root: &Path,
) -> anyhow::Result<Command> {
    let sandbox = Path::new("/usr/bin/sandbox-exec");
    if !sandbox.is_file() {
        anyhow::bail!("workspace sandbox requires /usr/bin/sandbox-exec on macOS");
    }
    let profile = macos_profile(policy, trust, cwd, writable_root)?;
    let mut command = Command::new(sandbox);
    command.args(["-p", &profile, shell, flag, script]);
    Ok(command)
}

#[cfg(target_os = "linux")]
fn workspace_command(
    policy: &SandboxPolicy,
    trust: Option<&kkagent_config::WorkspaceTrust>,
    shell: &str,
    flag: &str,
    script: &str,
    cwd: &Path,
    writable_root: &Path,
) -> anyhow::Result<Command> {
    let bwrap = which::which("bwrap")
        .map_err(|_| anyhow::anyhow!("workspace sandbox requires bubblewrap (bwrap) on Linux"))?;
    let mut command = Command::new(bwrap);
    command.args(["--die-with-parent", "--new-session", "--unshare-all"]);
    if policy.network {
        command.arg("--share-net");
    }
    let mut ro_roots: Vec<PathBuf> = ["/bin", "/sbin", "/usr", "/lib", "/lib64", "/etc", "/opt"]
        .iter()
        .map(PathBuf::from)
        .collect();
    // Interpreter location: if the shell lives outside the default roots
    // (NixOS `/nix/store`, homebrew `/opt/homebrew` covered by /opt, custom
    // toolchains), bind the *parent directory* of its realpath so the sandbox
    // can still exec it.
    if let Ok(real_shell) = std::fs::canonicalize(shell) {
        let covered = ro_roots.iter().any(|root| real_shell.starts_with(root));
        if !covered {
            if let Some(parent) = real_shell.parent() {
                ro_roots.push(parent.to_path_buf());
            }
        }
    }
    for path in ro_roots.iter().chain(policy.system_read_paths.iter()) {
        if path.is_dir() {
            command.args([
                "--ro-bind",
                &path.to_string_lossy(),
                &path.to_string_lossy(),
            ]);
        }
    }
    command.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
    // Writable root is the session workspace, not an attacker-controlled cwd.
    bind_path(&mut command, "--bind", writable_root)?;
    if cwd != writable_root && path_is_within(cwd, writable_root) {
        // cwd is already covered by the writable_root bind.
    } else if cwd != writable_root {
        bind_path(&mut command, "--bind", cwd)?;
    }
    for path in &policy.extra_read_paths {
        bind_path(&mut command, "--ro-bind", path)?;
    }
    for path in &policy.toolchain_overlay.extra_read {
        bind_path(&mut command, "--ro-bind", path)?;
    }
    for path in &policy.extra_write_paths {
        bind_path(&mut command, "--bind", path)?;
    }
    for path in &policy.toolchain_overlay.extra_write {
        bind_path(&mut command, "--bind", path)?;
    }
    if let Some(trust) = trust {
        if trust.global_git_config_allowed == Some(true) {
            for path in trust.global_git_read_paths().map(Path::new) {
                bind_trusted_path(&mut command, "--ro-bind", path, writable_root)?;
            }
        }
        if trust.git_metadata_allowed == Some(true) {
            for path in trust.git_metadata_paths.iter().map(Path::new) {
                bind_trusted_path(&mut command, "--bind", path, writable_root)?;
            }
        }
    }
    command.args([
        "--chdir",
        path_text(cwd)?,
        "--setenv",
        "HOME",
        "/tmp",
        "--",
        shell,
        flag,
        script,
    ]);
    Ok(command)
}

#[cfg(target_os = "windows")]
fn workspace_command(
    _policy: &SandboxPolicy,
    _trust: Option<&kkagent_config::WorkspaceTrust>,
    _shell: &str,
    _flag: &str,
    _script: &str,
    _cwd: &Path,
    _writable_root: &Path,
) -> anyhow::Result<Command> {
    anyhow::bail!("workspace filesystem sandbox is unavailable on Windows; use process mode or run kkagent in Windows Sandbox/WDAG")
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn workspace_command(
    _policy: &SandboxPolicy,
    _trust: Option<&kkagent_config::WorkspaceTrust>,
    _shell: &str,
    _flag: &str,
    _script: &str,
    _cwd: &Path,
    _writable_root: &Path,
) -> anyhow::Result<Command> {
    anyhow::bail!("workspace sandbox is unsupported on this platform")
}

#[cfg(target_os = "linux")]
fn bind_path(command: &mut Command, operation: &str, path: &Path) -> anyhow::Result<()> {
    let path = std::fs::canonicalize(path)
        .map_err(|error| anyhow::anyhow!("cannot bind sandbox path {}: {error}", path.display()))?;
    let text = path_text(&path)?;
    command.args([operation, text, text]);
    Ok(())
}

#[cfg(target_os = "linux")]
fn bind_trusted_path(
    command: &mut Command,
    operation: &str,
    path: &Path,
    cwd: &Path,
) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let path = std::fs::canonicalize(path)?;
    if path.starts_with(cwd) {
        return Ok(());
    }
    let text = path_text(&path)?;
    command.args([operation, text, text]);
    Ok(())
}

fn path_text(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("sandbox path is not valid UTF-8: {}", path.display()))
}

#[cfg(target_os = "macos")]
fn macos_profile(
    policy: &SandboxPolicy,
    trust: Option<&kkagent_config::WorkspaceTrust>,
    cwd: &Path,
    writable_root: &Path,
) -> anyhow::Result<String> {
    macos_profile_with_home(
        policy,
        trust,
        cwd,
        writable_root,
        dirs::home_dir().as_deref(),
    )
}

/// Credential directories that stay read-denied even if a broad allow rule
/// is ever introduced for HOME. Kept aligned with `path_policy`'s cloud/SSH
/// list. SBPL resolves equal-specificity conflicts by last-match-wins, so the
/// explicit `extra_read_paths` opt-in allows below still override these denies.
#[cfg(target_os = "macos")]
const DENIED_CREDENTIAL_SUBDIRS: &[&str] = &[".ssh", ".aws", ".gcp", ".kube", ".docker", ".gnupg"];

#[cfg(target_os = "macos")]
fn macos_profile_with_home(
    policy: &SandboxPolicy,
    trust: Option<&kkagent_config::WorkspaceTrust>,
    cwd: &Path,
    writable_root: &Path,
    home: Option<&Path>,
) -> anyhow::Result<String> {
    fn literal(path: &Path) -> anyhow::Result<String> {
        let value = path_text(path)?;
        Ok(format!(
            "\"{}\"",
            value.replace('\\', "\\\\").replace('"', "\\\"")
        ))
    }
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(allow process*)\n(allow sysctl-read)\n(allow mach-lookup)\n\
         (deny mach-lookup (global-name \"com.apple.SecurityServer\") \
         (global-name \"com.apple.securityd\"))\n\
         (allow file-write* (subpath \"/private/tmp\") (subpath \"/tmp\") (subpath \"/dev\"))\n",
    );
    if let Some(home) = home {
        let home_canon = std::fs::canonicalize(home)?;
        let home_path = home_canon.as_path();
        let home = literal(home_path)?;
        // Defense in depth: explicitly deny reads of credential directories
        // before any allow rules. The HOME exclusion below already blocks
        // them in practice; these denies keep that true even if rule ordering
        // or the HOME exclusion is ever relaxed. No existence probe — the
        // deny must also cover files created later.
        for dir in DENIED_CREDENTIAL_SUBDIRS {
            let dir_lit = literal(&home_canon.join(dir))?;
            profile.push_str(&format!(
                "(deny file-read* file-write* (subpath {dir_lit}))\n"
            ));
        }
        // A broad deny for HOME cannot be overridden by the later workspace
        // allow when the workspace itself lives under HOME. Exclude HOME from
        // the general read permission, then add narrow workspace/extra paths.
        // Metadata access is needed to resolve a workspace path through its
        // ancestors (Node.js performs this lstat/realpath walk at startup).
        profile.push_str(&format!("(allow file-read-metadata (subpath {home}))\n"));
        profile.push_str(&format!(
            "(allow file-read* (require-not (subpath {home})))\n"
        ));
    } else {
        profile.push_str("(allow file-read*)\n");
    }
    let root = literal(
        &std::fs::canonicalize(writable_root).unwrap_or_else(|_| writable_root.to_path_buf()),
    )?;
    profile.push_str(&format!(
        "(allow file-read* file-write* (subpath {root}))\n"
    ));
    if cwd != writable_root {
        let cwd_canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let cwd_lit = literal(&cwd_canon)?;
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath {cwd_lit}))\n"
        ));
    }
    for path in &policy.extra_read_paths {
        let path = literal(&std::fs::canonicalize(path)?)?;
        profile.push_str(&format!("(allow file-read* (subpath {path}))\n"));
    }
    for path in &policy.toolchain_overlay.extra_read {
        if !path.exists() {
            continue;
        }
        let path = literal(&std::fs::canonicalize(path)?)?;
        profile.push_str(&format!("(allow file-read* (subpath {path}))\n"));
    }
    for path in &policy.extra_write_paths {
        let path = literal(&std::fs::canonicalize(path)?)?;
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath {path}))\n"
        ));
    }
    for path in &policy.toolchain_overlay.extra_write {
        if !path.exists() {
            continue;
        }
        let path = literal(&std::fs::canonicalize(path)?)?;
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath {path}))\n"
        ));
    }
    if let Some(trust) = trust {
        if trust.global_git_config_allowed == Some(true) {
            for path in trust.global_git_read_paths().map(Path::new) {
                if !path.exists() {
                    continue;
                }
                let path = literal(&std::fs::canonicalize(path)?)?;
                profile.push_str(&format!("(allow file-read* (literal {path}))\n"));
            }
        }
        if trust.git_metadata_allowed == Some(true) {
            for path in trust.git_metadata_paths.iter().map(Path::new) {
                if !path.exists() {
                    continue;
                }
                let path = literal(&std::fs::canonicalize(path)?)?;
                profile.push_str(&format!(
                    "(allow file-read* file-write* (subpath {path}))\n"
                ));
            }
        }
    }
    if policy.network {
        profile.push_str("(allow network*)\n");
    }
    Ok(profile)
}

fn apply_git_environment(
    command: &mut Command,
    mode: SandboxMode,
    trust: Option<&kkagent_config::WorkspaceTrust>,
) {
    if mode == SandboxMode::Disabled {
        return;
    }
    command.envs(kkagent_config::git_environment(trust));
}

fn apply_workspace_env_whitelist(command: &mut Command) {
    let mut kept = std::collections::HashMap::new();
    for key in WORKSPACE_ENV_ALLOWLIST {
        if let Ok(value) = std::env::var(key) {
            kept.insert((*key).to_string(), value);
        }
    }
    // Prefer login-shell PATH when available so workspace builds see
    // rustup/homebrew/etc. without inheriting the full process env.
    if let Some(login_path) = kkagent_kaos::detect_login_shell_path() {
        kept.insert("PATH".into(), login_path);
    }
    for (key, value) in std::env::vars() {
        if key.starts_with("LC_") {
            kept.insert(key, value);
        }
    }
    command.env_clear();
    command.envs(kept);
}

fn resolve_auto_mode(warning: &mut Option<String>) -> SandboxMode {
    if cfg!(target_os = "windows") {
        return SandboxMode::Process;
    }
    if workspace_sandbox_available() {
        SandboxMode::Workspace
    } else {
        *warning = Some(auto_fallback_message());
        SandboxMode::Process
    }
}

fn workspace_sandbox_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        which::which("bwrap").is_ok()
    }
    #[cfg(target_os = "macos")]
    {
        Path::new("/usr/bin/sandbox-exec").is_file()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

fn auto_fallback_message() -> String {
    if cfg!(target_os = "linux") {
        "sandbox.mode=auto: bubblewrap (bwrap) not found; falling back to process mode. \
Install bubblewrap for workspace filesystem isolation."
            .into()
    } else if cfg!(target_os = "macos") {
        "sandbox.mode=auto: /usr/bin/sandbox-exec not found; falling back to process mode.".into()
    } else {
        "sandbox.mode=auto: workspace isolation unavailable; using process mode.".into()
    }
}

/// True when `path` is equal to or a descendant of `root`.
fn path_is_within(path: &Path, root: &Path) -> bool {
    if path == root {
        return true;
    }
    path.starts_with(root)
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn apply_resource_limits(command: &mut Command, policy: &SandboxPolicy) -> anyhow::Result<()> {
    // `sandbox.mode = "disabled"` opts out of every containment layer,
    // including OS resource limits, Job Objects and NO_NEW_PRIVS: users
    // disable the sandbox precisely so long builds, memory-hungry
    // toolchains, or setuid commands (sudo) can run unmodified.
    if policy.mode == SandboxMode::Disabled {
        return Ok(());
    }
    // For the remaining modes resource containment is independent from
    // filesystem sandboxing, so `process`/`workspace` still bound CPU and
    // memory even where file isolation is limited. Explicit zero values
    // retain the old unlimited behavior.
    if policy.memory_mb == 0 && policy.cpu_seconds == 0 && policy.max_processes == 0 {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        #[cfg(target_os = "linux")]
        let memory = policy.memory_mb.saturating_mul(1024 * 1024);
        let cpu = policy.cpu_seconds;
        unsafe {
            command.as_std_mut().pre_exec(move || {
                #[cfg(target_os = "linux")]
                if memory > 0 {
                    set_limit(libc::RLIMIT_AS as libc::c_int, memory)?;
                }
                if cpu > 0 {
                    set_limit(libc::RLIMIT_CPU as libc::c_int, cpu)?;
                }
                // RLIMIT_NPROC is accounted per real UID on Linux and macOS
                // alike, not per sandboxed process tree. Applying the
                // configured value here makes every fork fail with EAGAIN on
                // desktop systems where the user already owns more processes
                // than the limit, so process-count containment stays a
                // Windows Job Object feature (ACTIVE_PROCESS_LIMIT).
                #[cfg(target_os = "linux")]
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
        if policy.memory_mb > 0 || policy.max_processes > 0 {
            command.as_std_mut().creation_flags(CREATE_SUSPENDED);
        }
        let _ = command;
        if policy.mode == SandboxMode::Workspace {
            anyhow::bail!("workspace sandbox is unavailable on Windows");
        }
    }
    Ok(())
}

#[cfg(windows)]
fn resume_process_thread(pid: u32) -> anyhow::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut found = Thread32First(snapshot, &mut entry) != 0;
        while found {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !thread.is_null() {
                    let resumed = ResumeThread(thread);
                    CloseHandle(thread);
                    CloseHandle(snapshot);
                    if resumed == u32::MAX {
                        return Err(std::io::Error::last_os_error().into());
                    }
                    return Ok(());
                }
            }
            found = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        anyhow::bail!("cannot locate suspended main thread for process {pid}")
    }
}

#[cfg(unix)]
fn set_limit(resource: libc::c_int, value: u64) -> std::io::Result<()> {
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(resource as _, &mut current) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let requested = value as libc::rlim_t;
    let limit = libc::rlimit {
        rlim_cur: requested.min(current.rlim_max),
        rlim_max: current.rlim_max,
    };
    if unsafe { libc::setrlimit(resource as _, &limit) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_env_allowlist_is_minimal() {
        assert!(WORKSPACE_ENV_ALLOWLIST.contains(&"PATH"));
        assert!(WORKSPACE_ENV_ALLOWLIST.contains(&"TERM"));
        assert!(!WORKSPACE_ENV_ALLOWLIST
            .iter()
            .any(|k| k.contains("API_KEY")));
        assert!(!WORKSPACE_ENV_ALLOWLIST.contains(&"OPENAI_API_KEY"));
    }

    #[test]
    fn auto_mode_resolves_to_effective_mode() {
        let config = kkagent_config::SandboxConfig {
            mode: "auto".into(),
            ..Default::default()
        };
        let policy = SandboxPolicy::from_config(&config).unwrap();
        if cfg!(target_os = "windows") {
            assert_eq!(policy.mode, SandboxMode::Process);
        } else if workspace_sandbox_available() {
            assert_eq!(policy.mode, SandboxMode::Workspace);
            assert!(policy.auto_fallback_warning.is_none());
        } else {
            assert_eq!(policy.mode, SandboxMode::Process);
            assert!(policy.auto_fallback_warning.is_some());
        }
    }

    #[test]
    fn rejects_sensitive_extra_write_paths_by_default() {
        let home = dirs::home_dir().expect("home");
        let config = kkagent_config::SandboxConfig {
            mode: "process".into(),
            extra_write_paths: vec![home.join(".ssh").display().to_string()],
            ..Default::default()
        };
        let err = SandboxPolicy::from_config(&config).unwrap_err();
        assert!(
            err.to_string().contains("sensitive path")
                || err.to_string().contains("allow_sensitive_extra_paths"),
            "{err}"
        );
    }

    #[test]
    fn from_config_expands_tilde_in_extra_paths() {
        // Validation expands `~` before its sensitivity check, so the runtime
        // policy must expand too — otherwise `~/sdk` passes validation but is
        // bound as a literal `~` directory at sandbox setup time.
        let home = dirs::home_dir().expect("home");
        let config = kkagent_config::SandboxConfig {
            mode: "process".into(),
            memory_mb: 128,
            cpu_seconds: 10,
            max_processes: 8,
            extra_read_paths: vec!["~/kkagent-sandbox-extra-read".into()],
            extra_write_paths: vec!["~/kkagent-sandbox-extra-write".into()],
            system_read_paths: vec!["/nix/store".into(), "~/nix-extra".into()],
            ..Default::default()
        };
        let policy = SandboxPolicy::from_config(&config).unwrap();
        assert_eq!(
            policy.extra_read_paths,
            vec![home.join("kkagent-sandbox-extra-read")]
        );
        assert_eq!(
            policy.extra_write_paths,
            vec![home.join("kkagent-sandbox-extra-write")]
        );
        assert_eq!(
            policy.system_read_paths,
            vec![PathBuf::from("/nix/store"), home.join("nix-extra")]
        );
    }

    #[test]
    fn validates_config() {
        let config = kkagent_config::SandboxConfig {
            mode: "invalid".into(),
            ..Default::default()
        };
        assert!(SandboxPolicy::from_config(&config).is_err());
    }

    #[tokio::test]
    async fn disabled_mode_runs_commands() {
        let config = kkagent_config::SandboxConfig {
            mode: "disabled".into(),
            memory_mb: 0,
            cpu_seconds: 0,
            max_processes: 0,
            ..Default::default()
        };
        let policy = SandboxPolicy::from_config(&config).unwrap();
        assert_eq!(policy.mode, SandboxMode::Disabled);

        #[cfg(unix)]
        let mut command = policy
            .command("/bin/sh", "-c", "exit 0", Path::new("/tmp"))
            .unwrap();
        #[cfg(windows)]
        let mut command = policy
            .command("cmd.exe", "/C", "exit 0", &std::env::temp_dir())
            .unwrap();

        assert!(command.status().await.unwrap().success());
    }

    #[cfg(unix)]
    fn current_soft_ulimit(resource: libc::c_int) -> String {
        // `disabled` must not clamp limits below what the kkagent process
        // itself inherited (e.g. when the test binary runs inside an outer
        // sandboxed shell), so compare against the parent's soft limit.
        unsafe {
            let mut lim: libc::rlimit = std::mem::zeroed();
            assert_eq!(libc::getrlimit(resource, &mut lim), 0);
            if lim.rlim_cur == libc::RLIM_INFINITY {
                "unlimited".to_string()
            } else {
                lim.rlim_cur.to_string()
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn disabled_mode_skips_all_resource_limits() {
        let policy = SandboxPolicy {
            mode: SandboxMode::Disabled,
            memory_mb: 256,
            cpu_seconds: 1,
            max_processes: 1,
            ..Default::default()
        };
        let mut command = policy
            .command(
                "/bin/sh",
                "-c",
                "ulimit -v; ulimit -t; ulimit -u",
                Path::new("/tmp"),
            )
            .unwrap();
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        let limits: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .collect();
        // `disabled` must not clamp virtual memory, CPU time or process
        // count, even when non-zero values are configured: the child keeps
        // exactly the limits the parent inherited.
        assert_eq!(
            limits,
            vec![
                current_soft_ulimit(libc::RLIMIT_AS as libc::c_int),
                current_soft_ulimit(libc::RLIMIT_CPU as libc::c_int),
                current_soft_ulimit(libc::RLIMIT_NPROC as libc::c_int),
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn process_mode_still_applies_configured_memory_limit() {
        let policy = SandboxPolicy {
            mode: SandboxMode::Process,
            memory_mb: 256,
            cpu_seconds: 0,
            max_processes: 0,
            ..Default::default()
        };
        let mut command = policy
            .command("/bin/sh", "-c", "ulimit -v", Path::new("/tmp"))
            .unwrap();
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            (256 * 1024).to_string()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn process_mode_never_applies_per_uid_process_limit() {
        let policy = SandboxPolicy {
            mode: SandboxMode::Process,
            memory_mb: 0,
            cpu_seconds: 0,
            max_processes: 1,
            ..Default::default()
        };
        let mut command = policy
            .command(
                "/bin/bash",
                "-c",
                "/bin/echo child-process-ran; ulimit -u",
                Path::new("/tmp"),
            )
            .unwrap();
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("child-process-ran"));
        // RLIMIT_NPROC is per-UID on Linux; kkagent must never clamp it.
        assert!(
            !text.lines().any(|l| l.trim() == "1"),
            "unexpected RLIMIT_NPROC=1 in output: {text}"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn disabled_mode_skips_cpu_limit() {
        // `disabled` must not clamp CPU time even when cpu_seconds is
        // configured; this is the regression behind long builds being
        // killed by SIGXCPU after disabling the sandbox.
        let policy = SandboxPolicy {
            mode: SandboxMode::Disabled,
            memory_mb: 256,
            cpu_seconds: 1,
            max_processes: 1,
            ..Default::default()
        };
        let mut command = policy
            .command("/bin/sh", "-c", "ulimit -t", Path::new("/tmp"))
            .unwrap();
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        // The child must keep exactly the CPU limit this process inherited
        // (600s when the test binary itself runs inside kkagent's sandbox,
        // unlimited in CI) — proving disabled added no clamp of its own.
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            current_soft_ulimit(libc::RLIMIT_CPU as libc::c_int)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_profile_scopes_workspace_and_network() {
        let policy = SandboxPolicy {
            mode: SandboxMode::Workspace,
            network: false,
            ..Default::default()
        };
        let profile = macos_profile(
            &policy,
            None,
            Path::new("/tmp/work"),
            Path::new("/tmp/work"),
        )
        .unwrap();
        assert!(profile.contains("/tmp/work"));
        assert!(!profile.contains("allow network"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn mac_workspace_sandbox_blocks_outside_write() {
        let workspace =
            std::env::temp_dir().join(format!("kkagent-sandbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let outside = dirs::home_dir()
            .unwrap()
            .join(format!(".kkagent-outside-{}", uuid::Uuid::new_v4()));
        let policy = SandboxPolicy {
            mode: SandboxMode::Workspace,
            ..Default::default()
        };
        let script = format!(
            "printf allowed > inside.txt; printf denied > {}",
            outside.display()
        );
        let mut command = policy
            .command("/bin/bash", "-c", &script, &workspace)
            .unwrap();
        let status = command.status().await.unwrap();
        assert!(!status.success());
        assert_eq!(
            std::fs::read_to_string(workspace.join("inside.txt")).unwrap(),
            "allowed"
        );
        assert!(!outside.exists());
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn mac_process_limit_does_not_block_shell_children() {
        let workspace =
            std::env::temp_dir().join(format!("kkagent-sandbox-fork-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let policy = SandboxPolicy {
            mode: SandboxMode::Workspace,
            max_processes: 1,
            ..Default::default()
        };
        let mut command = policy
            .command("/bin/bash", "-c", "/bin/echo child-process-ran", &workspace)
            .unwrap();
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "child-process-ran"
        );
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn mac_workspace_denies_credential_reads_under_fake_home() {
        // The credential denies must hold even when the "HOME" is a fake
        // directory: profile uses macos_profile_with_home so the test does
        // not depend on the real user home layout.
        let base = std::env::temp_dir().join(format!("kkagent-cred-{}", uuid::Uuid::new_v4()));
        let home = base.join("home");
        let workspace = home.join("ws");
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("id_rsa"), "SECRET").unwrap();

        let policy = SandboxPolicy {
            mode: SandboxMode::Workspace,
            ..Default::default()
        };
        let profile =
            macos_profile_with_home(&policy, None, &workspace, &workspace, Some(&home)).unwrap();
        if std::env::var("KKAGENT_DUMP_PROFILE").is_ok() {
            eprintln!("---PROFILE---\n{profile}\n---END---");
        }
        // Sanity: the profile actually contains the deny + workspace allow.
        assert!(profile.contains("(deny file-read* file-write*"));
        assert!(profile.contains(".ssh"));

        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .arg("-p")
            .arg(&profile)
            .arg("/bin/bash")
            .arg("-c")
            .arg(format!("cat {}", ssh.join("id_rsa").display()));
        let output = command.output().await.unwrap();
        assert!(
            !output.status.success(),
            "credential read under sandbox must fail"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SECRET"));
        // And the workspace itself stays readable.
        std::fs::write(workspace.join("ok.txt"), "OK").unwrap();
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .arg("-p")
            .arg(&profile)
            .arg("/bin/bash")
            .arg("-c")
            .arg(format!("cat {}", workspace.join("ok.txt").display()));
        let output = command.output().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "OK");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn mac_workspace_below_home_remains_accessible() {
        let workspace = dirs::home_dir().unwrap().join(format!(
            ".kkagent-sandbox-home-workspace-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("visible.txt"), "visible").unwrap();
        let policy = SandboxPolicy {
            mode: SandboxMode::Workspace,
            ..Default::default()
        };
        let mut command = policy
            .command(
                "/bin/bash",
                "-c",
                "/bin/realpath . && /bin/ls visible.txt",
                &workspace,
            )
            .unwrap();
        let output = command.output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("visible.txt"));
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn mac_git_is_isolated_without_global_config_grant() {
        let workspace = dirs::home_dir().unwrap().join(format!(
            ".kkagent-sandbox-git-isolated-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        let initialized = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&workspace)
            .status()
            .unwrap();
        assert!(initialized.success());

        let policy = SandboxPolicy {
            mode: SandboxMode::Workspace,
            ..Default::default()
        };
        let output = policy
            .command("/bin/bash", "-c", "git status --short", &workspace)
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn mac_git_reads_explicitly_trusted_global_config() {
        let home = dirs::home_dir().unwrap();
        let workspace = home.join(format!(
            ".kkagent-sandbox-git-trusted-workspace-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = home.join(format!(
            ".kkagent-sandbox-gitconfig-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(&config_path, "[user]\n\temail = sandbox@example.test\n").unwrap();

        let mut policy = SandboxPolicy {
            mode: SandboxMode::Workspace,
            ..Default::default()
        };
        let mut trust = kkagent_config::WorkspaceTrust::new(&workspace);
        trust.global_git_config_allowed = Some(true);
        trust.global_git_config_roots = vec![config_path.to_string_lossy().into_owned()];
        trust.global_git_config_paths = trust.global_git_config_roots.clone();
        policy.upsert_workspace_trust(trust).unwrap();
        let output = policy
            .command("/bin/bash", "-c", "git config user.email", &workspace)
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "sandbox@example.test"
        );

        let output = policy
            .command(
                "/bin/bash",
                "-c",
                "git init --quiet && git config --local user.email local@example.test && git config user.email",
                &workspace,
            )
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "local@example.test"
        );
        std::fs::remove_file(config_path).unwrap();
        std::fs::remove_dir_all(workspace).unwrap();
    }
}
