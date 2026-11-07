//! Spots `Bash` commands that only read, search or list files and names the
//! dedicated tool that does the same job.
//!
//! The command is still executed — this is a nudge appended to the result,
//! never a block. Detection is deliberately conservative: anything with
//! redirects, command chaining, substitutions or an unknown filter in a pipe
//! gets no hint, so a missing hint is always cheaper than a wrong one.

use std::path::Path;

/// The dedicated tool a shell command could have used instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeTool {
    Read,
    Grep,
    Glob,
}

impl NativeTool {
    /// Tool name as registered in the tool registry.
    pub fn name(self) -> &'static str {
        match self {
            Self::Read => "Read",
            Self::Grep => "Grep",
            Self::Glob => "Glob",
        }
    }

    /// One-line pointer appended to the Bash result.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Read => {
                "Hint: for plain file reads, Read (path, offset, limit; files:[...] for several) \
                 returns numbered lines with bounded output and runs in parallel with other calls."
            }
            Self::Grep => {
                "Hint: for searching, Grep (pattern, path, glob, output_mode) honors ignore files \
                 and bounds output; several searches can run in one turn."
            }
            Self::Glob => {
                "Hint: for listing or finding files, Glob (pattern) needs no shell and runs in \
                 parallel with other calls."
            }
        }
    }
}

/// Commands that may follow a `|` without changing what the pipeline is for.
const FILTERS: &[&str] = &[
    "head", "tail", "wc", "sort", "uniq", "cut", "nl", "grep", "egrep", "fgrep", "rg", "tr",
    "column",
];

/// The dedicated tool that could replace `command`, if it is a plain
/// read / search / list.
pub fn suggest(command: &str) -> Option<NativeTool> {
    let command = strip_leading_cd(command)
        .replace("2>/dev/null", "")
        .replace("2> /dev/null", "")
        .replace("2>&1", "");
    let segments = split_pipeline(&command)?;
    let (first, rest) = segments.split_first()?;
    if !rest.iter().all(|segment| {
        segment
            .first()
            .is_some_and(|word| FILTERS.contains(&base(word)))
    }) {
        return None;
    }
    classify(first)
}

/// `cd dir && rest` is how models anchor a command; the hint is about `rest`.
fn strip_leading_cd(command: &str) -> &str {
    let mut rest = command.trim();
    while let Some(after) = rest.strip_prefix("cd ") {
        match after.split_once("&&") {
            Some((_, tail)) => rest = tail.trim_start(),
            None => break,
        }
    }
    rest
}

fn base(word: &str) -> &str {
    Path::new(word)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(word)
}

/// Splits a simple pipeline into shell words. `None` for anything that is not
/// one plain pipeline: chaining, redirects, background, subshells,
/// substitutions, multiple lines or unbalanced quotes.
fn split_pipeline(command: &str) -> Option<Vec<Vec<String>>> {
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = command.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        ch => word.push(ch),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => {
                            let escaped = chars.next()?;
                            if !matches!(escaped, '"' | '\\' | '$' | '`') {
                                word.push('\\');
                            }
                            word.push(escaped);
                        }
                        '`' => return None,
                        '$' if chars.peek() == Some(&'(') => return None,
                        ch => word.push(ch),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next()?);
            }
            '$' if chars.peek() == Some(&'(') => return None,
            '\n' | '\r' | ';' | '&' | '<' | '>' | '(' | ')' | '`' => return None,
            '|' => {
                if chars.peek() == Some(&'|') {
                    return None;
                }
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
                if words.is_empty() {
                    return None;
                }
                segments.push(std::mem::take(&mut words));
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    if words.is_empty() {
        return None;
    }
    segments.push(words);
    Some(segments)
}

