//! The approval views' one masker. It shows a command the way the shell will run it and hides
//! what is secret: executable text (commands, `$(...)`, `${...}` names) is never replaced, only
//! literal runs are, and only in a secret context or through the caller's scrubber.
//!
//! The danger tokenizer strips quotes, so it cannot render. This is a span-preserving pass over
//! the same quote and substitution scanners (`scan_paren`, `scan_backtick`), so the classifier
//! and the view cannot disagree on where a substitution ends.
use crate::danger::{scan_backtick, scan_paren};

/// Name fragments that make a value secret: `token=...`, `--password ...`.
const SECRET_WORDS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "key",
    "authorization",
    "auth",
    "credential",
];
const SECRET: &str = "<secret>";
/// Nesting cap for substitutions inside substitutions.
const MAX_DEPTH: usize = 16;

/// `command` for display. `scrub` masks known credential shapes (URL userinfo, token formats) in
/// literal text. Separators and quoting stay verbatim; nothing after a masked value is dropped.
pub fn mask_command(command: &str, scrub: &dyn Fn(&str) -> String) -> String {
    mask(command, scrub, 0)
}

/// One piece of a word.
#[derive(Clone)]
enum Part {
    /// Literal text, quotes and escapes included.
    Lit(String),
    /// Text the shell runs or expands and that holds nothing to mask (`$NAME`, `$((1+2))`).
    Verbatim(String),
    /// `$(...)` or a backtick substitution: its inside is a command.
    Cmd {
        open: &'static str,
        inner: String,
        close: &'static str,
    },
    /// `${NAME<op><operand>}`: the head up to the operator, and the operand.
    Param {
        head: String,
        operand: String,
        /// The `${` sat inside double quotes, where `'` is an ordinary character.
        dq: bool,
    },
}

fn is_sep(c: char) -> bool {
    c.is_whitespace() || matches!(c, '&' | '|' | ';' | '<' | '>' | '(' | ')')
}

fn has_secret_word(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SECRET_WORDS.iter().any(|k| name.contains(k))
}

/// A name made of the characters of an option or variable.
fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn mask(command: &str, scrub: &dyn Fn(&str) -> String, depth: usize) -> String {
    if depth > MAX_DEPTH {
        return scrub(command);
    }
    let chars: Vec<char> = command.chars().collect();
    let mut out = String::with_capacity(command.len());
    // The previous word was a flag named like a secret (`--password`): its operand is secret.
    let mut after_flag = false;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if is_sep(c) {
            out.push(c);
            // A shell operator or a line boundary ends the flag's operand. Spaces and tabs
            // within a line do not, so `--password value` still masks `value`, but a newline
            // cannot carry the secret context onto the next command's executable.
            if !c.is_whitespace() || matches!(c, '\n' | '\r') {
                after_flag = false;
            }
            i += 1;
            continue;
        }
        let (mut parts, next) = read_parts(&chars, i, true, false);
        i = next.max(i + 1);
        let first = match parts.first() {
            Some(Part::Lit(text)) => Some(text.clone()),
            _ => None,
        };
        let operand_of_flag = after_flag && !first.as_deref().is_some_and(|t| t.starts_with('-'));
        after_flag = false;
        let mut secret = operand_of_flag;
        if let Some(text) = first.as_deref() {
            match text.find('=') {
                Some(eq) => {
                    let name = text[..eq].trim_start_matches('-');
                    if !secret
                        && is_name(name)
                        && has_secret_word(name)
                        && (eq + 1 < text.len() || parts.len() > 1)
                    {
                        out.push_str(&text[..=eq]);
                        let rest = text[eq + 1..].to_string();
                        if rest.is_empty() {
                            parts.remove(0);
                        } else {
                            parts[0] = Part::Lit(rest);
                        }
                        secret = true;
                    }
                }
                None => {
                    let name = text.trim_start_matches('-');
                    if parts.len() == 1 && text.starts_with('-') && is_name(name) && has_secret_word(name) {
                        after_flag = true;
                    }
                }
            }
        }
        out.push_str(&render(&parts, secret, scrub, depth));
    }
    out
}

