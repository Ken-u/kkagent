use std::path::{Path, PathBuf};
use tokio::process::Command;

#[cfg(windows)]
pub struct SandboxProcessGuard(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for SandboxProcessGuard {}

#[cfg(windows)]
unsafe impl Sync for SandboxProcessGuard {}

#[cfg(not(windows))]
pub struct SandboxProcessGuard;

#[cfg(windows)]
impl Drop for SandboxProcessGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    Disabled,
    Process,
    Workspace,
}

#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    pub network: bool,
    pub memory_mb: u64,
    pub cpu_seconds: u64,
    pub max_processes: u32,
    pub extra_read_paths: Vec<PathBuf>,
    pub extra_write_paths: Vec<PathBuf>,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            mode: SandboxMode::Process,
            network: true,
            memory_mb: 4096,
            cpu_seconds: 600,
            max_processes: 128,
            extra_read_paths: Vec::new(),
            extra_write_paths: Vec::new(),
        }
    }
}

impl SandboxPolicy {
    pub fn from_config(config: &kkagent_config::SandboxConfig) -> anyhow::Result<Self> {
        let mode = match config.mode.trim().to_ascii_lowercase().as_str() {
            "auto" => {
                if cfg!(target_os = "windows") {
                    SandboxMode::Process
                } else {
                    SandboxMode::Workspace
                }
            }
            "disabled" | "off" | "none" => SandboxMode::Disabled,
            "process" => SandboxMode::Process,
            "workspace" | "strict" => SandboxMode::Workspace,
            other => anyhow::bail!(
                "invalid sandbox.mode {other:?}; expected auto, disabled, process, or workspace"
            ),
        };
        if config.memory_mb < 64 || config.cpu_seconds == 0 || config.max_processes == 0 {
            anyhow::bail!("sandbox limits must be positive and memory_mb must be at least 64");
        }
        Ok(Self {
            mode,
            network: config.network,
            memory_mb: config.memory_mb,
            cpu_seconds: config.cpu_seconds,
            max_processes: config.max_processes,
            extra_read_paths: config.extra_read_paths.iter().map(PathBuf::from).collect(),
            extra_write_paths: config.extra_write_paths.iter().map(PathBuf::from).collect(),
        })
    }

    pub fn command(
        &self,
        shell: &str,
        flag: &str,
        script: &str,
        cwd: &Path,
    ) -> anyhow::Result<Command> {
        let cwd = std::fs::canonicalize(cwd)
            .map_err(|error| anyhow::anyhow!("cannot sandbox cwd {}: {error}", cwd.display()))?;
        let mut command = match self.mode {
            SandboxMode::Disabled | SandboxMode::Process => shell_command(shell, flag, script),
            SandboxMode::Workspace => workspace_command(self, shell, flag, script, &cwd)?,
        };
        command.current_dir(&cwd);
        command.env("KKAGENT_SANDBOX", self.mode_name());
        apply_resource_limits(&mut command, self)?;
        Ok(command)
    }

    pub fn mode_name(&self) -> &'static str {
        match self.mode {
            SandboxMode::Disabled => "disabled",
            SandboxMode::Process => "process",
            SandboxMode::Workspace => "workspace",
        }
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
            let pid = child
                .id()
                .ok_or_else(|| anyhow::anyhow!("spawned process has no id"))?;
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(std::io::Error::last_os_error().into());
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                    | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                    | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
                info.BasicLimitInformation.ActiveProcessLimit = self.max_processes;
                info.ProcessMemoryLimit = self.memory_mb.saturating_mul(1024 * 1024) as usize;
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
                Ok(SandboxProcessGuard(job))
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
    shell: &str,
    flag: &str,
    script: &str,
    cwd: &Path,
) -> anyhow::Result<Command> {
    let sandbox = Path::new("/usr/bin/sandbox-exec");
    if !sandbox.is_file() {
        anyhow::bail!("workspace sandbox requires /usr/bin/sandbox-exec on macOS");
    }
    let profile = macos_profile(policy, cwd)?;
    let mut command = Command::new(sandbox);
    command.args(["-p", &profile, shell, flag, script]);
    Ok(command)
}

