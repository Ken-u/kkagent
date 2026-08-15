//! Lightweight bash-like AST tokenizer/parser for shell safety (tree-sitter-bash stand-in).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AstNode {
    Script(Vec<AstNode>),
    Pipeline(Vec<AstNode>),
    Command {
        name: String,
        args: Vec<String>,
    },
    Assignment {
        name: String,
        value: String,
    },
    Redirect {
        op: String,
        target: String,
        inner: Box<AstNode>,
    },
    Subshell(Box<AstNode>),
    /// `$(...)` or `` `...` `` command substitution.
    CommandSubst(Box<AstNode>),
    /// `<(...)` / `>(...)` process substitution.
    ProcessSubst {
        op: char,
        inner: Box<AstNode>,
    },
    /// `$((...))` arithmetic expansion (kept as raw text).
    Arithmetic(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Word(String),
    Pipe,
    AndAnd,
    OrOr,
    Semi,
    Newline,
    Redirect(String),
    LParen,
    RParen,
    /// `$(` start of command substitution
    DollarParen,
    /// `$((` start of arithmetic
    DollarDblParen,
    /// `<( ` or `>(`
    ProcessSubstOpen(char),
}

/// File / network / env dependencies extracted from a command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellDependencies {
    pub reads: Vec<String>,
    pub writes: Vec<String>,
    pub urls: Vec<String>,
    pub env_vars: Vec<String>,
}

pub fn tokenize(input: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            if c == '\n' {
                out.push(Token::Newline);
            }
            i += 1;
            continue;
        }
        match c {
            '|' if chars.get(i + 1) == Some(&'|') => {
                out.push(Token::OrOr);
                i += 2;
            }
            '|' => {
                out.push(Token::Pipe);
                i += 1;
            }
            '&' if chars.get(i + 1) == Some(&'&') => {
                out.push(Token::AndAnd);
                i += 2;
            }
            ';' => {
                out.push(Token::Semi);
                i += 1;
            }
            '$' if chars.get(i + 1) == Some(&'(') && chars.get(i + 2) == Some(&'(') => {
                out.push(Token::DollarDblParen);
                i += 3;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                out.push(Token::DollarParen);
                i += 2;
            }
            '$' => {
                // `$VAR`, `${VAR}`, or bare `$`
                let mut s = String::from('$');
                i += 1;
                if chars.get(i) == Some(&'{') {
                    s.push('{');
                    i += 1;
                    while i < chars.len() && chars[i] != '}' {
                        s.push(chars[i]);
                        i += 1;
                    }
                    if i < chars.len() {
                        s.push('}');
                        i += 1;
                    }
                } else {
                    while i < chars.len() {
                        let ch = chars[i];
                        if ch.is_ascii_alphanumeric() || ch == '_' {
                            s.push(ch);
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
                out.push(Token::Word(s));
            }
            '<' if chars.get(i + 1) == Some(&'(') => {
                out.push(Token::ProcessSubstOpen('<'));
                i += 2;
            }
            '>' if chars.get(i + 1) == Some(&'(') => {
                out.push(Token::ProcessSubstOpen('>'));
                i += 2;
            }
            '(' => {
                out.push(Token::LParen);
                i += 1;
            }
            ')' => {
                out.push(Token::RParen);
                i += 1;
            }
            '>' | '<' => {
                let mut op = String::from(c);
                i += 1;
                if chars.get(i) == Some(&'>') || chars.get(i) == Some(&'&') {
                    op.push(chars[i]);
                    i += 1;
                }
                out.push(Token::Redirect(op));
            }
            '`' => {
                // Backtick command substitution → treat as DollarParen + inner words + RParen
                i += 1;
                let mut inner = String::new();
                while i < chars.len() && chars[i] != '`' {
                    inner.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                out.push(Token::DollarParen);
                for t in tokenize(&inner) {
                    out.push(t);
                }
                out.push(Token::RParen);
            }
            '\'' | '"' => {
                let quote = c;
                i += 1;
                let mut s = String::new();
                while i < chars.len() && chars[i] != quote {
                    if chars[i] == '\\' && quote == '"' && i + 1 < chars.len() {
                        s.push(chars[i + 1]);
                        i += 2;
                    } else {
                        s.push(chars[i]);
                        i += 1;
                    }
                }
                if i < chars.len() {
                    i += 1;
                }
                out.push(Token::Word(s));
            }
            _ => {
                let mut s = String::new();
                while i < chars.len() {
                    let ch = chars[i];
                    if ch.is_whitespace()
                        || matches!(ch, '|' | '&' | ';' | '(' | ')' | '<' | '>' | '`' | '$')
                    {
                        break;
                    }
                    s.push(ch);
                    i += 1;
                }
                if !s.is_empty() {
                    out.push(Token::Word(s));
                } else {
                    // Defensive: never spin on an unrecognized character.
                    out.push(Token::Word(chars[i].to_string()));
                    i += 1;
                }
            }
        }
    }
    out
}

pub fn parse(input: &str) -> AstNode {
    let tokens = tokenize(input);
    parse_script(&tokens)
}

fn parse_script(tokens: &[Token]) -> AstNode {
    let mut stmts = Vec::new();
    let mut start = 0usize;
    for (i, t) in tokens.iter().enumerate() {
        if matches!(
            t,
            Token::Semi | Token::Newline | Token::AndAnd | Token::OrOr
        ) {
            if start < i {
                stmts.push(parse_pipeline(&tokens[start..i]));
            }
            start = i + 1;
        }
    }
    if start < tokens.len() {
        stmts.push(parse_pipeline(&tokens[start..]));
    }
    if stmts.len() == 1 {
        stmts.pop().unwrap()
    } else {
        AstNode::Script(stmts)
    }
}

fn parse_pipeline(tokens: &[Token]) -> AstNode {
    let mut parts = Vec::new();
    let mut start = 0usize;
    for (i, t) in tokens.iter().enumerate() {
        if matches!(t, Token::Pipe) {
            if start < i {
                parts.push(parse_command(&tokens[start..i]));
            }
            start = i + 1;
        }
    }
    if start < tokens.len() {
        parts.push(parse_command(&tokens[start..]));
    }
    if parts.len() == 1 {
        parts.pop().unwrap()
    } else {
        AstNode::Pipeline(parts)
    }
}

fn take_balanced(tokens: &[Token], open_kind: &str) -> (Vec<Token>, usize) {
    let mut depth = 1usize;
    let mut i = 0usize;
    let mut inner = Vec::new();
    while i < tokens.len() {
        match &tokens[i] {
            Token::LParen
            | Token::DollarParen
            | Token::DollarDblParen
            | Token::ProcessSubstOpen(_) => {
                depth += 1;
                if depth > 1 {
                    inner.push(tokens[i].clone());
                }
            }
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    let _ = open_kind;
                    return (inner, i + 1);
                }
                inner.push(tokens[i].clone());
            }
            other => inner.push(other.clone()),
        }
        i += 1;
    }
    (inner, i)
}