fn classify(words: &[String]) -> Option<NativeTool> {
    // `FOO=bar cmd …`
    let start = words
        .iter()
        .position(|word| !is_env_assignment(word))
        .unwrap_or(words.len());
    let (name, args) = words[start..].split_first()?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let positional = args.iter().filter(|arg| !arg.starts_with('-')).count();
    let has_flag = |long: &str, short: char| {
        args.iter().any(|arg| {
            *arg == long
                || arg
                    .strip_prefix('-')
                    .is_some_and(|cluster| !cluster.starts_with('-') && cluster.contains(short))
        })
    };

    match base(name) {
        "cat" | "head" | "tail" | "nl" | "bat" => {
            let follows = base(name) == "tail"
                && (args.iter().any(|arg| arg.starts_with("--follow"))
                    || has_flag("--follow", 'f')
                    || has_flag("--follow", 'F'));
            (!follows && positional >= 1).then_some(NativeTool::Read)
        }
        "sed" => {
            let quiet = args.iter().any(|arg| *arg == "-n" || *arg == "--quiet");
            let in_place = args
                .iter()
                .any(|arg| arg.starts_with("-i") || arg.starts_with("--in-place"));
            (quiet && !in_place && positional >= 2).then_some(NativeTool::Read)
        }
        "rg" | "ag" | "ack" => (positional >= 1).then_some(NativeTool::Grep),
        "grep" | "egrep" | "fgrep" => {
            let recursive = has_flag("--recursive", 'r') || has_flag("--recursive", 'R');
            let needed = if recursive { 1 } else { 2 };
            (positional >= needed).then_some(NativeTool::Grep)
        }
        "ls" | "tree" | "fd" | "fdfind" => Some(NativeTool::Glob),
        "find" => {
            let mutating = args.iter().any(|arg| {
                matches!(*arg, "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete")
                    || arg.starts_with("-fprint")
                    || arg.starts_with("-fls")
            });
            (!mutating).then_some(NativeTool::Glob)
        }
        _ => None,
    }
}

fn is_env_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((key, _)) => {
            !key.is_empty()
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !key.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(command: &str) -> Option<NativeTool> {
        suggest(command)
    }

    #[test]
    fn plain_reads_point_at_read() {
        for command in [
            "cat src/main.rs",
            "head -n 40 src/main.rs",
            "head -40 README.md",
            "tail -n 20 /var/log/app.log",
            "sed -n '10,40p' src/lib.rs",
            "sed -n \"10,40p\" src/lib.rs | head -20",
            "cat a.rs b.rs",
            "nl -ba src/lib.rs | head -5",
            "cd crates/x && cat Cargo.toml",
            "cd \"a b\" && cd c && head -5 f.txt",
            "/bin/cat Cargo.toml",
            "LC_ALL=C cat Cargo.toml",
            "cat Cargo.toml 2>/dev/null",
        ] {
            assert_eq!(s(command), Some(NativeTool::Read), "{command}");
        }
    }

    #[test]
    fn searches_point_at_grep() {
        for command in [
            "rg foo",
            "rg -n \"fn main\" src",
            "grep -rn TODO src",
            "grep -n foo src/lib.rs",
            "egrep -r 'a|b' .",
            "rg foo src | head -20",
            "rg -l needle | wc -l",
            "cd app && rg -n 'x;y' src",
        ] {
            assert_eq!(s(command), Some(NativeTool::Grep), "{command}");
        }
    }

    #[test]
    fn listings_point_at_glob() {
        for command in [
            "ls",
            "ls -la crates/",
            "find . -name '*.rs'",
            "find src -type f | sort | head -50",
            "fd main.rs",
            "tree -L 2",
        ] {
            assert_eq!(s(command), Some(NativeTool::Glob), "{command}");
        }
    }

    #[test]
    fn anything_else_gets_no_hint() {
        for command in [
            "",
            "cat",
            "grep foo",
            "tail -f app.log",
            "tail -F app.log",
            "tail --follow app.log",
            "sed -i 's/a/b/' f.txt",
            "sed -n '1p'",
            "sed 's/a/b/' f.txt",
            "find . -name '*.tmp' -delete",
            "find . -name '*.rs' -exec wc -l {} \\;",
            "cat a > b",
            "cat a >> b",
            "cat <<EOF\nhi\nEOF",
            "cat f && rm f",
            "cat f; echo done",
            "cat f || true",
            "cat f &",
            "cat $(ls)",
            "cat `ls`",
            "cat \"$(pwd)/f\"",
            "echo hi | cat",
            "ps aux | grep foo",
            "rg foo | xargs rm",
            "rg foo |",
            "| cat f",
            "git log",
            "cargo test",
            "cat 'unterminated",
            "(cat f)",
        ] {
            assert_eq!(s(command), None, "{command:?}");
        }
    }

    #[test]
    fn quoted_operators_and_escapes_are_words_not_syntax() {
        assert_eq!(s("rg \"a|b\" src"), Some(NativeTool::Grep));
        assert_eq!(s("rg 'a > b' src"), Some(NativeTool::Grep));
        assert_eq!(s("cat my\\ file.txt"), Some(NativeTool::Read));
    }

    #[test]
    fn hints_name_the_tool_and_stay_one_line() {
        for tool in [NativeTool::Read, NativeTool::Grep, NativeTool::Glob] {
            assert!(tool.hint().contains(tool.name()));
            assert!(!tool.hint().contains('\n'));
        }
    }
}
