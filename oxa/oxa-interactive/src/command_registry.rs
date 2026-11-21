//! Shared command declarations and pure frontend adapters.
//!
//! First declarations: help and the work-board consumer seam. Legacy command tables remain until their handlers are
//! migrated; this module does not claim that those commands already have surface parity.
//! Resolving help never needs a repository, environment, trust store or runtime policy.
//!
//! Global process options such as `--sandbox` are an explicit exception: this registry models
//! invocable commands and their frontend surfaces, while global flags are declared by the CLI
//! usage table in `oxa-cli/src/args.rs` and have no slash/menu/web handler.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Surface {
    Cli,
    Slash,
    Menu,
    Web,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArgumentKind {
    Word,
    Flag,
    Tail,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Argument {
    pub name: &'static str,
    pub kind: ArgumentKind,
    pub required: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Requirements {
    pub restart: bool,
    pub browser: bool,
    pub tty: bool,
    pub owner_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Availability {
    Supported,
    Exception(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Surfaces {
    pub cli: Availability,
    pub slash: Availability,
    pub menu: Availability,
    pub web: Availability,
}

impl Surfaces {
    pub fn at(self, surface: Surface) -> Availability {
        match surface {
            Surface::Cli => self.cli,
            Surface::Slash => self.slash,
            Surface::Menu => self.menu,
            Surface::Web => self.web,
        }
    }
}

// Reservations are invisible until the corresponding consumer handler is integrated.
const LEAD_PENDING: Surfaces = Surfaces {
    cli: Availability::Exception("Lead handler integration is pending"),
    slash: Availability::Exception("Lead handler integration is pending"),
    menu: Availability::Exception("Lead menu integration is pending"),
    web: Availability::Exception("Lead web adapter integration is pending"),
};
/// The `oxa memory` verbs are a CLI-only surface: no slash, menu or web handler exists.
const MEMORY_CLI_ONLY: Surfaces = Surfaces {
    cli: Availability::Supported,
    slash: Availability::Exception("The slash menu has no memory command"),
    menu: Availability::Exception("The menu has no memory command"),
    web: Availability::Exception("No web frontend is shipped"),
};
/// `oxa local add` is a CLI-only surface: it edits the user's settings file and prints a report.
const LOCAL_CLI_ONLY: Surfaces = Surfaces {
    cli: Availability::Supported,
    slash: Availability::Exception("Local models are wired from the command line, not a running session"),
    menu: Availability::Exception("The menu has no local command"),
    web: Availability::Exception("No web frontend is shipped"),
};
const NO_REQUIREMENTS: Requirements = Requirements {
    restart: false,
    browser: false,
    tty: false,
    owner_only: false,
};

#[derive(Clone, Copy, Debug)]
pub struct Declaration {
    pub id: CommandId,
    pub path: &'static [&'static str],
    pub aliases: &'static [&'static [&'static str]],
    pub summary: &'static str,
    pub arguments: &'static [Argument],
    pub surfaces: Surfaces,
    pub requirements: Requirements,
}

// IDs and declarations cannot acquire separate spelling tables. Both parsers and every
// presentation view below consume exactly this expansion.
macro_rules! commands {
    ($($id:ident { $($field:ident: $value:expr),* $(,)? }),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum CommandId { $($id),+ }
        pub const COMMANDS: &[Declaration] = &[$(Declaration { id: CommandId::$id, $($field: $value),* }),+];
    };
}

commands! {
    Help {
        path: &["help"],
        aliases: &[],
        summary: "Show command help",
        arguments: &[Argument { name: "command", kind: ArgumentKind::Tail, required: false }],
        surfaces: Surfaces {
            cli: Availability::Exception("CLI help command migration is pending"),
            slash: Availability::Supported,
            menu: Availability::Supported,
            web: Availability::Exception("No web frontend is shipped"),
        },
        requirements: Requirements { restart: false, browser: false, tty: false, owner_only: false },
    },
    Usage {
        path: &["usage"],
        aliases: &[],
        summary: "Print today's cloud spend and request sums per usage pool as JSON",
        arguments: &[Argument { name: "json", kind: ArgumentKind::Flag, required: true }],
        surfaces: Surfaces {
            cli: Availability::Supported,
            slash: Availability::Exception("The JSON usage report is a CLI command"),
            menu: Availability::Exception("The JSON usage report is a CLI command"),
            web: Availability::Exception("The panel reads this report from the CLI"),
        },
        requirements: NO_REQUIREMENTS,
    },
    Cache {
        path: &["cache"],
        aliases: &[],
        summary: "Report saved prompt-cache evidence without contacting a model",
        arguments: &[
            Argument { name: "session-id|path", kind: ArgumentKind::Word, required: true },
            Argument { name: "json", kind: ArgumentKind::Flag, required: false },
        ],
        surfaces: Surfaces {
            cli: Availability::Supported,
            slash: Availability::Exception("Cache reports are a CLI command"),
            menu: Availability::Exception("Cache reports are a CLI command"),
            web: Availability::Exception("Cache reports are a CLI command"),
        },
        requirements: NO_REQUIREMENTS,
    },
    WorkStatus {
        path: &["work", "status"],
        aliases: &[],
        summary: "List the saved work records and their liveness",
        arguments: &[Argument { name: "json", kind: ArgumentKind::Flag, required: false }],
        surfaces: Surfaces {
            cli: Availability::Supported,
            slash: Availability::Exception("Slash handler migration is pending"),
            menu: Availability::Exception("Menu handler migration is pending"),
            web: Availability::Exception("No web frontend is shipped"),
        },
        requirements: Requirements { restart: false, browser: false, tty: false, owner_only: false },
    },
    WorkBoard {
        path: &["work", "board"],
        aliases: &[],
        summary: "Show the work board",
        arguments: &[Argument { name: "args", kind: ArgumentKind::Tail, required: false }],
        surfaces: Surfaces {
            cli: Availability::Exception("Work board handler has not landed"),
            slash: Availability::Exception("Work board handler has not landed"),
            menu: Availability::Exception("Work board handler has not landed"),
            web: Availability::Exception("No web frontend is shipped"),
        },
        requirements: Requirements { restart: false, browser: false, tty: false, owner_only: false },
    },
    Status {
        path: &["status"],
        aliases: &[],
        summary: "Show session status, or print suite status as JSON",
        arguments: &[
            Argument { name: "session-id|session-file", kind: ArgumentKind::Word, required: false },
            Argument { name: "json", kind: ArgumentKind::Flag, required: false },
            Argument { name: "session-dir", kind: ArgumentKind::Flag, required: false },
        ],
        surfaces: Surfaces {
            cli: Availability::Supported,
            slash: Availability::Exception("The slash menu has no status command"),
            menu: Availability::Exception("The menu has no status command"),
            web: Availability::Exception("No web frontend is shipped"),
        },
        requirements: NO_REQUIREMENTS,
    },
    Lead {
        path: &["lead"], aliases: &[], summary: "Chat with the dedicated Lead session",
        arguments: &[Argument { name: "prompt", kind: ArgumentKind::Tail, required: false }],
        surfaces: Surfaces {
            cli: Availability::Supported,
            slash: Availability::Exception("The slash menu is not available yet"),
            menu: Availability::Supported,
            web: Availability::Exception("Use the board navigation in the web UI"),
        }, requirements: NO_REQUIREMENTS,
    },
    LeadServe {
        path: &["lead", "serve"], aliases: &[], summary: "Serve the local lead board",
        arguments: &[],
        surfaces: Surfaces {
            cli: Availability::Supported,
            menu: Availability::Supported,
            ..LEAD_PENDING
        },
        requirements: Requirements { owner_only: true, ..NO_REQUIREMENTS },
    },
    LeadBoard {
        path: &["lead", "board"], aliases: &[], summary: "Show the lead board",
        arguments: &[Argument { name: "args", kind: ArgumentKind::Tail, required: false }],
        surfaces: LEAD_PENDING, requirements: NO_REQUIREMENTS,
    },
    LeadStatus {
        path: &["lead", "status"], aliases: &[], summary: "Show lead status",
        arguments: &[Argument { name: "args", kind: ArgumentKind::Tail, required: false }],
        surfaces: LEAD_PENDING, requirements: NO_REQUIREMENTS,
    },
    LeadAsks {
        path: &["lead", "asks"], aliases: &[], summary: "Show lead questions",
        arguments: &[],
        surfaces: Surfaces { cli: Availability::Supported, ..LEAD_PENDING },
        requirements: Requirements { owner_only: true, ..NO_REQUIREMENTS },
    },
    LeadRun {
        path: &["lead", "run"], aliases: &[], summary: "Run the Lead observation loop",
        arguments: &[Argument { name: "args", kind: ArgumentKind::Tail, required: false }],
        surfaces: Surfaces { cli: Availability::Supported, ..LEAD_PENDING },
        requirements: Requirements { owner_only: true, ..NO_REQUIREMENTS },
    },
    LeadChat {
        path: &["lead", "chat"], aliases: &[], summary: "Open lead chat",
        arguments: &[Argument { name: "args", kind: ArgumentKind::Tail, required: false }],
        surfaces: Surfaces { cli: Availability::Supported, ..LEAD_PENDING },
        requirements: Requirements { owner_only: true, ..NO_REQUIREMENTS },
    },
    // #1512 S5: the owner's CLI verbs for sessions. The CLI parser keeps them (`oxa send` with
    // `--approve` and `--approvals` goes to the approval desk); they are declared for help.
    Send {
        path: &["send"],
        aliases: &[],
        summary: "Steer a session (send [--follow-up] <session-id> <text>; --follow-up queues for the end of its turn), or answer its approval request (send <session-id> --approve <request-id> once|session|always|deny)",
        arguments: &[Argument { name: "args", kind: ArgumentKind::Tail, required: false }],
        surfaces: Surfaces {
            cli: Availability::Supported,
            slash: Availability::Exception("Session messages are a CLI verb"),
            menu: Availability::Exception("Session messages are a CLI verb"),
            web: Availability::Exception("No web frontend is shipped"),
        },
        requirements: Requirements { restart: false, browser: false, tty: false, owner_only: false },
    },
    SendApprovals {
        path: &["send", "--approvals"],
        aliases: &[],
        summary: "List the approval requests that wait for an answer",
        arguments: &[],
        surfaces: Surfaces {
            cli: Availability::Supported,
            slash: Availability::Exception("Session messages are a CLI verb"),
            menu: Availability::Exception("Session messages are a CLI verb"),
            web: Availability::Exception("No web frontend is shipped"),
        },
        requirements: Requirements { restart: false, browser: false, tty: false, owner_only: false },
    },
    Memory {
        path: &["memory"], aliases: &[], summary: "Show the memory index, or show or remove one memory entry",
        arguments: &[Argument { name: "verb", kind: ArgumentKind::Tail, required: false }],
        surfaces: MEMORY_CLI_ONLY, requirements: NO_REQUIREMENTS,
    },
    MemoryList {
        path: &["memory", "list"], aliases: &[], summary: "Print the memory index",
        arguments: &[],
        surfaces: MEMORY_CLI_ONLY, requirements: NO_REQUIREMENTS,
    },
    MemoryShow {
        path: &["memory", "show"], aliases: &[], summary: "Print one memory entry",
        arguments: &[Argument { name: "name", kind: ArgumentKind::Word, required: true }],
        surfaces: MEMORY_CLI_ONLY, requirements: NO_REQUIREMENTS,
    },
    MemoryRm {
        path: &["memory", "rm"], aliases: &[], summary: "Delete one memory entry and its index line",
        arguments: &[Argument { name: "name", kind: ArgumentKind::Word, required: true }],
        surfaces: MEMORY_CLI_ONLY, requirements: NO_REQUIREMENTS,
    },
    Models {
        path: &["models"], aliases: &[], summary: "List the local and cloud models this install can serve, as a table or as JSON",
        arguments: &[
            Argument { name: "json", kind: ArgumentKind::Flag, required: false },
            Argument { name: "search", kind: ArgumentKind::Word, required: false },
        ],
        surfaces: Surfaces {
            cli: Availability::Supported,
            slash: Availability::Exception("No models slash handler is shipped"),
            menu: Availability::Exception("No models menu handler is shipped"),
            web: Availability::Exception("No web frontend is shipped"),
        },
        requirements: NO_REQUIREMENTS,
    },
    Local {
        path: &["local"], aliases: &[], summary: "Wire a local GGUF model into the native local engine settings",
        arguments: &[Argument { name: "verb", kind: ArgumentKind::Tail, required: false }],
        surfaces: LOCAL_CLI_ONLY, requirements: Requirements { owner_only: true, ..NO_REQUIREMENTS },
    },
    LocalAdd {
        path: &["local", "add"], aliases: &[],
        summary: "Add a local GGUF model from the engine's admission report",
        arguments: &[Argument { name: "args", kind: ArgumentKind::Tail, required: true }],
        surfaces: LOCAL_CLI_ONLY, requirements: Requirements { owner_only: true, ..NO_REQUIREMENTS },
    },
    Admission {
        path: &["admission"],
        aliases: &[],
        summary: "Engine-only: size a model's memory before serving it (`oxe admission`)",
        arguments: &[Argument { name: "--model|--ctx|--margin-bytes|launch options|--json", kind: ArgumentKind::Tail, required: false }],
        surfaces: Surfaces {
            cli: Availability::Exception("Engine-only subcommand of the oxe binary; oxa never dispatches it"),
            slash: Availability::Exception("Not a session command"),
            menu: Availability::Exception("Not a session command"),
            web: Availability::Exception("No web frontend is shipped"),
        },
        requirements: NO_REQUIREMENTS,
    },
    Goal {
        path: &["goal"],
        aliases: &[],
        summary: "Set, show or clear the session goal",
        arguments: &[Argument { name: "text|clear|pause|resume", kind: ArgumentKind::Tail, required: false }],
        surfaces: Surfaces {
            cli: Availability::Exception("The goal belongs to a running session"),
            slash: Availability::Supported,
            menu: Availability::Exception("Slash handler is the only goal surface for now"),
            web: Availability::Exception("No web frontend is shipped"),
        },
        requirements: NO_REQUIREMENTS,
    },
}

#[derive(Debug)]
pub struct Invocation<'a> {
    pub command: &'static Declaration,
    /// Borrowed verbatim; the existing handler retains argument validation and policy.
    pub args: &'a [String],
    /// Frontends must answer this before constructing runtime inputs or invoking handlers.
    pub help_requested: bool,
}

fn resolve(surface: Surface, words: &[String]) -> Option<Invocation<'_>> {
    COMMANDS
        .iter()
        .flat_map(|command| {
            std::iter::once(command.path)
                .chain(command.aliases.iter().copied())
                .map(move |path| (command, path))
        })
        .filter(|(_, path)| {
            words.len() >= path.len() && words.iter().zip(*path).all(|(word, part)| word.as_str() == *part)
        })
        .max_by_key(|(_, path)| path.len())
        .filter(|(command, _)| command.surfaces.at(surface) == Availability::Supported)
        .map(|(command, path)| Invocation {
            command,
            args: &words[path.len()..],
            help_requested: command.id == CommandId::Help
                || words[path.len()..]
                    .iter()
                    .any(|word| matches!(word.as_str(), "-h" | "--help")),
        })
}

pub fn parse_cli(words: &[String]) -> Option<Invocation<'_>> {
    resolve(Surface::Cli, words)
}

/// Slash tokenization stays with the existing frontend, preserving its quoting rules.
/// The first token is the command name without the leading slash.
pub fn dispatch_slash(words: &[String]) -> Option<Invocation<'_>> {
    resolve(Surface::Slash, words)
}

/// One source for help, menus, completion and generated documentation. Consumers render
/// these declarations instead of maintaining another command-name/summary list.
pub fn entries(surface: Surface) -> impl Iterator<Item = &'static Declaration> {
    COMMANDS
        .iter()
        .filter(move |entry| entry.surfaces.at(surface) == Availability::Supported)
}