fn render(parts: &[Part], secret: bool, scrub: &dyn Fn(&str) -> String, depth: usize) -> String {
    let mut out = String::new();
    for part in parts {
        match part {
            Part::Lit(text) => out.push_str(&if secret { mask_literal(text) } else { scrub(text) }),
            Part::Verbatim(text) => out.push_str(text),
            Part::Cmd { open, inner, close } => {
                out.push_str(open);
                out.push_str(&mask(inner, scrub, depth + 1));
                out.push_str(close);
            }
            Part::Param { head, operand, dq } => {
                out.push_str(head);
                if depth > MAX_DEPTH {
                    out.push_str(&scrub(operand));
                } else {
                    let chars: Vec<char> = operand.chars().collect();
                    let (inner, _) = read_parts(&chars, 0, false, *dq);
                    out.push_str(&render(&inner, secret, scrub, depth + 1));
                }
                out.push('}');
            }
        }
    }
    out
}

/// A literal run in a secret context: `<secret>`, unless it holds only quote and escape
/// characters (a fully expanded value still reads as written). A literal that merely resembles
/// an expansion (a single-quoted `'$(...)'`) is still a value: parsed expansions are separate
/// parts rendered as commands, so no literal bypasses masking on looks alone.
fn mask_literal(text: &str) -> String {
    if text.chars().any(|c| !matches!(c, '"' | '\'' | '\\')) {
        SECRET.to_string()
    } else {
        text.to_string()
    }
}

/// Reads one word (or, with `split` false, the whole text) into parts from `start`. Returns the
/// parts and the index after them. An unterminated quote or substitution makes the rest literal.
fn read_parts(chars: &[char], start: usize, split: bool, init_double: bool) -> (Vec<Part>, usize) {
    let n = chars.len();
    let mut parts: Vec<Part> = Vec::new();
    let mut lit = String::new();
    let (mut single, mut double) = (false, init_double);
    let mut p = start;
    let flush = |parts: &mut Vec<Part>, lit: &mut String| {
        if !lit.is_empty() {
            parts.push(Part::Lit(std::mem::take(lit)));
        }
    };
    while p < n {
        let c = chars[p];
        if split && !single && !double && is_sep(c) {
            break;
        }
        if c == '\\' && !single {
            lit.push(c);
            if let Some(next) = chars.get(p + 1) {
                lit.push(*next);
            }
            p += 2;
            continue;
        }
        if c == '$' && !single && !double && chars.get(p + 1) == Some(&'\'') {
            // `$'...'`: ANSI-C quoting, where a backslash escapes the next character.
            lit.push_str("$'");
            p += 2;
            while p < n && chars[p] != '\'' {
                if chars[p] == '\\' {
                    lit.push('\\');
                    p += 1;
                    if p >= n {
                        break;
                    }
                }
                lit.push(chars[p]);
                p += 1;
            }
            if p < n {
                lit.push('\'');
                p += 1;
            }
            continue;
        }
        if c == '\'' && !double {
            single = !single;
            lit.push(c);
            p += 1;
            continue;
        }
        if c == '"' && !single {
            double = !double;
            lit.push(c);
            p += 1;
            continue;
        }
        if !single && c == '`' {
            match scan_backtick(chars, p + 1) {
                Some((inner, np)) => {
                    flush(&mut parts, &mut lit);
                    parts.push(Part::Cmd {
                        open: "`",
                        inner,
                        close: "`",
                    });
                    p = np;
                }
                None => {
                    lit.extend(&chars[p..]);
                    p = n;
                }
            }
            continue;
        }
        if !single && c == '$' {
            match chars.get(p + 1).copied() {
                Some('(') => match scan_paren(chars, p + 2) {
                    Some((inner, np)) => {
                        flush(&mut parts, &mut lit);
                        if inner.starts_with('(') && inner.ends_with(')') {
                            // `$((1+2))`: arithmetic, kept as written.
                            parts.push(Part::Verbatim(format!("$({inner})")));
                        } else {
                            parts.push(Part::Cmd {
                                open: "$(",
                                inner,
                                close: ")",
                            });
                        }
                        p = np;
                    }
                    None => {
                        lit.extend(&chars[p..]);
                        p = n;
                    }
                },
                Some('{') => match scan_brace(chars, p + 2, double) {
                    Some((inner, np)) => {
                        flush(&mut parts, &mut lit);
                        parts.push(param(&inner, double));
                        p = np;
                    }
                    None => {
                        lit.extend(&chars[p..]);
                        p = n;
                    }
                },
                Some(next) if next.is_alphanumeric() || next == '_' => {
                    flush(&mut parts, &mut lit);
                    let mut name = String::from('$');
                    p += 1;
                    while p < n && (chars[p].is_alphanumeric() || chars[p] == '_') {
                        name.push(chars[p]);
                        p += 1;
                    }
                    parts.push(Part::Verbatim(name));
                }
                // A `$` that cannot start an expansion is literal text.
                _ => {
                    lit.push(c);
                    p += 1;
                }
            }
            continue;
        }
        lit.push(c);
        p += 1;
    }
    flush(&mut parts, &mut lit);
    (parts, p.min(n))
}

