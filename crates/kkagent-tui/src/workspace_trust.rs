use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use kkagent_config::{AppConfig, WorkspaceTrust};
use ratatui::{
    backend::CrosstermBackend,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame, Terminal,
};
use std::collections::{BTreeSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_CONFIG_INCLUDE_FILES: usize = 64;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GitTrustDiscovery {
    external_metadata: Vec<String>,
    config_roots: Vec<String>,
    repo_config_path: Option<String>,
    config_paths: Vec<String>,
    ignore_path: Option<String>,
    attributes_path: Option<String>,
    risks: Vec<String>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuestionKind {
    Workspace,
    GitMetadata,
    GlobalGitConfig,
}

#[derive(Debug, Clone)]
struct TrustQuestion {
    kind: QuestionKind,
    title: String,
    summary: String,
    details: Vec<String>,
    allow_label: String,
    deny_label: String,
}

/// Review and persist the grants needed before the in-process server starts.
/// Returns the effective record that should also be sent to a connected server.
pub fn ensure_workspace_trust(
    config: &mut AppConfig,
    config_path: &Path,
    workspace: &Path,
    use_alt_screen: bool,
) -> Result<WorkspaceTrust> {
    let workspace = std::fs::canonicalize(workspace)
        .with_context(|| format!("Cannot resolve workspace {}", workspace.display()))?;
    let discovery = discover_git_trust(&workspace);
    let inherited = config.workspace_trust.matching(&workspace).cloned();
    let mut record = inherited
        .clone()
        .unwrap_or_else(|| WorkspaceTrust::new(&workspace));

    let configured_trust = config.trusted_workspaces.iter().any(|root| {
        let root = PathBuf::from(root);
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        workspace.starts_with(root)
    });
    let mut questions = Vec::new();
    if inherited.is_none() && !configured_trust {
        questions.push(TrustQuestion {
            kind: QuestionKind::Workspace,
            title: "Trust this workspace?".into(),
            summary: format!(
                "Allow kkagent to load project instructions and run commands in {}",
                workspace.display()
            ),
            details: vec![
                "Only trust repositories whose code and AGENTS/MCP/Hook configuration you accept."
                    .into(),
            ],
            allow_label: "Trust workspace".into(),
            deny_label: "Cancel".into(),
        });
    }

    if metadata_needs_review(&record, &discovery.external_metadata) {
        let mut details = abbreviated_paths(&discovery.external_metadata);
        details
            .push("Read/write access is required for status, commit, rebase and repo sync.".into());
        questions.push(TrustQuestion {
            kind: QuestionKind::GitMetadata,
            title: "External Git metadata".into(),
            summary: "This workspace resolves Git state outside its trusted root.".into(),
            details,
            allow_label: "Allow read/write Git metadata".into(),
            deny_label: "Keep external metadata blocked".into(),
        });
    }

    if global_config_needs_review(&record, &discovery) {
        let mut details = abbreviated_paths(&discovery.config_paths);
        if let Some(path) = &discovery.ignore_path {
            details.push(format!("global ignore: {path}"));
        }
        if let Some(path) = &discovery.attributes_path {
            details.push(format!("global attributes: {path}"));
        }
        if let Some(path) = &discovery.repo_config_path {
            details.push(format!("AOSP repo user config: {path}"));
        }
        if !discovery.risks.is_empty() {
            details.push(format!(
                "Capabilities present: {}",
                discovery.risks.join(", ")
            ));
        }
        details.extend(discovery.warnings.iter().cloned());
        details.push(
            "Read-only config access does not grant .ssh, .git-credentials, .gnupg or keychain files."
                .into(),
        );
        questions.push(TrustQuestion {
            kind: QuestionKind::GlobalGitConfig,
            title: "Global Git configuration".into(),
            summary: "Allow Git and the agent to read the listed global preferences?".into(),
            details,
            allow_label: "Allow read-only Git config".into(),
            deny_label: "Use isolated Git config".into(),
        });
    }

    let answers = if questions.is_empty() {
        Vec::new()
    } else {
        run_questions(&questions, use_alt_screen)?
    };
    for (question, allowed) in questions.iter().zip(answers) {
        match question.kind {
            QuestionKind::Workspace if !allowed => {
                anyhow::bail!("Workspace was not trusted")
            }
            QuestionKind::Workspace => {}
            QuestionKind::GitMetadata => record.git_metadata_allowed = Some(allowed),
            QuestionKind::GlobalGitConfig => record.global_git_config_allowed = Some(allowed),
        }
    }

    if discovery.external_metadata.is_empty() {
        record.git_metadata_allowed.get_or_insert(true);
    }
    record.git_metadata_paths = discovery.external_metadata;

    if record.global_git_config_allowed == Some(true) {
        record.global_git_config_roots = discovery.config_roots;
        record.repo_config_path = discovery.repo_config_path;
        record.global_git_config_paths = discovery.config_paths;
        record.global_git_ignore_path = discovery.ignore_path;
        record.global_git_attributes_path = discovery.attributes_path;
        record.global_git_risks = discovery.risks;
    } else if record.global_git_config_allowed == Some(false) {
        record.global_git_config_roots.clear();
        record.repo_config_path = None;
        record.global_git_config_paths.clear();
        record.global_git_ignore_path = None;
        record.global_git_attributes_path = None;
        record.global_git_risks.clear();
    }

    // Preserve a broader inherited trust record rather than creating a narrower
    // duplicate for every directory entered beneath it.
    config.workspace_trust.upsert(record.clone());
    config.workspace_trust.save(config_path)?;
    Ok(record)
}

fn metadata_needs_review(record: &WorkspaceTrust, discovered: &[String]) -> bool {
    if discovered.is_empty() || record.git_metadata_allowed == Some(false) {
        return false;
    }
    record.git_metadata_allowed.is_none()
        || discovered
            .iter()
            .any(|path| !record.git_metadata_paths.contains(path))
}

fn global_config_needs_review(record: &WorkspaceTrust, discovery: &GitTrustDiscovery) -> bool {
    let has_global = !discovery.config_paths.is_empty()
        || discovery.ignore_path.is_some()
        || discovery.attributes_path.is_some();
    if !has_global || record.global_git_config_allowed == Some(false) {
        return false;
    }
    record.global_git_config_allowed.is_none()
        || discovery
            .config_paths
            .iter()
            .any(|path| !record.global_git_config_paths.contains(path))
        || discovery
            .risks
            .iter()
            .any(|risk| !record.global_git_risks.contains(risk))
        || discovery.repo_config_path != record.repo_config_path
        || discovery.ignore_path != record.global_git_ignore_path
        || discovery.attributes_path != record.global_git_attributes_path
}

fn discover_git_trust(workspace: &Path) -> GitTrustDiscovery {
    let mut discovery = GitTrustDiscovery {
        external_metadata: discover_external_git_metadata(workspace),
        ..Default::default()
    };
    discover_global_git_config(workspace, &mut discovery);
    discovery
}

fn discover_external_git_metadata(workspace: &Path) -> Vec<String> {
    if let Some(repo_dir) = workspace
        .ancestors()
        .map(|ancestor| ancestor.join(".repo"))
        .find(|path| path.is_dir())
    {
        if let Ok(repo_dir) = std::fs::canonicalize(repo_dir) {
            if !repo_dir.starts_with(workspace) {
                return vec![repo_dir.to_string_lossy().into_owned()];
            }
        }
        return Vec::new();
    }

    let mut paths = BTreeSet::new();
    for args in [
        &["rev-parse", "--path-format=absolute", "--git-dir"][..],
        &["rev-parse", "--path-format=absolute", "--git-common-dir"][..],
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ][..],
    ] {
        if let Some(path) = git_path(workspace, args) {
            insert_external_path(&mut paths, workspace, &path);
        }
    }
    if paths.is_empty() {
        if let Some(path) = git_path(workspace, &["rev-parse", "--absolute-git-dir"]) {
            insert_external_path(&mut paths, workspace, &path);
        }
    }

    if let Some(objects) = git_path(
        workspace,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ],
    ) {
        let alternates = objects.join("info").join("alternates");
        if let Ok(body) = std::fs::read_to_string(alternates) {
            for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
                let path = PathBuf::from(line);
                let path = if path.is_absolute() {
                    path
                } else {
                    objects.join(path)
                };
                insert_external_path(&mut paths, workspace, &path);
            }
        }
    }
    paths.into_iter().collect()
}