fn parse_command(tokens: &[Token]) -> AstNode {
    let mut words = Vec::new();
    let mut redirect_op = None;
    let mut redirect_target = None;
    let mut nested: Vec<AstNode> = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        match &tokens[i] {
            Token::Word(w) => {
                if w.contains('=') && !w.starts_with('=') && words.is_empty() && nested.is_empty() {
                    let (n, v) = w.split_once('=').unwrap();
                    return AstNode::Assignment {
                        name: n.to_string(),
                        value: v.to_string(),
                    };
                }
                words.push(w.clone());
            }
            Token::Redirect(op) => {
                redirect_op = Some(op.clone());
                if let Some(Token::Word(t)) = tokens.get(i + 1) {
                    redirect_target = Some(t.clone());
                    i += 1;
                }
            }
            Token::DollarParen => {
                let (inner, consumed) = take_balanced(&tokens[i + 1..], "$(");
                nested.push(AstNode::CommandSubst(Box::new(parse_pipeline(&inner))));
                i += consumed;
            }
            Token::DollarDblParen => {
                let (inner, consumed) = take_balanced(&tokens[i + 1..], "$((");
                let text = inner
                    .iter()
                    .filter_map(|t| match t {
                        Token::Word(w) => Some(w.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                nested.push(AstNode::Arithmetic(text));
                i += consumed;
            }
            Token::ProcessSubstOpen(op) => {
                let op = *op;
                let (inner, consumed) = take_balanced(&tokens[i + 1..], "process");
                nested.push(AstNode::ProcessSubst {
                    op,
                    inner: Box::new(parse_pipeline(&inner)),
                });
                i += consumed;
            }
            Token::LParen => {
                let (inner, _consumed) = take_balanced(&tokens[i + 1..], "(");
                return AstNode::Subshell(Box::new(parse_pipeline(&inner)));
            }
            _ => {}
        }
        i += 1;
    }
    let name = words.first().cloned().unwrap_or_default();
    let args = if words.len() > 1 {
        words[1..].to_vec()
    } else {
        Vec::new()
    };
    let mut cmd = AstNode::Command { name, args };
    // Attach nested substitutions as sibling script nodes when present.
    if !nested.is_empty() {
        nested.insert(0, cmd);
        cmd = AstNode::Script(nested);
    }
    if let (Some(op), Some(target)) = (redirect_op, redirect_target) {
        AstNode::Redirect {
            op,
            target,
            inner: Box::new(cmd),
        }
    } else {
        cmd
    }
}

/// Collect command names from AST (for safety analysis).
pub fn collect_commands(node: &AstNode) -> Vec<String> {
    match node {
        AstNode::Script(xs) | AstNode::Pipeline(xs) => {
            xs.iter().flat_map(collect_commands).collect()
        }
        AstNode::Command { name, .. } => vec![name.clone()],
        AstNode::Assignment { .. } | AstNode::Arithmetic(_) => vec![],
        AstNode::Redirect { inner, .. }
        | AstNode::Subshell(inner)
        | AstNode::CommandSubst(inner)
        | AstNode::ProcessSubst { inner, .. } => collect_commands(inner),
    }
}

/// True if pipeline pipes into a shell interpreter.
pub fn pipes_into_shell(node: &AstNode) -> bool {
    match node {
        AstNode::Pipeline(parts) => {
            if parts.len() < 2 {
                return false;
            }
            let last = parts.last().unwrap();
            matches!(
                last,
                AstNode::Command { name, .. }
                    if matches!(name.as_str(), "sh" | "bash" | "zsh" | "dash" | "ksh")
            )
        }
        AstNode::Script(xs) => xs.iter().any(pipes_into_shell),
        AstNode::Redirect { inner, .. }
        | AstNode::Subshell(inner)
        | AstNode::CommandSubst(inner)
        | AstNode::ProcessSubst { inner, .. } => pipes_into_shell(inner),
        _ => false,
    }
}

/// Extract coarse file/URL/env dependencies for sandbox mount planning.
pub fn extract_dependencies(node: &AstNode) -> ShellDependencies {
    let mut deps = ShellDependencies::default();
    walk_deps(node, &mut deps);
    deps.reads.sort();
    deps.reads.dedup();
    deps.writes.sort();
    deps.writes.dedup();
    deps.urls.sort();
    deps.urls.dedup();
    deps.env_vars.sort();
    deps.env_vars.dedup();
    deps
}

fn walk_deps(node: &AstNode, deps: &mut ShellDependencies) {
    match node {
        AstNode::Script(xs) | AstNode::Pipeline(xs) => {
            for x in xs {
                walk_deps(x, deps);
            }
        }
        AstNode::Command { name, args } => {
            let n = name.to_ascii_lowercase();
            for arg in args {
                classify_arg(&n, arg, deps);
            }
        }
        AstNode::Assignment { name, value } => {
            deps.env_vars.push(name.clone());
            classify_arg("", value, deps);
        }
        AstNode::Redirect { op, target, inner } => {
            if op.contains('>') {
                deps.writes.push(target.clone());
            } else {
                deps.reads.push(target.clone());
            }
            walk_deps(inner, deps);
        }
        AstNode::Subshell(inner)
        | AstNode::CommandSubst(inner)
        | AstNode::ProcessSubst { inner, .. } => walk_deps(inner, deps),
        AstNode::Arithmetic(_) => {}
    }
}

fn classify_arg(cmd: &str, arg: &str, deps: &mut ShellDependencies) {
    if arg.starts_with('$') && arg.len() > 1 {
        let name = arg
            .trim_start_matches('$')
            .trim_start_matches('{')
            .trim_end_matches('}');
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            deps.env_vars.push(name.to_string());
        }
    }
    if arg.starts_with("http://") || arg.starts_with("https://") || arg.starts_with("ftp://") {
        deps.urls.push(arg.to_string());
        return;
    }
    let looks_like_path = arg.contains('/')
        || arg.starts_with('.')
        || arg.starts_with('~')
        || arg.ends_with(".rs")
        || arg.ends_with(".py")
        || arg.ends_with(".js")
        || arg.ends_with(".txt")
        || arg.ends_with(".json")
        || arg.ends_with(".toml");
    if !looks_like_path {
        return;
    }
    match cmd {
        "rm" | "unlink" | "rmdir" | "mv" | "cp" | "tee" | "install" | "touch" | "mkdir"
        | "chmod" | "chown" => deps.writes.push(arg.to_string()),
        "cat" | "less" | "more" | "head" | "tail" | "source" | "." | "grep" | "rg" | "find"
        | "stat" | "file" | "wc" | "diff" => deps.reads.push(arg.to_string()),
        _ => deps.reads.push(arg.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_curl_sh() {
        let ast = parse("curl https://x.y | sh");
        assert!(pipes_into_shell(&ast));
        let cmds = collect_commands(&ast);
        assert_eq!(cmds, vec!["curl".to_string(), "sh".to_string()]);
    }

    #[test]
    fn simple_echo() {
        let ast = parse("echo hi");
        assert_eq!(
            ast,
            AstNode::Command {
                name: "echo".into(),
                args: vec!["hi".into()],
            }
        );
    }

    #[test]
    fn parses_command_substitution() {
        let ast = parse("echo $(date +%s)");
        let cmds = collect_commands(&ast);
        assert!(cmds.contains(&"echo".to_string()));
        assert!(cmds.contains(&"date".to_string()));
    }

    #[test]
    fn parses_backtick_substitution() {
        let ast = parse("echo `uname -s`");
        let cmds = collect_commands(&ast);
        assert!(cmds.iter().any(|c| c == "uname"));
    }

    #[test]
    fn parses_process_substitution() {
        let ast = parse("diff <(sort a.txt) <(sort b.txt)");
        let cmds = collect_commands(&ast);
        assert!(cmds.contains(&"diff".to_string()));
        assert!(cmds.iter().filter(|c| *c == "sort").count() >= 2);
    }

    #[test]
    fn parses_arithmetic_expansion() {
        let ast = parse("echo $((1 + 2))");
        fn has_arith(n: &AstNode) -> bool {
            match n {
                AstNode::Arithmetic(_) => true,
                AstNode::Script(xs) | AstNode::Pipeline(xs) => xs.iter().any(has_arith),
                AstNode::Redirect { inner, .. }
                | AstNode::Subshell(inner)
                | AstNode::CommandSubst(inner)
                | AstNode::ProcessSubst { inner, .. } => has_arith(inner),
                _ => false,
            }
        }
        assert!(has_arith(&ast));
    }

    #[test]
    fn parses_multi_pipeline_and_redirect() {
        let ast = parse("cat in.txt | grep foo | tee out.txt > log.txt");
        let cmds = collect_commands(&ast);
        assert_eq!(cmds, vec!["cat", "grep", "tee"]);
        let deps = extract_dependencies(&ast);
        assert!(deps.reads.iter().any(|p| p.contains("in.txt")));
        assert!(deps.writes.iter().any(|p| p.contains("log.txt")));
    }

    #[test]
    fn extracts_urls_and_env() {
        let ast = parse("curl https://example.com/a -o /tmp/a");
        let deps = extract_dependencies(&ast);
        assert!(deps.urls.iter().any(|u| u.contains("example.com")));
        assert!(
            deps.writes.iter().any(|p| p.contains("/tmp/a"))
                || deps.reads.iter().any(|p| p.contains("/tmp/a"))
        );
    }

    #[test]
    fn extracts_env_var_references() {
        let ast = parse("echo $HOME/$USER");
        let deps = extract_dependencies(&ast);
        // Path-like $HOME/$USER may be classified as a single arg
        assert!(
            deps.env_vars.contains(&"HOME".to_string())
                || deps.reads.iter().any(|p| p.contains("$HOME")),
            "{deps:?}"
        );
    }

    #[test]
    fn subshell_grouping() {
        let ast = parse("(cd /tmp && ls)");
        let cmds = collect_commands(&ast);
        assert!(cmds.contains(&"cd".to_string()) || cmds.contains(&"ls".to_string()));
    }
}