/// The text up to the `}` that closes a `${` opened just before `start`, and the index after it.
fn scan_brace(chars: &[char], start: usize, in_double: bool) -> Option<(String, usize)> {
    let mut depth = 1usize;
    let mut s = String::new();
    let mut p = start;
    let (mut single, mut double) = (false, in_double);
    while p < chars.len() {
        let c = chars[p];
        if c == '\\' && !single {
            s.push(c);
            if let Some(next) = chars.get(p + 1) {
                s.push(*next);
            }
            p += 2;
            continue;
        }
        if c == '\'' && !double {
            single = !single;
        } else if c == '"' && !single {
            double = !double;
        } else if !single && c == '$' && chars.get(p + 1) == Some(&'(') {
            let (inner, np) = scan_paren(chars, p + 2)?;
            s.push_str("$(");
            s.push_str(&inner);
            s.push(')');
            p = np;
            continue;
        } else if !single && c == '{' {
            depth += 1;
        } else if !single && c == '}' {
            depth -= 1;
            if depth == 0 {
                return Some((s, p + 1));
            }
        }
        s.push(c);
        p += 1;
    }
    None
}

/// `${NAME<op><operand>}` split into the head (`${NAME<op>`) and its operand. A form with no
/// operator (`${NAME}`, `${#NAME}`) is kept as written.
fn param(inner: &str, dq: bool) -> Part {
    let chars: Vec<char> = inner.chars().collect();
    let mut k = 0usize;
    while k < chars.len() && (chars[k].is_ascii_alphanumeric() || chars[k] == '_') {
        k += 1;
    }
    if k == 0 && !chars.is_empty() {
        k = 1; // a special parameter: `@`, `*`, `#`, `?`, `!`, `$`, `-`
    }
    let rest = &chars[k..];
    let two = |a: char, b: char| rest.first() == Some(&a) && rest.get(1) == Some(&b);
    let op_len = if rest.is_empty() {
        0
    } else if matches!(
        (rest[0], rest.get(1).copied()),
        (':', Some('-' | '=' | '+' | '?'))
            | ('#', Some('#'))
            | ('%', Some('%'))
            | ('/', Some('/'))
            | ('^', Some('^'))
            | (',', Some(','))
    ) || two('/', '#')
        || two('/', '%')
    {
        2
    } else if matches!(rest[0], '-' | '=' | '+' | '?' | '#' | '%' | '/' | '^' | ',' | ':') {
        1
    } else {
        0
    };
    if op_len == 0 {
        return Part::Verbatim(format!("${{{inner}}}"));
    }
    let head: String = chars[..k + op_len].iter().collect();
    let operand: String = chars[k + op_len..].iter().collect();
    Part::Param {
        head: format!("${{{head}"),
        operand,
        dq,
    }
}

#[cfg(test)]
mod tests {
    use super::mask_command;

    fn plain(command: &str) -> String {
        mask_command(command, &|lit| lit.replace("opaque-example", "<scrubbed>"))
    }

