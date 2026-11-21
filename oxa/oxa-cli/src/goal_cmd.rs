//! Explicit goal file loading for headless sessions.
use oxa_proto::{codes, OxaError};
use oxa_session::{Goal, GoalBudget};
use std::io::Read;
use std::path::Path;

pub(crate) fn load(args: &crate::Args, cwd: &Path) -> Result<Option<Goal>, OxaError> {
    args.goal
        .as_ref()
        .map(|file| {
            let file = std::fs::File::open(cwd.join(file)).map_err(|e| OxaError::io("read goal file", &e))?;
            from_objective(read_objective(file)?, args.max_time, args.max_tokens)
        })
        .transpose()
}

pub(crate) fn read_objective(reader: impl Read) -> Result<String, OxaError> {
    let mut bytes = Vec::new();
    reader
        .take(65537)
        .read_to_end(&mut bytes)
        .map_err(|e| OxaError::io("read goal file", &e))?;
    String::from_utf8(bytes).map_err(|_| OxaError::new(codes::USAGE, "Goal file must be UTF-8 text."))
}

pub(crate) fn from_objective(
    objective: String,
    max_time: Option<std::time::Duration>,
    max_tokens: Option<u64>,
) -> Result<Goal, OxaError> {
    let max_time_ms = max_time
        .map(|duration| {
            u64::try_from(duration.as_millis())
                .map_err(|_| OxaError::new(codes::USAGE, "Goal time budget is too large."))
        })
        .transpose()?;
    let started_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| OxaError::new(codes::IO, "System clock precedes epoch."))?
        .as_millis();
    Goal::new(
        objective,
        GoalBudget {
            max_time_ms,
            max_tokens,
            max_turns: None,
        },
        u64::try_from(started_ms).map_err(|_| OxaError::new(codes::IO, "System clock is too large."))?,
    )
}
