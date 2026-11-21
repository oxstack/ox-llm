use super::Query;
use std::fmt::Write;

pub(super) const BOARD_ROWS: usize = 50;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prompt {
    Status,
    Workers,
    Attention,
}

/// Read-only destinations; request text never becomes a renderer instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Home,
    Asks,
    Board,
    BoardQuery(Query),
    Workers,
    Usage,
    Item(u64),
    /// A fixed snapshot question, with the entry id a `?before=` page opens at.
    Chat(Prompt, Option<[u8; 8]>),
}

impl Page {
    const ALL: [Self; 5] = [Self::Home, Self::Asks, Self::Board, Self::Workers, Self::Usage];

    pub fn title(self) -> &'static str {
        match self {
            Self::Home => "Status",
            Self::Asks => "Asks",
            Self::Board | Self::BoardQuery(_) => "Board",
            Self::Workers => "Workers",
            Self::Usage => "Usage",
            Self::Item(_) => "Item",
            Self::Chat(..) => "Chat with lead",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Home => "/",
            Self::Asks => "/asks",
            Self::Board | Self::BoardQuery(_) | Self::Item(_) => "/board",
            Self::Workers => "/workers",
            Self::Usage => "/usage",
            Self::Chat(..) => "/chat",
        }
    }

    pub(crate) fn request(path: &str) -> Option<(Self, bool)> {
        let (path, fragment) = path
            .strip_suffix("?fragment=1")
            .or_else(|| path.strip_suffix("&fragment=1"))
            .map_or((path, false), |path| (path, true));
        if let Some(number) = path.strip_prefix("/items/") {
            let parsed: u64 = number.parse().ok()?;
            return (parsed > 0 && parsed.to_string() == number).then_some((Self::Item(parsed), fragment));
        }
        if let Some(query) = path.strip_prefix("/board?") {
            return Some((Self::BoardQuery(Query::parse(query)?), fragment));
        }
        if let Some(query) = path.strip_prefix("/chat?") {
            return Some((chat_request(query)?, fragment));
        }
        if path == "/chat" {
            return Some((Self::Chat(Prompt::Status, None), fragment));
        }
        Self::ALL
            .into_iter()
            .find(|page| page.path() == path)
            .map(|page| (page, fragment))
    }

    /// The entry id a `?before=` page of the thread opens at.
    pub(super) fn chat_before(&self) -> Option<[u8; 8]> {
        match self {
            Self::Chat(_, before) => *before,
            _ => None,
        }
    }

    pub(super) fn navigation(self, ask_count: Option<usize>) -> String {
        let mut html = String::from("<nav class=tabs aria-label=Main>");
        let selected = if matches!(self, Self::BoardQuery(_) | Self::Item(_)) {
            Self::Board
        } else {
            self
        };
        for page in Self::ALL {
            let _ = write!(html, "<a href={}", page.path());
            if page == Self::Asks {
                let label = ask_count.map_or_else(
                    || "Asks, observations unavailable".into(),
                    |count| format!("Asks, {count} open in snapshot"),
                );
                let _ = write!(html, " aria-label=\"{label}\"");
            }
            if page == selected {
                html.push_str(" aria-current=page");
            }
            let _ = write!(
                html,
                ">{0}<svg aria-hidden=true focusable=false width=22 height=22 viewBox=\"0 0 24 24\" fill=none stroke=currentColor stroke-width=1.8 stroke-linecap=round stroke-linejoin=round><path d=\"{1}\"/></svg>",
                if page == Self::Home { "Home" } else { page.title() },
                page.icon(),
            );
            if page == Self::Asks {
                let count = ask_count.map_or_else(|| "?".into(), |count| count.to_string());
                let class = if ask_count.is_some_and(|count| count > 0) {
                    "badge"
                } else {
                    "badge quiet"
                };
                let _ = write!(html, "<span class=\"{class}\" aria-hidden=true>{count}</span>");
            }
            html.push_str("</a>");
        }
        let _ = write!(
            html,
            "<a class=compose href=/chat aria-label=\"Chat with lead\"{}>✎ Chat</a></nav>",
            if matches!(self, Self::Chat(..)) {
                " aria-current=page"
            } else {
                ""
            }
        );
        html
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Home => "M3 10 12 3l9 7M5 9v12h14V9M9 21v-7h6v7",
            Self::Asks => "M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0M9.5 9a2.5 2.5 0 1 1 3.5 2.3c-1 .4-1 1.2-1 2M12 17h.01",
            Self::Board | Self::BoardQuery(_) | Self::Item(_) => "M4 4h16v16H4zM9 4v16M4 9h16",
            Self::Workers => "M4 20v-3a4 4 0 0 1 8 0v3M8 10a3 3 0 1 0 0-6 3 3 0 0 0 0 6M14 20v-3a4 4 0 0 1 8 0v3M18 10a3 3 0 1 0 0-6 3 3 0 0 0 0 6",
            Self::Usage => "M4 20V4M4 20h17M8 16v-4M13 16V8M18 16V5",
            Self::Chat(..) => "M4 20l4-1 12-12-3-3L5 16z",
        }
    }
}

/// `/chat` query fields: one fixed question and one older-thread bound, no other. An
/// entry id is checked against the session file's own id alphabet here; the thread
/// itself looks it up, so an id the session does not hold changes nothing.
fn chat_request(query: &str) -> Option<Page> {
    let (mut prompt, mut before) = (None, None);
    for part in query.split('&').filter(|part| !part.is_empty()) {
        let (key, value) = part.split_once('=')?;
        match key {
            "prompt" if prompt.is_none() => {
                prompt = Some(match value {
                    "status" => Prompt::Status,
                    "workers" => Prompt::Workers,
                    "attention" => Prompt::Attention,
                    _ => return None,
                })
            }
            "before" if before.is_none() => {
                // A session entry id is exactly eight lowercase hex characters; any
                // other bound is not a page of this thread and is refused here.
                let bytes = value.as_bytes();
                if bytes.len() != 8
                    || !bytes
                        .iter()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
                {
                    return None;
                }
                let mut bound = [0u8; 8];
                bound.copy_from_slice(bytes);
                before = Some(bound);
            }
            _ => return None,
        }
    }
    Some(Page::Chat(prompt.unwrap_or(Prompt::Status), before))
}
