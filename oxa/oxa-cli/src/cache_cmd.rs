//! Report saved request evidence, not a replay of a provider request or engine cache.
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{codes, rebase_why, shown_why, EntryBody, OxaError, TurnKind};
use oxa_session::{decode_entry, FsStore, Transcript};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[serde(rename = "type")]
    ty: String,
    id: String,
    parent_id: Option<String>,
    message: Option<Message>,
    custom_type: Option<String>,
    why: Option<String>,
    kind: Option<String>,
    usage: Option<Counts>,
}

#[derive(Deserialize)]
struct Message {
    role: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Counts {
    input: Option<u32>,
    cache_read: Option<u32>,
    #[serde(default)]
    unreported: bool,
}

struct Anchor {
    system: String,
    tools: String,
}

struct Chat {
    request: usize,
    input: Option<u32>,
    anchor: Arc<Anchor>,
}

#[derive(Default)]
struct History {
    tail: Option<String>,
    anchor: Option<Arc<Anchor>>,
    previous_chat: Option<Chat>,
    boundary: Option<&'static str>,
    contiguous_usage: bool,
    requests: usize,
    gaps: usize,
}

#[derive(Serialize)]
struct AnchorComparison {
    against_request: usize,
    system_equal: bool,
    // Byte index within saved system text, NOT JSON bytes or rendered/tokenized input.
    first_differing_system_byte: Option<usize>,
    tools_equal: bool,
}

#[derive(Serialize)]
struct Row {
    request: usize,
    kind: &'static str,
    input: Option<u32>,
    cached: Option<u32>,
    expected_cached: Option<u32>,
    // What complete reuse of the previous saved chat's input alone would count as, not a
    // prediction, runtime threshold, or proof that these are the same provider tokens.
    previous_input_reuse_estimate: Option<u32>,
    matched_request: Option<usize>,
    first_differing_message: Option<usize>,
    first_differing_byte: Option<usize>,
    miss_cause: &'static str,
    recorded_boundary: Option<&'static str>,
    saved_anchor: Option<AnchorComparison>,
}

impl History {
    fn discontinuity(&mut self, boundary: &'static str) {
        self.anchor = None;
        self.previous_chat = None;
        self.boundary = Some(boundary);
        self.contiguous_usage = false;
    }

    fn gap(&mut self) -> Option<Row> {
        self.gaps += 1;
        self.discontinuity("unreadable_entry");
        None
    }

    fn record(&mut self, line: Option<&[u8]>) -> Option<Row> {
        let Some((line, record)) = line.and_then(|line| {
            serde_json::from_slice::<Record>(line)
                .ok()
                .map(|record| (line, record))
        }) else {
            return self.gap();
        };
        if record.parent_id != self.tail {
            self.discontinuity("nonlinear_history");
        }
        self.tail = Some(record.id);
        let boundary = match record.ty.as_str() {
            "message" if record.message.is_some_and(|m| m.role == "system") => {
                match decode_entry(line).map(|entry| entry.body) {
                    Ok(EntryBody::Prefix(prefix)) => {
                        let epoch = self.anchor.is_some();
                        self.anchor = Some(Arc::new(Anchor {
                            system: prefix.system_text(),
                            tools: prefix.tools_json,
                        }));
                        epoch.then_some(rebase_why::EPOCH)
                    }
                    Ok(EntryBody::Msg { .. }) => None,
                    _ => return self.gap(),
                }
            }
            "compaction" => Some(rebase_why::COMPACTION),
            "context_edit" => Some(rebase_why::CONTEXT_EDIT),
            "branch_summary" => Some("branch_summary"),
            "model_change" => Some("model_change"),
            "thinking_level_change" => Some("thinking_level_change"),
            "custom" if record.custom_type.as_deref() == Some(Transcript::EXTENSION_REBASE_ENTRY) => {
                Some(rebase_why::EXTENSION)
            }
            "shown"
                if matches!(
                    record.why.as_deref(),
                    Some(shown_why::CUT | shown_why::FAILED | shown_why::ABORTED)
                ) =>
            {
                Some("interrupted_reply")
            }
            "usage" => return Some(self.usage(record.kind.as_deref(), record.usage)),
            _ => None,
        };
        if boundary.is_some() {
            self.boundary = boundary;
            self.contiguous_usage = false;
        }
        None
    }