pub fn help(surface: Surface) -> String {
    entries(surface)
        .map(|entry| format!("{} — {}", entry.path.join(" "), entry.summary))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn declarations_drive_both_parsers_and_shared_views() {
        for entry in COMMANDS {
            for path in std::iter::once(entry.path).chain(entry.aliases.iter().copied()) {
                let input = words(&format!("{} work board --help", path.join(" ")));
                for (surface, parsed) in [
                    (Surface::Cli, parse_cli(&input)),
                    (Surface::Slash, dispatch_slash(&input)),
                ] {
                    match entry.surfaces.at(surface) {
                        Availability::Supported => {
                            let parsed = parsed.expect("declared surface resolves");
                            assert_eq!(parsed.command.id, entry.id);
                            assert_eq!(parsed.args, words("work board --help"));
                            assert!(parsed.help_requested);
                        }
                        Availability::Exception(reason) => {
                            assert!(!reason.trim().is_empty());
                            assert!(parsed.is_none(), "unavailable surface must not resolve");
                        }
                    }
                }
            }
            for surface in [Surface::Cli, Surface::Slash, Surface::Menu, Surface::Web] {
                match entry.surfaces.at(surface) {
                    Availability::Supported => {
                        assert!(entries(surface).any(|view| view.id == entry.id));
                        assert!(help(surface).contains(entry.summary));
                    }
                    Availability::Exception(reason) => {
                        assert!(!reason.trim().is_empty());
                        assert!(!entries(surface).any(|view| view.id == entry.id));
                    }
                }
            }
        }
    }

    #[test]
    fn help_is_resolved_without_runtime_inputs_and_keeps_argument_bytes() {
        let input = vec!["help".into(), "work".into(), " board ".into()];
        let parsed = dispatch_slash(&input).unwrap();
        assert!(parsed.help_requested);
        assert_eq!(parsed.args, &input[1..]);
        assert_eq!(parsed.command.requirements, Requirements::default());
        assert!(
            parse_cli(&input).is_none(),
            "unmigrated CLI help is not advertised"
        );
        let cli = words("work status --help");
        let parsed = parse_cli(&cli).unwrap();
        assert!(parsed.help_requested);
        assert_eq!(parsed.args, &cli[2..]);
        assert!(parse_cli(&words("unknown help")).is_none());
        assert!(dispatch_slash(&words("unknown")).is_none());
    }

    #[test]
    fn status_json_is_declared_for_the_cli_surface() {
        let input = words("status --json");
        let parsed = parse_cli(&input).expect("status resolves as a CLI command");
        assert_eq!(parsed.command.id, CommandId::Status);
        assert_eq!(parsed.args, &input[1..]);
        assert!(parsed
            .command
            .arguments
            .iter()
            .any(|argument| argument.name == "json" && argument.kind == ArgumentKind::Flag));
        assert!(dispatch_slash(&input).is_none());
        assert!(entries(Surface::Cli).any(|entry| entry.id == CommandId::Status));
    }

    #[test]
    fn paths_are_unique_and_declarations_are_complete() {
        let mut paths = std::collections::BTreeSet::new();
        for entry in COMMANDS {
            assert!(!entry.path.is_empty());
            assert!(!entry.summary.trim().is_empty());
            for path in std::iter::once(entry.path).chain(entry.aliases.iter().copied()) {
                assert!(path
                    .iter()
                    .all(|word| !word.is_empty() && !word.contains(char::is_whitespace)));
                assert!(paths.insert(path), "duplicate command path");
            }
        }
    }
    #[test]
    fn lead_family_is_reserved_without_admitting_uninstalled_handlers() {
        let routes: &[(CommandId, &[&str])] = &[
            (CommandId::Lead, &["lead"]),
            (CommandId::LeadServe, &["lead", "serve"]),
            (CommandId::LeadBoard, &["lead", "board"]),
            (CommandId::LeadStatus, &["lead", "status"]),
            (CommandId::LeadAsks, &["lead", "asks"]),
            (CommandId::LeadRun, &["lead", "run"]),
            (CommandId::LeadChat, &["lead", "chat"]),
        ];
        for (id, path) in routes {
            let declaration = COMMANDS.iter().find(|entry| entry.id == *id).unwrap();
            assert_eq!(declaration.path, *path);
            let input: Vec<_> = path.iter().map(|part| (*part).to_string()).collect();
            let in_menu = matches!(id, CommandId::Lead | CommandId::LeadServe);
            let in_cli =
                in_menu || matches!(id, CommandId::LeadChat | CommandId::LeadRun | CommandId::LeadAsks);
            assert_eq!(
                parse_cli(&input).map(|invocation| invocation.command.id),
                in_cli.then_some(*id)
            );
            assert!(dispatch_slash(&input).is_none());
            for surface in [Surface::Cli, Surface::Slash, Surface::Menu, Surface::Web] {
                let shown = (in_menu && surface == Surface::Menu) || (in_cli && surface == Surface::Cli);
                assert_eq!(entries(surface).any(|entry| entry.id == *id), shown);
                assert_eq!(help(surface).contains(declaration.summary), shown);
            }
        }
    }

    #[test]
    fn local_add_is_declared_for_the_cli_surface_only() {
        let input = words("local add m.gguf --enable");
        let parsed = parse_cli(&input).expect("local add resolves on the CLI");
        assert_eq!(parsed.command.id, CommandId::LocalAdd);
        assert_eq!(parsed.args, &input[2..]);
        assert!(parsed.command.requirements.owner_only);
        assert_eq!(parse_cli(&words("local")).unwrap().command.id, CommandId::Local);
        assert!(dispatch_slash(&words("local add m.gguf")).is_none());
        for surface in [Surface::Slash, Surface::Menu, Surface::Web] {
            assert!(!entries(surface).any(|e| e.id == CommandId::LocalAdd));
        }
    }

    #[test]
    fn goal_is_declared_for_the_slash_surface_only() {
        let input = words("goal pause");
        let parsed = dispatch_slash(&input).expect("goal resolves as a slash command");
        assert_eq!(parsed.command.id, CommandId::Goal);
        assert_eq!(parsed.args, &input[1..]);
        assert!(parse_cli(&input).is_none());
        assert!(entries(Surface::Slash).any(|entry| entry.id == CommandId::Goal));
        assert!(!entries(Surface::Cli).any(|entry| entry.id == CommandId::Goal));
    }

    #[test]
    fn models_declares_its_json_flag_search_word_and_cli_only_surface() {
        let input = words("models --json");
        let parsed = parse_cli(&input).expect("models resolves on the CLI");
        assert_eq!(parsed.command.id, CommandId::Models);
        assert_eq!(parsed.args, &input[1..]);
        assert_eq!(parsed.command.arguments[0].name, "json");
        assert_eq!(parsed.command.arguments[0].kind, ArgumentKind::Flag);
        assert_eq!(parsed.command.arguments[1].name, "search");
        assert_eq!(parsed.command.arguments[1].kind, ArgumentKind::Word);
        assert!(dispatch_slash(&input).is_none());
    }

    #[test]
    fn unavailable_child_does_not_resolve_as_the_available_lead_menu() {
        assert_eq!(parse_cli(&words("lead")).unwrap().command.id, CommandId::Lead);
        assert!(parse_cli(&words("lead board")).is_none());
        let input = words("lead chat status");
        let invocation = parse_cli(&input).unwrap();
        assert_eq!(invocation.command.id, CommandId::LeadChat);
        assert_eq!(invocation.args, &input[2..]);
    }
}