fn insert_external_path(paths: &mut BTreeSet<String>, workspace: &Path, path: &Path) {
    let path = absolutize(workspace, path);
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    if !path.starts_with(workspace) {
        paths.insert(path.to_string_lossy().into_owned());
    }
}

fn git_path(workspace: &Path, args: &[&str]) -> Option<PathBuf> {
    let output = isolated_git(workspace).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(absolutize(workspace, Path::new(&value)))
    }
}

fn isolated_git(workspace: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(workspace)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "core.fsmonitor")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_1", null_device())
        .env("GIT_CONFIG_KEY_2", "credential.helper")
        .env("GIT_CONFIG_VALUE_2", "");
    command
}

fn discover_global_git_config(workspace: &Path, discovery: &mut GitTrustDiscovery) {
    let Some(home) = dirs::home_dir() else {
        discovery
            .warnings
            .push("Home directory could not be determined.".into());
        return;
    };
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let candidates = [xdg.join("git/config"), home.join(".gitconfig")];
    for candidate in candidates {
        if let Some(path) = canonical_file(&candidate) {
            push_unique(&mut discovery.config_roots, path);
        }
    }

    let repo_config_base = std::env::var_os("REPO_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.clone());
    discovery.repo_config_path = canonical_file(&repo_config_base.join(".repoconfig/config"));

    let mut queue: VecDeque<PathBuf> = discovery.config_roots.iter().map(PathBuf::from).collect();
    if let Some(path) = &discovery.repo_config_path {
        queue.push_back(PathBuf::from(path));
    }
    let mut visited = BTreeSet::new();
    while let Some(path) = queue.pop_front() {
        if visited.len() >= MAX_CONFIG_INCLUDE_FILES {
            discovery.warnings.push(format!(
                "Stopped after {MAX_CONFIG_INCLUDE_FILES} recursively included Git config files."
            ));
            break;
        }
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
        let key = canonical.to_string_lossy().into_owned();
        if !visited.insert(key.clone()) {
            continue;
        }
        discovery.config_paths.push(key);
        scan_config_risks(&canonical, &mut discovery.risks);
        for include in config_includes(&canonical) {
            let include = if include.is_absolute() {
                include
            } else {
                canonical.parent().unwrap_or(Path::new(".")).join(include)
            };
            if include.is_file() {
                queue.push_back(include);
            } else {
                discovery.warnings.push(format!(
                    "Git config include is unavailable: {}",
                    include.display()
                ));
            }
        }
    }
    discovery.config_paths.sort();
    discovery.config_paths.dedup();
    discovery.risks.sort();
    discovery.risks.dedup();

    discovery.ignore_path = configured_git_path(workspace, "core.excludesFile")
        .or_else(|| canonical_file(&xdg.join("git/ignore")));
    discovery.attributes_path = configured_git_path(workspace, "core.attributesFile")
        .or_else(|| canonical_file(&xdg.join("git/attributes")));
}

fn config_includes(path: &Path) -> Vec<PathBuf> {
    let Ok(output) = Command::new("git")
        .args([
            "config",
            "--file",
            &path.to_string_lossy(),
            "--no-includes",
            "--path",
            "--get-regexp",
            r"^include(if\..*)?\.path$",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() && output.status.code() != Some(1) {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            line.split_once(char::is_whitespace)
                .map(|(_, value)| value.trim())
        })
        .filter(|value| !value.is_empty())
        .map(expand_home)
        .collect()
}

fn scan_config_risks(path: &Path, risks: &mut Vec<String>) {
    let Ok(output) = Command::new("git")
        .args([
            "config",
            "--file",
            &path.to_string_lossy(),
            "--no-includes",
            "--name-only",
            "-z",
            "--list",
        ])
        .output()
    else {
        return;
    };
    for key in output.stdout.split(|byte| *byte == 0) {
        let key = String::from_utf8_lossy(key).to_ascii_lowercase();
        let category = if key.starts_with("credential.") || key == "core.askpass" {
            Some("credential helpers")
        } else if key == "http.extraheader" || key == "http.cookiefile" || key == "http.sslkey" {
            Some("HTTP secrets or client keys")
        } else if key.starts_with("alias.")
            || key == "core.hookspath"
            || key == "core.fsmonitor"
            || key.ends_with(".command")
            || key.ends_with(".driver")
            || key.ends_with(".clean")
            || key.ends_with(".smudge")
            || key.ends_with(".process")
        {
            Some("external commands")
        } else if key.starts_with("include.") || key.starts_with("includeif.") {
            Some("included config files")
        } else if key.starts_with("url.") || key.ends_with(".proxy") {
            Some("URL rewrites or proxies")
        } else {
            None
        };
        if let Some(category) = category {
            risks.push(category.into());
        }
    }
}

fn configured_git_path(workspace: &Path, key: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--global", "--includes", "--path", "--get", key])
        .current_dir(workspace)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    canonical_file(&expand_home(String::from_utf8_lossy(&output.stdout).trim()))
}

fn canonical_file(path: &Path) -> Option<String> {
    path.is_file()
        .then(|| std::fs::canonicalize(path).ok())
        .flatten()
        .map(|path| path.to_string_lossy().into_owned())
}

fn expand_home(value: impl AsRef<str>) -> PathBuf {
    let value = value.as_ref();
    if value == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(value)
}

fn absolutize(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn push_unique(paths: &mut Vec<String>, path: String) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn abbreviated_paths(paths: &[String]) -> Vec<String> {
    let mut lines: Vec<String> = paths
        .iter()
        .take(6)
        .map(|path| format!("• {path}"))
        .collect();
    if paths.len() > 6 {
        lines.push(format!("• … and {} more", paths.len() - 6));
    }
    lines
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn run_questions(questions: &[TrustQuestion], use_alt_screen: bool) -> Result<Vec<bool>> {
    enable_raw_mode().context("Failed to enter raw mode for workspace trust review")?;
    let mut stdout = io::stdout();
    if use_alt_screen {
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
    };

    let result = (|| -> Result<Vec<bool>> {
        let mut answers = Vec::with_capacity(questions.len());
        for question in questions {
            // Trust prompts fail closed: Enter keeps the narrower/blocked choice
            // unless the user explicitly selects the allow option.
            let mut selected = 1usize;
            let allowed = loop {
                terminal.draw(|frame| render_question(frame, question, selected))?;
                if let Event::Key(key) = event::read()? {
                    if !crate::platform_keys::is_actionable_key_event(&key) {
                        continue;
                    }
                    match key.code {
                        KeyCode::Up | KeyCode::Left => selected = selected.saturating_sub(1),
                        KeyCode::Down | KeyCode::Right => selected = (selected + 1).min(1),
                        KeyCode::Char('1') => break true,
                        KeyCode::Char('2') => break false,
                        KeyCode::Enter => break selected == 0,
                        KeyCode::Esc | KeyCode::Char('q') => {
                            anyhow::bail!("Workspace trust review cancelled")
                        }
                        _ => {}
                    }
                }
            };
            answers.push(allowed);
            if question.kind == QuestionKind::Workspace && !allowed {
                break;
            }
        }
        Ok(answers)
    })();

    let _ = disable_raw_mode();
    if use_alt_screen {
        let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    }
    let _ = terminal.show_cursor();
    result
}

fn render_question(frame: &mut Frame, question: &TrustQuestion, selected: usize) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = if area.width >= 16 {
        area.width.saturating_sub(4).min(88)
    } else {
        area.width
    }
    .max(1);

    let mut lines = vec![
        Line::from(Span::styled(
            question.summary.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for detail in question.details.iter().take(12) {
        lines.push(Line::from(Span::styled(
            detail.clone(),
            Style::default().fg(Color::Gray),
        )));
    }
    lines.push(Line::from(""));
    for (index, label) in [&question.allow_label, &question.deny_label]
        .into_iter()
        .enumerate()
    {
        let style = if selected == index {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        lines.push(Line::from(vec![
            Span::styled(if selected == index { "> " } else { "  " }, style),
            Span::styled(format!("{}  {label}", index + 1), style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if width < 40 {
            "↑↓ / 1·2 · Enter · Esc"
        } else {
            "↑↓ / 1·2 / Enter · Esc cancel"
        },
        Style::default().fg(Color::DarkGray),
    )));

    let inner_width = width.saturating_sub(2).max(1) as usize;
    let content_height = lines.iter().fold(0u16, |height, line| {
        let rows = line.width().max(1).div_ceil(inner_width) as u16;
        height.saturating_add(rows.max(1))
    });
    let height = content_height.saturating_add(2).max(4);
    let panel = centered_rect(area, width, height);
    frame.render_widget(Clear, panel);

    let block = Block::default()
        .title(format!(" {} ", question.title))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false }),
        panel,
    );
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width.min(area.width),
        height.min(area.height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn detects_new_grants_without_reprompting_denials() {
        let mut record = WorkspaceTrust::new(Path::new("/workspace"));
        let paths = vec!["/outside/git".to_string()];
        assert!(metadata_needs_review(&record, &paths));
        record.git_metadata_allowed = Some(false);
        assert!(!metadata_needs_review(&record, &paths));

        let discovery = GitTrustDiscovery {
            config_paths: vec!["/home/user/.gitconfig".into()],
            risks: vec!["external commands".into()],
            ..Default::default()
        };
        assert!(global_config_needs_review(&record, &discovery));
        record.global_git_config_allowed = Some(false);
        assert!(!global_config_needs_review(&record, &discovery));
    }

    #[test]
    fn abbreviates_large_path_sets() {
        let paths = (0..8).map(|i| format!("/path/{i}")).collect::<Vec<_>>();
        let lines = abbreviated_paths(&paths);
        assert_eq!(lines.len(), 7);
        assert!(lines.last().unwrap().contains("2 more"));
    }

    #[test]
    fn trust_prompt_keeps_its_border_and_choices_on_a_phone_terminal() {
        let backend = TestBackend::new(24, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let question = TrustQuestion {
            kind: QuestionKind::Workspace,
            title: "Trust this workspace?".into(),
            summary: "Allow project instructions and commands in a narrow terminal".into(),
            details: vec![
                "Only trust repositories whose code and configuration you accept.".into(),
            ],
            allow_label: "Trust workspace".into(),
            deny_label: "Cancel".into(),
        };

        terminal
            .draw(|frame| render_question(frame, &question, 1))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .filter_map(|x| buffer.cell((x, y)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(rows
            .iter()
            .any(|row| row.contains('┌') && row.contains('┐')));
        assert!(rows.join("\n").contains("Cancel"));
    }

    #[test]
    fn detects_aosp_repo_metadata_above_a_subproject() {
        let root = std::env::temp_dir().join(format!(
            "kkagent-aosp-trust-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = root.join("frameworks/base");
        std::fs::create_dir_all(root.join(".repo/projects/frameworks/base.git")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let paths = discover_external_git_metadata(&project);
        assert_eq!(
            paths,
            vec![std::fs::canonicalize(root.join(".repo"))
                .unwrap()
                .to_string_lossy()
                .into_owned()]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn follows_unconditional_and_conditional_git_config_includes() {
        let root = std::env::temp_dir().join(format!(
            "kkagent-git-config-trust-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let config = root.join("config");
        let child = root.join("child.conf");
        let conditional = root.join("conditional.conf");
        std::fs::write(&child, "[user]\n\temail = test@example.test\n").unwrap();
        std::fs::write(&conditional, "[alias]\n\tdanger = !echo test\n").unwrap();
        std::fs::write(
            &config,
            format!(
                "[include]\n\tpath = {}\n[includeIf \"gitdir:/a/**\"]\n\tpath = {}\n",
                child.display(),
                conditional.display()
            ),
        )
        .unwrap();

        let includes = config_includes(&config);
        assert_eq!(includes.len(), 2);
        assert!(includes.contains(&child));
        assert!(includes.contains(&conditional));
        let mut risks = Vec::new();
        scan_config_risks(&conditional, &mut risks);
        assert!(risks.contains(&"external commands".to_string()));
        std::fs::remove_dir_all(root).unwrap();
    }
}