    #[test]
    fn a_secret_assignment_keeps_every_executable_part() {
        assert_eq!(
            plain(r#"password="$(printf ')' ; rm -rf /srv/data)" git push --force origin main"#),
            r#"password="$(printf ')' ; rm -rf /srv/data)" git push --force origin main"#
        );
        assert_eq!(
            plain(r#"password="${UNSET:-opaque-example}" git push"#),
            r#"password="${UNSET:-<secret>}" git push"#
        );
        assert_eq!(
            plain(r#"password="${UNSET:+opaque-example}" git push"#),
            r#"password="${UNSET:+<secret>}" git push"#
        );
        assert_eq!(plain("token=demo&&rm -rf x"), "token=<secret>&&rm -rf x");
    }

    #[test]
    fn a_quote_inside_double_quotes_cannot_hide_a_substitution() {
        let out = plain(r#"password="${X:-'}$(rm -rf /srv/data)'}" git push --force origin main"#);
        assert!(out.contains("$(rm -rf /srv/data)"), "{out}");
        assert!(out.contains("git push --force origin main"), "{out}");
    }

    #[test]
    fn an_ansi_c_quote_cannot_hide_the_command_after_it() {
        let out = plain(r#"password=$'\'' rm -rf /srv/data #'"#);
        assert!(out.contains("rm -rf /srv/data"), "{out}");
        assert!(!out.contains("password=<secret>rm"), "{out}");
    }

    #[test]
    fn a_flag_named_like_a_secret_masks_its_operand_only() {
        assert_eq!(
            plain("docker login -u me --password opaque-example registry.invalid && git push --force origin main"),
            "docker login -u me --password <secret> registry.invalid && git push --force origin main"
        );
    }

    #[test]
    fn literals_beside_an_expansion_go_through_the_scrubber() {
        assert_eq!(
            plain(r#"git push "https://u:opaque-example@h.invalid/$REPO" main"#),
            r#"git push "https://u:<scrubbed>@h.invalid/$REPO" main"#
        );
    }

    #[test]
    fn an_unterminated_substitution_keeps_the_rest_as_literal_text() {
        assert_eq!(plain("echo $(opaque-example"), "echo $(<scrubbed>");
        assert_eq!(plain("echo \"opaque-example"), "echo \"<scrubbed>");
    }

    #[test]
    fn nested_substitutions_are_rendered_as_commands() {
        assert_eq!(
            plain(r#"x=$(curl -H "Authorization: opaque-example" https://h.invalid)"#),
            r#"x=$(curl -H "Authorization: <scrubbed>" https://h.invalid)"#
        );
    }

    #[test]
    fn a_single_quoted_credential_that_looks_like_an_expansion_is_masked() {
        // Single quotes make the value literal: the shell never runs these expansions, so the
        // credential must be masked, not returned verbatim because it resembles code.
        assert_eq!(
            plain("login --password 'p$(printf x)' git push"),
            "login --password <secret> git push"
        );
        assert_eq!(
            plain("login --password 'p`id`' git push"),
            "login --password <secret> git push"
        );
        assert_eq!(
            plain("login --password 'p${HOME}' git push"),
            "login --password <secret> git push"
        );
        // A real (double-quoted) substitution still renders as a command, unchanged.
        assert_eq!(
            plain(r#"login --password "$(printf x)" git push"#),
            r#"login --password "$(printf x)" git push"#
        );
    }

    #[test]
    fn a_line_boundary_ends_the_secret_operand_so_the_next_command_stays_visible() {
        // A bare secret flag followed by a newline must not mask the next command's executable.
        assert_eq!(
            plain(": --password\nrm -rf /srv/data"),
            ": --password\nrm -rf /srv/data"
        );
        assert_eq!(
            plain(": --password\r\nrm -rf /srv/data"),
            ": --password\r\nrm -rf /srv/data"
        );
        // The operand on the flag's own line is still masked; the next line stays visible.
        assert_eq!(
            plain("login --password opaque-example\nrm -rf /srv/data"),
            "login --password <secret>\nrm -rf /srv/data"
        );
    }
}
