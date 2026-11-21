//! Shared chrome markup: the one place the shapes every page composes live.
//! Labels and paths are allow-listed constants or escaped here, never raw caller text.
use super::escape;

/// The chat icon, shared by the rail entry and the floating button.
pub(super) const CHAT_ICON: &str = "M4 5h16v11H9l-5 4zM8 9h8M8 12.5h5";

/// One stroke icon. Decorative: the surrounding control carries the label.
pub(super) fn icon(path: &str) -> String {
    format!(
        "<svg aria-hidden=true focusable=false width=22 height=22 viewBox=\"0 0 24 24\" fill=none stroke=currentColor stroke-width=1.8 stroke-linecap=round stroke-linejoin=round><path d=\"{}\"/></svg>",
        escape(path)
    )
}

/// The open-ask count on the Asks tab. Nothing renders at zero or when the count
/// is unknown; the tab's aria-label carries both facts, so no "?" badge is shown.
pub(super) fn ask_badge(count: Option<usize>) -> String {
    match count {
        Some(count) if count > 0 => format!("<span class=badge aria-hidden=true>{count}</span>"),
        _ => String::new(),
    }
}

/// The floating chat button: one tap from every page except chat itself.
pub(super) fn fab() -> String {
    format!(
        "<a class=fab href=/chat aria-label=\"Chat with lead\">{}</a>",
        icon(CHAT_ICON)
    )
}

/// The sixth rail entry, visible from 900px where the floating button hides.
pub(super) fn rail_chat(current: bool) -> String {
    format!(
        "<a class=rail-only href=/chat{}>Chat{}</a>",
        if current { " aria-current=page" } else { "" },
        icon(CHAT_ICON)
    )
}

/// The top bar's state dot. Grey until a sampler state colours it: an unknown
/// state never renders green. The word beside it says what the dot means.
pub(super) fn dot() -> String {
    "<span class=dot aria-hidden=true></span>".into()
}

/// The light/dark choice, kept in the browser only; live.js drives it.
pub(super) fn theme_toggle() -> String {
    format!(
        "<button id=theme class=iconbtn type=button aria-label=\"Switch colour theme\" aria-pressed=false>{}</button>",
        icon("M20 14.5A8 8 0 0 1 9.5 4a8 8 0 1 0 10.5 10.5z")
    )
}
