//! Word-boundary cursor movement.

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

pub fn move_word_left(text: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let bytes: Vec<(usize, char)> = text[..cursor].char_indices().collect();
    if bytes.is_empty() {
        return 0;
    }
    let mut i = bytes.len();
    // skip trailing whitespace
    while i > 0 && bytes[i - 1].1.is_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    let wordish = is_word(bytes[i - 1].1);
    while i > 0 && is_word(bytes[i - 1].1) == wordish && !bytes[i - 1].1.is_whitespace() {
        i -= 1;
    }
    bytes.get(i).map(|(b, _)| *b).unwrap_or(0)
}

pub fn move_word_right(text: &str, cursor: usize) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    let rest: Vec<(usize, char)> = text[cursor..]
        .char_indices()
        .map(|(i, c)| (cursor + i, c))
        .collect();
    if rest.is_empty() {
        return text.len();
    }
    let mut i = 0usize;
    while i < rest.len() && rest[i].1.is_whitespace() {
        i += 1;
    }
    if i >= rest.len() {
        return text.len();
    }
    let wordish = is_word(rest[i].1);
    while i < rest.len() && is_word(rest[i].1) == wordish && !rest[i].1.is_whitespace() {
        i += 1;
    }
    if i >= rest.len() {
        text.len()
    } else {
        rest[i].0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        let t = "hello world";
        assert_eq!(move_word_left(t, 11), 6);
        assert_eq!(move_word_right(t, 0), 5);
    }
}
