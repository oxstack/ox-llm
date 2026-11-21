//! Lead text as safe markup: the one place a model's words become HTML.
//!
//! Escaping happens here and nowhere else for lead text. Inline HTML in the source
//! arrives as raw-text events and is escaped, so a reply can never open an element.
//! A link keeps its destination only when it is a same-origin absolute path or a web
//! page. A scheme of its own, a protocol-relative host and a backslash path keep their
//! text and get no link.
//! `#123` becomes the item page. Nothing here can change permission, owner authority
//! or a prompt: it only decides how text is shown.
use super::escape;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::fmt::Write;

/// The most link targets one reply renders; a longer tail renders as text.
const MAX_LINKS: usize = 64;

/// Rendered Markdown blocks, one `String` per top-level block. The caller decides the
/// wrapper (`div` for a bubble, `p` for one line), so the renderer holds no page layout.
pub(super) fn blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    // Within one top-level block: a list or a table has no paragraph wrapper.
    let mut block_paragraphs = 0usize;
    let mut links = 0usize;
    let mut in_link = false;
    let mut lists: Vec<&'static str> = Vec::new();
    let mut in_code = false;
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(tag) => {
                if let Tag::Paragraph = tag {
                    if depth == 0 {
                        current.clear();
                        block_paragraphs = 0;
                    }
                    block_paragraphs += 1;
                    current.push_str(if block_paragraphs > 1 { "</p><p>" } else { "<p>" });
                    depth += 1;
                    continue;
                }
                if depth == 0 {
                    current.clear();
                }
                depth += 1;
                match tag {
                    Tag::Paragraph => {}
                    Tag::Heading { level, .. } => {
                        let _ = write!(current, "<h{} class=md-h{}>", level as u8, level as u8);
                    }
                    Tag::BlockQuote(_) => current.push_str("<blockquote>"),
                    Tag::CodeBlock(kind) => {
                        let language = match &kind {
                            CodeBlockKind::Fenced(info) => info.split_whitespace().next().filter(|word| {
                                !word.is_empty()
                                    && word.len() <= 32
                                    && word.chars().all(|c| {
                                        c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '#' | '.')
                                    })
                            }),
                            CodeBlockKind::Indented => None,
                        };
                        in_code = true;
                        current.push_str(
                            "<div class=code><button type=button class=copy data-copy=1>Copy</button>",
                        );
                        let _ = write!(
                            current,
                            "<span class=lang>{}</span><pre><code>",
                            escape(language.unwrap_or("code"))
                        );
                    }
                    Tag::List(start) => match start {
                        Some(start) => {
                            let _ = write!(current, "<ol start={start}>");
                            lists.push("</ol>");
                        }
                        None => {
                            current.push_str("<ul>");
                            lists.push("</ul>");
                        }
                    },
                    Tag::Item => current.push_str("<li>"),
                    Tag::Emphasis => current.push_str("<em>"),
                    Tag::Strong => current.push_str("<strong>"),
                    Tag::Strikethrough => current.push_str("<del>"),
                    Tag::Link { dest_url, .. } => {
                        let target = (!in_link && links < MAX_LINKS)
                            .then(|| local_target(&dest_url))
                            .flatten();
                        match target {
                            Some(target) => {
                                links += 1;
                                in_link = true;
                                let _ = write!(current, "<a href=\"{}\" rel=noreferrer>", escape(&target));
                            }
                            None => current.push_str("<span class=link>"),
                        }
                    }
                    Tag::Table(_) => current.push_str("<div class=scroll><table>"),
                    Tag::TableHead => current.push_str("<thead><tr>"),
                    Tag::TableRow => current.push_str("<tr>"),
                    Tag::TableCell => current.push_str("<th>"),
                    Tag::FootnoteDefinition(_) => current.push_str("<div class=note>"),
                    Tag::Image { .. } => current.push_str("<span class=alt>"),
                    Tag::DefinitionList => current.push_str("<dl>"),
                    Tag::DefinitionListTitle => current.push_str("<dt>"),
                    Tag::DefinitionListDefinition => current.push_str("<dd>"),
                    Tag::Superscript => current.push_str("<sup>"),
                    Tag::Subscript => current.push_str("<sub>"),
                    // Rules, HTML blocks and metadata carry no text the owner asked for.
                    Tag::HtmlBlock | Tag::MetadataBlock(_) => {}
                }
            }
            Event::End(tag) => {
                depth = depth.saturating_sub(1);
                match tag {
                    TagEnd::Paragraph => {
                        current.push_str("</p>");
                        if depth == 0 {
                            blocks.push(std::mem::take(&mut current));
                        }
                        continue;
                    }
                    TagEnd::CodeBlock => {
                        in_code = false;
                        current.push_str("</code></pre></div>");
                    }
                    TagEnd::Heading(level) => {
                        let _ = write!(current, "</h{}>", level as u8);
                    }
                    TagEnd::BlockQuote(_) => current.push_str("</blockquote>"),
                    TagEnd::List(_) => current.push_str(lists.pop().unwrap_or("</ul>")),
                    TagEnd::Item => current.push_str("</li>"),
                    TagEnd::Emphasis => current.push_str("</em>"),
                    TagEnd::Strong => current.push_str("</strong>"),
                    TagEnd::Strikethrough => current.push_str("</del>"),
                    TagEnd::Link => {
                        current.push_str(if in_link { "</a>" } else { "</span>" });
                        in_link = false;
                    }
                    TagEnd::Table => current.push_str("</table></div>"),
                    TagEnd::TableHead => current.push_str("</th></tr></thead>"),
                    TagEnd::TableRow => current.push_str("</tr>"),
                    TagEnd::TableCell => current.push_str("</td>"),
                    TagEnd::FootnoteDefinition => current.push_str("</div>"),
                    TagEnd::Image => current.push_str("</span>"),
                    TagEnd::DefinitionList => current.push_str("</dl>"),
                    TagEnd::DefinitionListTitle => current.push_str("</dt>"),
                    TagEnd::DefinitionListDefinition => current.push_str("</dd>"),
                    TagEnd::Superscript => current.push_str("</sup>"),
                    TagEnd::Subscript => current.push_str("</sub>"),
                    TagEnd::HtmlBlock | TagEnd::MetadataBlock(_) => {}
                }
                if depth == 0 && !current.is_empty() {
                    blocks.push(std::mem::take(&mut current));
                }
            }
            Event::Text(text) if in_code => current.push_str(&escape_multiline(&text)),
            Event::Text(text) if in_link => current.push_str(&escape(&text)),
            Event::Text(text) => current.push_str(&mentions(&text)),
            Event::Code(code) => {
                let _ = write!(current, "<code>{}</code>", escape(&code));
            }
            // Raw HTML is text, shown as the source wrote it and never as an element.
            Event::Html(raw) | Event::InlineHtml(raw) => current.push_str(&escape(&raw)),
            Event::SoftBreak => current.push(' '),
            Event::HardBreak => current.push_str("<br>"),
            Event::Rule => {
                if depth == 0 {
                    current.push_str("<hr>");
                    blocks.push(std::mem::take(&mut current));
                }
            }
            Event::TaskListMarker(done) => {
                current.push_str(if done {
                    "<span class=task aria-hidden=true>☑</span>"
                } else {
                    "<span class=task aria-hidden=true>☐</span>"
                });
            }
            Event::FootnoteReference(label) => {
                let _ = write!(current, "<sup class=fn>{}</sup>", escape(&label));
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                let _ = write!(current, "<code>{}</code>", escape(&math));
            }
        }
    }
    if !current.trim().is_empty() {
        blocks.push(current);
    }
    blocks
}