    fn usage(&mut self, kind: Option<&str>, counts: Option<Counts>) -> Row {
        const KINDS: [TurnKind; 5] = [
            TurnKind::Chat,
            TurnKind::Compaction,
            TurnKind::BranchSummary,
            TurnKind::Prefill,
            TurnKind::Subagent,
        ];
        let kind = KINDS
            .iter()
            .map(|kind| kind.as_str())
            .find(|k| Some(*k) == kind)
            .unwrap_or("unknown");
        let counts = counts.filter(|counts| !counts.unreported);
        let input = counts.as_ref().and_then(|counts| counts.input);
        let cached = counts.and_then(|counts| counts.cache_read);
        self.requests += 1;
        let chat = kind == TurnKind::Chat.as_str();
        let mut comparison = None;
        let mut estimate = None;
        if chat {
            if let (Some(anchor), Some(prior)) = (&self.anchor, &self.previous_chat) {
                let (system_diff, tools_equal) = if Arc::ptr_eq(anchor, &prior.anchor) {
                    (None, true)
                } else {
                    (
                        first_difference(&prior.anchor.system, &anchor.system),
                        prior.anchor.tools == anchor.tools,
                    )
                };
                comparison = Some(AnchorComparison {
                    against_request: prior.request,
                    system_equal: system_diff.is_none(),
                    first_differing_system_byte: system_diff,
                    tools_equal,
                });
                if self.contiguous_usage && system_diff.is_none() && tools_equal {
                    estimate = prior
                        .input
                        .filter(|prior| input.is_some_and(|input| *prior <= input));
                }
            }
            self.previous_chat = self.anchor.clone().map(|anchor| Chat {
                request: self.requests,
                input,
                anchor,
            });
        }
        self.contiguous_usage = chat;
        Row {
            request: self.requests,
            kind,
            input,
            cached,
            expected_cached: None,
            previous_input_reuse_estimate: estimate,
            matched_request: None,
            first_differing_message: None,
            first_differing_byte: None,
            miss_cause: "unknown",
            recorded_boundary: if chat { self.boundary.take() } else { None },
            saved_anchor: comparison,
        }
    }
}

fn first_difference(a: &str, b: &str) -> Option<usize> {
    a.bytes()
        .zip(b.bytes())
        .position(|(a, b)| a != b)
        .or_else(|| (a.len() != b.len()).then_some(a.len().min(b.len())))
}

pub(crate) fn run(
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    env: &Env,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    let mut reference = None;
    let mut json = false;
    for word in words {
        match word.as_str() {
            "--json" if !json => json = true,
            word if !word.starts_with('-') && !word.is_empty() && reference.is_none() => {
                reference = Some(word)
            }
            _ => return Err(usage_error()),
        }
    }
    let reference = reference.ok_or_else(usage_error)?;
    let sessions = (env.get("OXA_SESSION_DIR").is_none())
        .then(|| Settings::load(dirs, cwd, false).session_dir(&dirs.home, cwd))
        .flatten()
        .unwrap_or_else(|| dirs.sessions_dir.clone());
    let scan = FsStore::new(sessions).open_scan(reference)?;
    let mut history = History::default();
    if json {
        write!(
            out,
            r#"{{"type":"oxa_cache","version":1,"coverage":"saved_usage_only","requests":["#
        )
        .map_err(write_error)?;
    } else {
        writeln!(out, "Saved usage only; runtime expectation, matched request, request offsets and miss cause are unknown.\nRequest  Kind            Input     Cached    Prior input (est.)  Recorded boundary / saved anchor").map_err(write_error)?;
    }
    let complete = scan.visit(|line| {
        if let Some(row) = history.record(line) {
            if json {
                if row.request != 1 {
                    out.write_all(b",").map_err(write_error)?;
                }
                serde_json::to_writer(&mut *out, &row)
                    .map_err(|_| OxaError::new(codes::IO, "cannot write cache report"))?;
            } else {
                text_row(out, &row).map_err(write_error)?;
            }
        }
        Ok(())
    })? && history.gaps == 0;
    if json {
        writeln!(out, "],\"scan_complete\":{complete}}}").map_err(write_error)?;
    } else if !complete {
        writeln!(
            out,
            "Partial scan: an unreadable entry, torn tail or scan limit left gaps."
        )
        .map_err(write_error)?;
    }
    Ok(0)
}

fn text_row(out: &mut dyn Write, row: &Row) -> io::Result<()> {
    let number = |n: Option<u32>| n.map_or_else(|| "?".into(), |n| n.to_string());
    write!(
        out,
        "{:>7}  {:<14}  {:>8}  {:>8}  {:>18}  {}",
        row.request,
        row.kind,
        number(row.input),
        number(row.cached),
        number(row.previous_input_reuse_estimate),
        row.recorded_boundary.unwrap_or("-")
    )?;
    if let Some(anchor) = &row.saved_anchor {
        write!(out, "; saved vs {}: system ", anchor.against_request)?;
        if let Some(byte) = anchor.first_differing_system_byte {
            write!(out, "differs at byte {byte}")?;
        } else {
            write!(out, "equal")?;
        }
        write!(
            out,
            ", tools {}",
            if anchor.tools_equal { "equal" } else { "differ" }
        )?;
    }
    writeln!(out)
}

fn usage_error() -> OxaError {
    OxaError::new(codes::USAGE, "usage: oxa cache <session-id|path> [--json]")
}

fn write_error(error: io::Error) -> OxaError {
    OxaError::io("cannot write cache report", &error)
}
