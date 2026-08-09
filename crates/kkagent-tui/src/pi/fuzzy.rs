//! Fuzzy match — aligned with ref `pi-tui/fuzzy.ts`.

#[derive(Debug, Clone, Copy)]
pub struct FuzzyMatch {
    pub matches: bool,
    /// Lower is better.
    pub score: f64,
}

pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let q = query.to_lowercase();
    let t = text.to_lowercase();
    if q.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    if q.len() > t.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }

    let q_chars: Vec<char> = q.chars().collect();
    let t_chars: Vec<char> = t.chars().collect();
    let mut qi = 0usize;
    let mut score = 0.0f64;
    let mut last = -1isize;
    let mut consec = 0i32;

    for (i, &ch) in t_chars.iter().enumerate() {
        if qi >= q_chars.len() {
            break;
        }
        if ch == q_chars[qi] {
            let boundary =
                i == 0 || matches!(t_chars[i - 1], ' ' | '-' | '_' | '.' | '/' | ':' | '\\');
            if last == i as isize - 1 {
                consec += 1;
                score -= f64::from(consec) * 5.0;
            } else {
                consec = 0;
                if last >= 0 {
                    score += (i as isize - last - 1) as f64 * 2.0;
                }
            }
            if boundary {
                score -= 10.0;
            }
            score += i as f64 * 0.1;
            last = i as isize;
            qi += 1;
        }
    }

    if qi < q_chars.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }
    if q == t {
        score -= 100.0;
    }
    FuzzyMatch {
        matches: true,
        score,
    }
}

pub fn fuzzy_filter<'a>(
    query: &str,
    items: impl IntoIterator<Item = &'a str>,
) -> Vec<(usize, f64)> {
    let mut out = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        let m = fuzzy_match(query, item);
        if m.matches {
            out.push((i, m.score));
        }
    }
    out.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_best() {
        let a = fuzzy_match("read", "read");
        let b = fuzzy_match("read", "ready");
        assert!(a.matches && b.matches);
        assert!(a.score < b.score);
    }
}
