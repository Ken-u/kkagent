//! Control-character sanitizing for terminal-bound text.
//!
//! Tool output (build logs, grep hits, file contents) and LLM text can carry
//! raw ANSI/OSC/DCS escape sequences. ratatui writes cell contents verbatim,
//! so an injected `ESC` reaches the terminal and reinterprets our output: at
//! best colors shift, at worst an OSC string gets truncated mid-sequence by
//! line wrapping and the terminal swallows every following byte — including
//! ratatui's own cursor moves — leaving permanently corrupted frames ("ghost"
//! artifacts over SSH). Strip control characters at the last hop before
//! rendering; keep `\n` and `\t` (renderers split lines themselves) and strip
//! the rest — including C1 bytes, which some terminals honor as 8-bit controls.

/// One-line, allocation-light cleaner for untrusted terminal-bound text.
///
/// - Passes through: `\t`, `\n`, `\r` (renderers split lines themselves; `\r`
///   is kept so `lines()`-based height math is unchanged), and all
///   printable/graphic characters including wide CJK and emoji.
/// - Recognized escape sequences (CSI/OSC/DCS/SOS/PM/APC, and ESC-prefixed
///   two-byte codes like `ESC 7` / `ESC M` / `ESC ( B`) are removed whole, so
///   nothing partial ever reaches the terminal.
/// - A lone ESC followed by something unrecognized, or truncated at end of
///   input, is replaced with U+FFFD and the following char is kept — visible
///   corruption beats silently swallowing user data.
/// - Other C0 controls, DEL and C1 (some terminals honor C1 as 8-bit
///   controls, e.g. 0x9B acts as CSI) are replaced with U+FFFD: one visible
///   char per control keeps column math stable and makes the loss diagnosable.
pub fn sanitize_text(input: &str) -> std::borrow::Cow<'_, str> {
    if !needs_cleaning(input) {
        return std::borrow::Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        match ch {
            '\u{1b}' => match skip_escape_sequence(&mut chars) {
                SequenceResult::Removed => {}
                SequenceResult::BareEsc => out.push('\u{fffd}'),
            },
            '\t' | '\n' | '\r' => out.push(ch),
            c if (c as u32) < 0x20 || (0x7f..=0x9f).contains(&(c as u32)) => {
                out.push('\u{fffd}');
            }
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Fast path: bytes that never need rewriting let us skip allocation.
fn needs_cleaning(input: &str) -> bool {
    // ESC and C0 (minus \t \n \r) are ASCII; DEL/C1 start at 0x7f. Checking
    // bytes is sufficient: multi-byte UTF-8 sequences all have the high bit
    // set but never decode into the 0x7f..=0x9f *character* range (those C1
    // codepoints encode as two bytes starting with 0xC2).
    input.bytes().any(|b| {
        b == 0x1b || (b < 0x20 && b != b'\t' && b != b'\n' && b != b'\r') || b == 0x7f || b == 0xc2
    })
}

/// Consume an escape sequence started by an already-consumed ESC.
enum SequenceResult {
    /// A recognized sequence was fully consumed.
    Removed,
    /// Not a recognized sequence: just the ESC itself (caller replaces it).
    BareEsc,
}

/// Consume a sequence started by ESC. Unrecognized ESC + char combinations
/// only sacrifice the ESC itself; the following char stays for normal
/// processing, so a stray ESC inside otherwise-valid text never swallows
/// user content.
fn skip_escape_sequence(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) -> SequenceResult {
    // CSI: ESC [ params(0x30-0x3F) intermediates(0x20-0x2F) final(0x40-0x7E)
    if matches!(chars.peek().map(|&(_, c)| c), Some('[')) {
        chars.next();
        for (_, c) in chars.by_ref() {
            if ('\u{40}'..='\u{7e}').contains(&c) {
                break; // final byte terminates CSI
            }
            if !('\u{20}'..='\u{3f}').contains(&c) {
                break; // malformed: stop consuming, keep the rest as text
            }
        }
        return SequenceResult::Removed;
    }
    // OSC/DCS/SOS/PM/APC: string sequences terminated by ST (ESC \) or BEL.
    if matches!(
        chars.peek().map(|&(_, c)| c),
        Some(']') | Some('P') | Some('X') | Some('^') | Some('_')
    ) {
        chars.next(); // consume the introducer
        let mut prev_esc = false;
        for (_, c) in chars.by_ref() {
            if prev_esc && c == '\\' {
                break; // ST
            }
            prev_esc = c == '\u{1b}';
            if c == '\u{7}' {
                break; // BEL terminator
            }
        }
        return SequenceResult::Removed;
    }
    // Two-byte ESC codes: ESC M / ESC 7 / ESC 8 / ESC = / ESC > / ESC ( B …
    // The introducer set is small and closed; anything else (letters that
    // could be payload, e.g. `ESC b` inside prose) is treated as bare ESC.
    if let Some(&(_, c)) = chars.peek() {
        if matches!(
            c,
            'M' | '7' | '8' | '=' | '>' | 'D' | 'E' | 'c' | '(' | ')' | '#' | '%'
        ) {
            chars.next();
            // Charset designations carry one more byte: ESC ( B
            if matches!(c, '(' | ')' | '#' | '%') && chars.peek().is_some() {
                chars.next();
            }
            return SequenceResult::Removed;
        }
    }
    SequenceResult::BareEsc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(s: &str) -> String {
        match sanitize_text(s) {
            std::borrow::Cow::Borrowed(b) => panic!("expected owned: {b:?}"),
            std::borrow::Cow::Owned(o) => o,
        }
    }

    #[test]
    fn clean_text_passes_through_borrowed() {
        assert!(matches!(
            sanitize_text("hello 世界 🌏\n\ttabbed"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn ansi_csi_sequences_are_stripped() {
        assert_eq!(owned("\x1b[31mRED\x1b[0m"), "RED");
        assert_eq!(owned("\x1b[1;38;2;10;20;30mX\x1b[mY"), "XY");
    }

    #[test]
    fn osc_sequences_are_stripped_including_unterminated() {
        assert_eq!(owned("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"), "link");
        // Truncated OSC (the ghost-frame trigger) must swallow to EOT.
        assert_eq!(owned("\x1b]0;title"), "");
        assert_eq!(owned("before\x1b]52;c;"), "before");
    }

    #[test]
    fn osc_bel_terminator_is_handled() {
        assert_eq!(owned("\x1b]0;bell-title\x07after"), "after");
    }

    #[test]
    fn bare_esc_and_c0_are_replaced_not_dropped() {
        assert_eq!(owned("a\x1bb"), "a\u{fffd}b"); // stray ESC keeps the payload
        assert_eq!(owned("a\u{7f}b"), "a\u{fffd}b");
        assert_eq!(owned("a\u{0}b"), "a\u{fffd}b");
        assert_eq!(owned("a\u{9b}0mb"), "a\u{fffd}0mb"); // C1 CSI byte as text
        assert_eq!(owned("trailing\x1b"), "trailing\u{fffd}"); // truncated at EOT
        assert_eq!(owned("\x1b7save\x1b8"), "save"); // two-byte codes removed
        assert_eq!(owned("\x1b(B latin"), " latin"); // charset designation
    }

    #[test]
    fn newlines_tabs_survive() {
        // No byte triggers the cleaning path: stays borrowed, zero alloc.
        assert!(matches!(
            sanitize_text("line1\nline2\r\n\tindented"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn dcs_and_sos_sequences_are_stripped() {
        assert_eq!(owned("\x1bP+q544e\x1b\\rest"), "rest");
        assert_eq!(owned("\x1bXdata\x1b\\tail"), "tail");
    }
}