#[cfg(target_os = "linux")]
fn workspace_command(
    policy: &SandboxPolicy,
    shell: &str,
    flag: &str,
    script: &str,
    cwd: &Path,
) -> anyhow::Result<Command> {
    let bwrap = which::which("bwrap")
        .map_err(|_| anyhow::anyhow!("workspace sandbox requires bubblewrap (bwrap) on Linux"))?;
    let mut command = Command::new(bwrap);
    command.args(["--die-with-parent", "--new-session", "--unshare-all"]);
    if policy.network {
        command.arg("--share-net");
    }
    for path in ["/bin", "/sbin", "/usr", "/lib", "/lib64", "/etc", "/opt"] {
        if Path::new(path).exists() {
            command.args(["--ro-bind", path, path]);
        }
    }
    command.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
    bind_path(&mut command, "--bind", cwd)?;
    for path in &policy.extra_read_paths {
        bind_path(&mut command, "--ro-bind", path)?;
    }
    for path in &policy.extra_write_paths {
        bind_path(&mut command, "--bind", path)?;
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
    _shell: &str,
    _flag: &str,
    _script: &str,
    _cwd: &Path,
) -> anyhow::Result<Command> {
    anyhow::bail!("workspace filesystem sandbox is unavailable on Windows; use process mode or run kkagent in Windows Sandbox/WDAG")
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn workspace_command(
    _policy: &SandboxPolicy,
    _shell: &str,
    _flag: &str,
    _script: &str,
    _cwd: &Path,
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

fn path_text(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("sandbox path is not valid UTF-8: {}", path.display()))
}

#[cfg(target_os = "macos")]
fn macos_profile(policy: &SandboxPolicy, cwd: &Path) -> anyhow::Result<String> {
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
    if let Some(home) = dirs::home_dir() {
        let home = literal(&std::fs::canonicalize(home)?)?;
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
    let cwd = literal(cwd)?;
    profile.push_str(&format!("(allow file-read* file-write* (subpath {cwd}))\n"));
    for path in &policy.extra_read_paths {
        let path = literal(&std::fs::canonicalize(path)?)?;
        profile.push_str(&format!("(allow file-read* (subpath {path}))\n"));
    }
    for path in &policy.extra_write_paths {
        let path = literal(&std::fs::canonicalize(path)?)?;
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath {path}))\n"
        ));
    }
    if policy.network {
        profile.push_str("(allow network*)\n");
    }
    Ok(profile)
}

fn apply_resource_limits(command: &mut Command, policy: &SandboxPolicy) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        #[cfg(target_os = "linux")]
        let memory = policy.memory_mb.saturating_mul(1024 * 1024);
        let cpu = policy.cpu_seconds;
        #[cfg(target_os = "linux")]
        let processes = policy.max_processes as u64;
        unsafe {
            command.as_std_mut().pre_exec(move || {
                #[cfg(target_os = "linux")]
                set_limit(libc::RLIMIT_AS as libc::c_int, memory)?;
                set_limit(libc::RLIMIT_CPU as libc::c_int, cpu)?;
                // macOS accounts RLIMIT_NPROC across the entire user rather than
                // the sandboxed process tree. Applying a per-command value there
                // prevents ordinary shell commands from forking as soon as the
                // desktop user owns more processes than the configured limit.
                #[cfg(target_os = "linux")]
                set_limit(libc::RLIMIT_NPROC as libc::c_int, processes)?;
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
        command.as_std_mut().creation_flags(CREATE_SUSPENDED);
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
    fn validates_config() {
        let config = kkagent_config::SandboxConfig {
            mode: "invalid".into(),
            ..Default::default()
        };
        assert!(SandboxPolicy::from_config(&config).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_profile_scopes_workspace_and_network() {
        let policy = SandboxPolicy {
            mode: SandboxMode::Workspace,
            network: false,
            ..Default::default()
        };
        let profile = macos_profile(&policy, Path::new("/tmp/work")).unwrap();
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
}