/// One reply as blocks inside its bubble content.
pub(super) fn render(text: &str) -> String {
    let blocks = blocks(text);
    if blocks.is_empty() {
        return "<p class=muted>Empty reply.</p>".into();
    }
    let mut html = String::with_capacity(text.len() + 64);
    for block in &blocks {
        html.push_str(block);
    }
    html
}

/// A same-origin path or an http(s) page; anything else renders as text.
fn local_target(dest: &str) -> Option<String> {
    let trimmed = dest.trim();
    if let Some(rest) = trimmed.strip_prefix('#') {
        let number = rest.parse::<u64>().ok()?;
        return (number > 0).then(|| format!("/items/{number}"));
    }
    if let Some((scheme, rest)) = trimmed.split_once("://") {
        return matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https")
            .then_some(trimmed)
            .filter(|_| rest.split('/').next().is_some_and(|host| !host.is_empty()))
            .map(str::to_owned);
    }
    // Only a plain path: no scheme, no protocol-relative host, no backslash or control.
    if !trimmed.starts_with('/')
        || trimmed.starts_with("//")
        || trimmed
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | '"' | '<' | '>' | ' '))
    {
        return None;
    }
    Some(trimmed.to_owned())
}

/// Text with `#123` mentions linked to their item page. The number must stand alone.
fn mentions(text: &str) -> String {
    let mut html = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('#') {
        let tail = &rest[at + 1..];
        let end = tail.find(|c: char| !c.is_ascii_digit()).unwrap_or(tail.len());
        let number = &tail[..end];
        let before_ok = at == 0
            || !rest[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '#' || c == '_');
        html.push_str(&escape(&rest[..at]));
        // Written back without leading zeros, so `#0017` opens item 17 as itself.
        match number.trim_start_matches('0').parse::<u64>() {
            Ok(number) if before_ok && number > 0 => {
                let _ = write!(html, "<a href=\"/items/{number}\">#{number}</a>");
            }
            _ => html.push_str(&escape("#")),
        }
        rest = &tail[end..];
    }
    html.push_str(&escape(rest));
    html
}

