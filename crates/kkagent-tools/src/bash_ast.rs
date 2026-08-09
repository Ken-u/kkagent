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
                        || matches!(ch, '|' | '&' | ';' | '(' | ')' | '<' | '>')
                    {
                        break;
                    }
                    s.push(ch);
                    i += 1;
                }
                out.push(Token::Word(s));
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
        if matches!(t, Token::Semi | Token::Newline | Token::AndAnd | Token::OrOr) {
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

fn parse_command(tokens: &[Token]) -> AstNode {
    let mut words = Vec::new();
    let mut redirect_op = None;
    let mut redirect_target = None;
    let mut i = 0usize;
    while i < tokens.len() {
        match &tokens[i] {
            Token::Word(w) => {
                if w.contains('=') && !w.starts_with('=') && words.is_empty() {
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
            Token::LParen => {
                // naive: rest as subshell words
                let inner: Vec<Token> = tokens[i + 1..]
                    .iter()
                    .take_while(|t| !matches!(t, Token::RParen))
                    .cloned()
                    .collect();
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
    let cmd = AstNode::Command { name, args };
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
        AstNode::Assignment { .. } => vec![],
        AstNode::Redirect { inner, .. } | AstNode::Subshell(inner) => collect_commands(inner),
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
        AstNode::Redirect { inner, .. } | AstNode::Subshell(inner) => pipes_into_shell(inner),
        _ => false,
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
}