/// Text shown as written, with line breaks and tabs kept. Every other control
/// character becomes a space, exactly as the page's one escaper does for a line.
pub(super) fn escape_multiline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\n' | '\t' => out.push(c),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(text: &str) -> String {
        render(text)
    }

    #[test]
    fn reply_markup_is_escaped_and_only_same_origin_links_survive() {
        let out = html("Use <b>bold</b> & <script>alert(1)</script>.\n\n- one\n- two\n\n- first\n\n1. step");
        assert!(out.contains("&lt;b&gt;bold&lt;/b&gt;"));
        assert!(out.contains("&lt;script&gt;alert(1)&lt;/script&gt;") && !out.contains("<script>"));
        // The list item text is its own paragraph, so `<p>` wraps each item's text.
        for item in [
            "<li><p>one</p></li>",
            "<li><p>two</p></li>",
            "<li><p>step</p></li>",
        ] {
            assert!(out.contains(item), "{item}: {out}");
        }
        assert!(out.contains("<ul>") && out.contains("</ul>"), "{out}");
        assert!(out.contains("<ol start=1>") && out.contains("</ol>"), "{out}");
        for hostile in [
            "[x](javascript:alert(1))",
            "[x](data:text/html,1)",
            "[x](//attacker.invalid/)",
            "[x](/\\attacker.invalid)",
            "[x]( /items/1)",
        ] {
            let out = html(hostile);
            assert!(!out.contains("href="), "{hostile}: {out}");
            assert!(out.contains("x"), "{hostile}: {out}");
        }
        let link = html("[the item](/items/17) and [docs](https://example.invalid/a?b=1&c=2) and #17 and #0");
        assert!(link.contains("href=\"/items/17\""), "{link}");
        assert!(
            link.contains("href=\"https://example.invalid/a?b=1&amp;c=2\""),
            "{link}"
        );
        assert!(link.contains("<a href=\"/items/17\">#17</a>"), "{link}");
        assert!(link.contains("#0") && !link.contains("/items/0"), "{link}");
    }

    #[test]
    fn code_is_escaped_with_its_language_and_link_targets_are_bounded() {
        let out = html("```rust\nfn main() { println!(\"<hi>\"); }\n```");
        assert!(out.contains("<span class=lang>rust</span><pre><code>"), "{out}");
        assert!(out.contains("&lt;hi&gt;") && out.contains("&quot;"), "{out}");
        assert!(!out.contains("<hi>"), "{out}");
        let many = (0..MAX_LINKS + 5)
            .map(|n| format!("[{n}](/items/{n})"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(html(&many).matches(" href=").count(), MAX_LINKS);
        // An empty reply is said to be empty rather than rendered as nothing.
        assert!(html("").contains("Empty reply."));
    }
}
