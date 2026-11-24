use crate::types::{Error, Result};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq)]
pub enum Intent {
    New,
    ClaudeStreamNew(String),
    Print(Vec<String>),
    Resume(String),
    Misc,
}
impl Intent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::New | Self::ClaudeStreamNew(_) => "new",
            Self::Print(_) => "print",
            Self::Resume(_) => "resume",
            Self::Misc => "misc",
        }
    }
    pub fn rotates(&self) -> bool {
        matches!(self, Self::New | Self::ClaudeStreamNew(_) | Self::Print(_))
    }
    pub fn argv<'a>(&'a self, original: &'a [String]) -> &'a [String] {
        match self {
            Self::Print(argv) => argv,
            _ => original,
        }
    }
    pub fn id(&self) -> Option<&str> {
        if let Self::Resume(id) | Self::ClaudeStreamNew(id) = self {
            Some(id)
        } else {
            None
        }
    }
}
pub struct Invocation {
    pub intent: Intent,
    pub agy_tui: bool,
    pub agy_text_stdin: bool,
    pub workspace: PathBuf,
}
impl Invocation {
    pub fn agy_conversation(&self) -> Option<&str> {
        self.intent.id().filter(|_| self.agy_tui)
    }
}
pub fn codex_stdio_command(args: &[String]) -> Result<(Intent, Vec<String>)> {
    let mut it = args.iter().map(String::as_str);
    if it.next() != Some("app-server") {
        return Err(unsupported());
    }
    let mut argv = vec!["app-server".to_owned()];
    let mut intent = Intent::New;
    let mut stdio = false;
    while let Some(flag) = it.next() {
        match flag {
            "--stdio" if !stdio => stdio = true,
            "--listen=stdio://" if !stdio => {
                stdio = true;
                argv.push(flag.into());
            }
            "--listen" if !stdio && it.next() == Some("stdio://") => {
                stdio = true;
                argv.extend([flag.into(), "stdio://".into()]);
            }
            "--for-session" if intent == Intent::New => {
                intent = Intent::Resume(native_id(it.next().unwrap_or(""))?)
            }
            "--config" => {
                let value = it.next().ok_or_else(unsupported)?;
                if !matches!(
                    value,
                    "features.goals=false"
                        | "features.wake_on_agent_messages=false"
                        | "features.default_mode_request_user_input=true"
                ) {
                    return Err(unsupported());
                }
                argv.extend([flag.into(), value.into()]);
            }
            _ => return Err(unsupported()),
        }
    }
    Ok((intent, argv))
}
pub fn native_id(raw: &str) -> Result<String> {
    let parsed = Uuid::parse_str(raw).ok().filter(|_| raw.len() == 36);
    parsed.map(|u| u.to_string()).ok_or_else(exact_id_required)
}
pub(crate) fn exact_id_required() -> Error {
    Error::bad(
        "exact_id_required",
        "Use an exact native session UUID from Sessions. Pickers, names and --last are not supported.",
    )
}
fn unsupported() -> Error {
    Error::bad(
        "unsupported_invocation",
        "Unsupported or ambiguous invocation. Use a new chat, an exact resume UUID from Sessions, or a supported maintenance command.",
    )
}
pub(crate) fn git_required() -> Error {
    Error::bad(
        "git_required",
        "Not inside a Git repository. Use --skip-git-repo-check to run exec here.",
    )
}
fn claude_internal(word: &str) -> bool {
    matches!(
        word,
        "project"
            | "edit-hook"
            | "edit-permission-rules"
            | "edit-memory-settings"
            | "edit-skill-overrides"
            | "edit-sandbox-settings"
            | "edit-chrome-settings"
            | "design-login"
            | "workspace-trust"
            | "worktree-hook"
    )
}
fn claude_command(word: &str) -> bool {
    matches!(
        word,
        "agents"
            | "apply-project-settings"
            | "attach"
            | "auth"
            | "auto-mode"
            | "doctor"
            | "gateway"
            | "import"
            | "import-conversations"
            | "install"
            | "kill"
            | "logs"
            | "mcp"
            | "plugin"
            | "plugins"
            | "purge"
            | "rc"
            | "remote-control"
            | "respawn"
            | "rm"
            | "sandbox"
            | "setup-token"
            | "stop"
            | "ultrareview"
            | "update"
            | "upgrade"
    )
}
fn compact_window(s: &str) -> bool {
    s.strip_suffix('k')
        .and_then(|v| v.parse::<u16>().ok())
        .is_some_and(|n| (100..=1000).contains(&n) && s == format!("{n}k"))
}
fn agy_terminal(args: &[String]) -> Result<(Intent, bool)> {
    let invalid = || {
        Error::bad(
            "unsupported_invocation",
            "Unsupported or ambiguous AgY arguments. Use -p for print, -i for interactive input, or one --conversation=<uuid> to resume.",
        )
    };
    let mut resume = None;
    let mut print = false;
    let mut interactive = false;
    let mut print_options = false;
    let mut stream_json = false;
    let mut prompt = None;
    let mut tail = false;
    let mut options = Vec::new();
    let mut it = args.iter().map(String::as_str).peekable();
    while let Some(arg) = it.next() {
        if tail || !arg.starts_with('-') || arg == "-" {
            if !tail
                && matches!(
                    arg,
                    "acp"
                        | "agent"
                        | "agents"
                        | "changelog"
                        | "help"
                        | "install"
                        | "mcp"
                        | "mic-serve"
                        | "models"
                        | "plugin"
                        | "plugins"
                        | "remote-control"
                        | "update"
                )
            {
                return Err(invalid());
            }
            if prompt.replace(arg).is_some() {
                return Err(invalid());
            }
            continue;
        }
        if arg == "--" {
            tail = true;
            continue;
        }
        let start = args.len() - it.len() - 1;
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        let mut value = || {
            inline
                .or_else(|| it.next().filter(|v| !v.starts_with('-')))
                .filter(|v| !v.is_empty())
                .ok_or_else(invalid)
        };
        match flag {
            "-p" | "--print" if !print => {
                print = true;
                let operand = inline.or_else(|| it.next_if(|a| !a.starts_with('-') || *a == "-"));
                if let Some(p) = operand
                    && prompt.replace(p).is_some()
                {
                    return Err(invalid());
                }
                continue;
            }
            "--dangerously-skip-permissions" | "--sandbox"
                if matches!(inline, None | Some("true" | "false")) => {}
            "--conversation" if resume.is_none() => {
                let id = value()?;
                resume = Some(native_id(id).ok().filter(|v| v == id).ok_or_else(|| {
                    Error::bad(
                        "exact_id_required",
                        "Use the exact lowercase UUID printed by AgY with --conversation.",
                    )
                })?);
            }
            "--model" | "--effort" | "--mode" | "--agent" | "--add-dir" => {
                value()?;
            }
            "-i" | "--prompt-interactive" => {
                interactive = true;
                value()?;
            }
            "--output-format" => {
                print_options = true;
                if !matches!(value()?, "text" | "json" | "stream-json") {
                    return Err(invalid());
                }
            }
            "--input-format" => {
                print_options = true;
                stream_json = match value()? {
                    "text" => false,
                    "stream-json" => true,
                    _ => return Err(invalid()),
                };
            }
            "--print-timeout" | "--json-schema" | "--settings" => {
                print_options = true;
                value()?;
            }
            "--disable-slash-commands" if matches!(inline, None | Some("true" | "false")) => {
                print_options = true
            }
            _ => return Err(invalid()),
        }
        options.extend_from_slice(&args[start..args.len() - it.len()]);
    }
    if print {
        if interactive || resume.is_some() {
            return Err(invalid());
        }
        if prompt
            .and_then(|p| p.split_whitespace().next())
            .is_some_and(|w| matches!(w, "/usage" | "/quota" | "/credits"))
        {
            return Err(Error::bad(
                "unsupported_invocation",
                "Account reports are not routed prompts. For the native /usage report, use ccp usage <antigravity-profile-id>.",
            ));
        }
        // Native AgY takes its prompt on the flag, not as an interspersed positional.
        options.push(format!("--print={}", prompt.unwrap_or_default()));
        return Ok((Intent::Print(options), prompt.is_none() && !stream_json));
    }
    if tail || prompt.is_some() || print_options {
        return Err(invalid());
    }
    Ok((resume.map(Intent::Resume).unwrap_or(Intent::New), false))
}
pub fn classify(tool: &str, args: &[String], cwd: &Path) -> Result<Invocation> {
    if tool == "agy" {
        let (intent, agy_tui, agy_text_stdin) = if matches!(args, [a] if matches!(a.as_str(), "--version" | "--help" | "models"))
            || matches!(args, [a,b] if a=="acp" && b=="--help")
        {
            (Intent::Misc, false, false)
        } else if args.first().is_some_and(|a| a == "acp") {
            let mut intent = Intent::New;
            let mut it = args[1..].iter();
            while let Some(arg) = it.next() {
                match arg.as_str() {
                    "--stdio" | "--sandbox" | "--process-per-session" | "--disable-telemetry" => {}
                    "--for-session" if intent == Intent::New => {
                        intent = Intent::Resume(
                            crate::agy::session_id(it.next().map(String::as_str).unwrap_or(""))?
                                .to_owned(),
                        );
                    }
                    _ => return Err(unsupported()),
                }
            }
            (intent, false, false)
        } else {
            let (i, text_stdin) = agy_terminal(args)?;
            let tui = !matches!(i, Intent::Print(_));
            (i, tui, text_stdin)
        };
        return Ok(Invocation {
            intent,
            agy_tui,
            agy_text_stdin,
            workspace: cwd.into(),
        });
    }
    let claude = tool == "claude";
    if !claude
        && args.first().is_some_and(|s| s == "app-server")
        && !matches!(args.get(1).map(String::as_str), Some("--help" | "-h"))
    {
        return Ok(Invocation {
            intent: codex_stdio_command(args)?.0,
            agy_tui: false,
            agy_text_stdin: false,
            workspace: cwd.into(),
        });
    }
    let mut workspace = cwd.to_owned();
    let mut words = Vec::new();
    let mut cd = false;
    let mut tail = false;
    let mut claude_selector = None;
    let mut canonical_selector = true;
    let (mut input_format, mut output_format) = (None, None);
    let mut repeated_format = false;
    let mut replay_user = false;
    let mut permission_stdio = false;
    let mut help = false;
    let mut print_mode = false;
    let mut no_persistence = false;
    let mut autocompact = false;
    let mut ephemeral = false;
    let mut exec_config = false;
    let mut tui_only = false;
    let mut it = args.iter().peekable();
    let positional = |arg: &str| !arg.starts_with('-') || arg == "-";
    while let Some(arg) = it.next() {
        let arg = arg.as_str();
        if tail || positional(arg) {
            words.push((arg, !tail));
            continue;
        }
        if arg == "--" {
            tail = true;
            continue;
        }
        let (key, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(k, v)| (k, Some(v)));
        tui_only |= matches!(
            key,
            "--no-alt-screen" | "--no-daemon" | "--search" | "-a" | "--ask-for-approval"
        );
        let mut value = || {
            inline
                .or_else(|| it.next().map(String::as_str))
                .ok_or_else(unsupported)
        };
        match (claude, key) {
            (_, "-h" | "--help" | "--version") | (true, "-v") | (false, "-V")
                if inline.is_none() =>
            {
                help = true
            }
            (true, "-r" | "--resume" | "--session-id") => {
                if claude_selector.is_some() {
                    return Err(unsupported());
                }
                let raw = value().unwrap_or("");
                let id = native_id(raw)?;
                canonical_selector = raw == id;
                claude_selector = Some(if key == "--session-id" {
                    Intent::ClaudeStreamNew(id)
                } else {
                    Intent::Resume(id)
                });
            }
            (false, "-C" | "--cd") => {
                let s = value()?;
                if s.is_empty() || (s.starts_with('-') && inline.is_none()) || cd {
                    return Err(unsupported());
                }
                cd = true;
                workspace = cwd
                    .join(s)
                    .canonicalize()
                    .map_err(|_| Error::bad("bad_cwd", "Working directory is unavailable."))?;
                if !workspace.is_dir() {
                    return Err(Error::bad("bad_cwd", "Working directory is unavailable."));
                }
            }
            (false, "-m" | "--model") => {
                let s = value()?;
                if s.is_empty() || (s.starts_with('-') && inline.is_none()) {
                    return Err(unsupported());
                }
            }
            (false, "--config") if matches!(words.first(), Some(("exec" | "e", true))) => {
                if !matches!(
                    value()?,
                    "service_tier=\"default\""
                        | "approval_policy=\"never\""
                        | "approval_policy=\"on-request\""
                        | "model_reasoning_effort=\"low\""
                        | "model_reasoning_effort=\"medium\""
                        | "model_reasoning_effort=\"high\""
                        | "model_reasoning_effort=\"xhigh\""
                        | "model_reasoning_effort=\"max\""
                        | "model_reasoning_effort=\"ultra\""
                ) {
                    return Err(unsupported());
                }
                exec_config = true;
            }
            (false, "-i" | "--image") => {
                let file = value()?;
                if file.is_empty() || (inline.is_none() && !positional(file)) {
                    return Err(unsupported());
                }
                // Pinned Codex consumes a list, except for an attached value or after exec resume.
                if inline.is_none()
                    && !matches!(
                        words.as_slice(),
                        [("exec" | "e", true), ("resume", true), ..]
                    )
                {
                    while let Some(file) = it.next_if(|arg| positional(arg)) {
                        if matches!(file.as_str(), "resume" | "fork" | "app-server") {
                            return Err(Error::bad(
                                "unsupported_invocation",
                                "Ambiguous Codex image list. Use --image=FILE before the command.",
                            ));
                        }
                    }
                }
            }
            (false, "--device-auth" | "--with-api-key")
                if inline.is_none() && words.first() == Some(&("login", true)) => {}
            (true, "--console" | "--claudeai" | "--sso" | "--text")
                if inline.is_none() && words.first() == Some(&("auth", true)) => {}
            (true, "--client-secret")
                if inline.is_none() && words.first() == Some(&("mcp", true)) => {}
            (
                true,
                "-s" | "--scope" | "-t" | "--transport" | "-e" | "--env" | "-H" | "--header"
                | "--callback-port" | "--client-id",
            )
            | (
                false,
                "--env"
                | "--url"
                | "--bearer-token-env-var"
                | "--oauth-client-id"
                | "--oauth-client-secret"
                | "--oauth-client-registration"
                | "--oauth-resource",
            ) if words.first() == Some(&("mcp", true)) => {
                if value()?.starts_with('-') && inline.is_none() {
                    return Err(unsupported());
                }
            }
            (true, "--permission-prompts") => {
                if !matches!(value()?, "host" | "none") {
                    return Err(unsupported());
                }
            }
            (true, "--input-format" | "--output-format") => {
                let v = value()?;
                if v.starts_with('-') && inline.is_none() {
                    return Err(unsupported());
                }
                let slot = if key == "--input-format" {
                    &mut input_format
                } else {
                    &mut output_format
                };
                repeated_format |= slot.replace(v).is_some();
            }
            (true, "--replay-user-messages") if inline.is_none() && !replay_user => {
                replay_user = true
            }
            (true, "--permission-prompt-tool") if !permission_stdio && value()? == "stdio" => {
                permission_stdio = true
            }
            (
                true,
                "--model"
                | "-n"
                | "--name"
                | "--effort"
                | "--max-turns"
                | "--max-budget-usd"
                | "--permission-mode"
                | "--system-prompt"
                | "--append-system-prompt"
                | "--agent"
                | "--fallback-model"
                | "--json-schema",
            )
            | (
                false,
                "-s"
                | "--sandbox"
                | "-a"
                | "--ask-for-approval"
                | "--color"
                | "-o"
                | "--output-last-message"
                | "--output-schema"
                | "--add-dir",
            ) => {
                if value()?.starts_with('-') && inline.is_none() {
                    return Err(unsupported());
                }
            }
            (true, "-p" | "--print") if inline.is_none() => print_mode = true,
            (true, "--autocompact") if !autocompact && compact_window(value()?) => {
                autocompact = true
            }
            (true, "--no-session-persistence") if inline.is_none() => no_persistence = true,
            (false, "--ephemeral")
                if inline.is_none()
                    && !ephemeral
                    && matches!(words.first(), Some(("exec" | "e", true))) =>
            {
                ephemeral = true;
            }
            (false, "-p" | "--profile" | "--print") => {
                return Err(Error::bad(
                    "unsupported_invocation",
                    "Codex -p/--profile selects a native configuration profile, not print mode. Use ccp codex exec --ephemeral for a one-shot prompt.",
                ));
            }
            (
                true,
                "--verbose"
                | "--include-partial-messages"
                | "--ax-screen-reader"
                | "--no-chrome"
                | "--allow-dangerously-skip-permissions"
                | "--dangerously-skip-permissions"
                | "--disable-slash-commands",
            )
            | (
                false,
                "--json"
                | "--no-alt-screen"
                | "--no-daemon"
                | "--search"
                | "--worktree"
                | "--approve-for-me"
                | "--dangerously-bypass-approvals-and-sandbox"
                | "--yolo"
                | "--skip-git-repo-check"
                | "--strict-config",
            ) if inline.is_none() => {}
            (_, "--last" | "--continue") | (true, "-c") => return Err(exact_id_required()),
            _ => return Err(unsupported()),
        }
    }
    let command = words.first().filter(|(_, cmd)| *cmd).map(|(w, _)| *w);
    if exec_config && !ephemeral {
        return Err(unsupported());
    }
    if claude && words.first().is_some_and(|(word, _)| claude_internal(word)) {
        return Err(unsupported());
    }
    let sub = words.get(1).filter(|(_, cmd)| *cmd).map(|(w, _)| *w);
    if claude
        && let Some(&(word, before_delimiter)) = words.first()
        && claude_command(word)
    {
        // Interactive Claude dispatches registered verbs even AFTER "--".
        // Print mode accepts "-p -- <verb>" as a literal prompt instead.
        if (!before_delimiter && !print_mode) || (before_delimiter && print_mode) {
            return Err(unsupported());
        }
    }
    // Claude can dispatch Remote Control before handling ordinary help flags.
    // Codex's hidden subcommands and aliases are also reserved prompt words.
    if matches!(
        (claude, command),
        (
            true,
            Some("remote-control" | "rc" | "remote" | "sync" | "bridge")
        ) | (
            false,
            Some(
                "cloud"
                    | "cloud-tasks"
                    | "remote-control"
                    | "tcp-tunnel"
                    | "git-credential"
                    | "execpolicy"
                    | "responses-api-proxy"
            )
        )
    ) {
        return Err(unsupported());
    }
    let duplex = print_mode
        && !repeated_format
        && input_format == Some("stream-json")
        && output_format == Some("stream-json");
    // Only bidirectional print on stdin may allocate a caller-selected ID.
    // An argv prompt alone (including a selector after "--") never does.
    let allocating = matches!(claude_selector, Some(Intent::ClaudeStreamNew(_)));
    if (allocating || replay_user || permission_stdio) && (!duplex || help)
        || allocating && words.iter().any(|(word, _)| !word.is_empty())
    {
        return Err(unsupported());
    }
    if duplex && !canonical_selector {
        return Err(Error::bad(
            "exact_id_required",
            "Use a canonical lowercase Claude session UUID.",
        ));
    }
    let mut intent = if help {
        Intent::Misc
    } else if claude {
        match command {
            Some("doctor" | "auth") if claude_selector.is_none() => Intent::Misc,
            Some("mcp")
                if claude_selector.is_none()
                    && matches!(sub, Some("list" | "get" | "add" | "add-json" | "remove")) =>
            {
                Intent::Misc
            }
            Some(word) if claude_command(word) => return Err(unsupported()),
            _ if words.len() <= 1 => claude_selector.unwrap_or(Intent::New),
            _ => return Err(unsupported()),
        }
    } else {
        let exact = |idx: usize| -> Result<Intent> {
            if words.len() > idx + 2 {
                return Err(unsupported());
            }
            Ok(Intent::Resume(native_id(
                words.get(idx).map(|w| w.0).unwrap_or(""),
            )?))
        };
        match command {
            Some("login" | "logout" | "features" | "doctor" | "help") => Intent::Misc,
            Some("mcp")
                if matches!(
                    sub,
                    Some("list" | "get" | "add" | "remove" | "login" | "logout")
                ) =>
            {
                Intent::Misc
            }
            Some("exec" | "e") if sub == Some("resume") => exact(2)?,
            Some("exec" | "e") if matches!(sub, Some("fork" | "review" | "help")) => {
                return Err(unsupported());
            }
            Some("exec" | "e") if words.len() <= 2 => Intent::New,
            Some("resume") => exact(1)?,
            Some(
                "agents" | "review" | "mcp" | "plugin" | "app-server" | "completion" | "update"
                | "sandbox" | "debug" | "apply" | "a" | "queue" | "archive" | "delete"
                | "migrate-rollouts" | "migrate-nfs-db" | "unarchive" | "fork" | "exec-server"
                | "exec" | "e",
            ) => return Err(unsupported()),
            _ if words.len() <= 1 => Intent::New,
            _ => return Err(unsupported()),
        }
    };
    if claude {
        if no_persistence && (!print_mode || intent != Intent::New)
            || autocompact && (!print_mode || intent == Intent::Misc)
        {
            return Err(unsupported());
        }
        if print_mode && intent == Intent::New {
            let mut native = args.to_vec();
            if !no_persistence {
                native.insert(0, "--no-session-persistence".into());
            }
            intent = Intent::Print(native);
        }
    } else if ephemeral {
        if intent != Intent::New || tui_only {
            return Err(unsupported());
        }
        intent = Intent::Print(args.to_vec());
    }
    Ok(Invocation {
        intent,
        agy_tui: false,
        agy_text_stdin: false,
        workspace,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn parse(t: &str, a: &[&str]) -> Result<Invocation> {
        classify(
            t,
            &a.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            Path::new("/"),
        )
    }
    fn print(args: &[&str]) -> Intent {
        Intent::Print(args.iter().map(|s| s.to_string()).collect())
    }
    #[test]
    fn claude_caller_ids_require_the_durable_stdio_protocol() {
        let id = "9e230000-0000-4000-8000-000000000001";
        let selector = format!("--session-id={id}");
        let args = vec![
            "--print",
            "--output-format",
            "stream-json",
            "--replay-user-messages",
            "--verbose",
            "--input-format",
            "stream-json",
            "--model",
            "sonnet[1m]",
            "--autocompact",
            "200k",
            "--dangerously-skip-permissions",
            "--permission-prompt-tool",
            "stdio",
            "--append-system-prompt",
            "test guidance",
            "--effort",
            "high",
            &selector,
            "-p",
            "",
        ];
        assert_eq!(
            parse("claude", &args).unwrap().intent,
            Intent::ClaudeStreamNew(id.into())
        );
        let io = [
            "--output-format=stream-json",
            "-p",
            "--input-format=stream-json",
        ];
        let reordered = [
            "--session-id",
            id,
            "--permission-prompt-tool=stdio",
            "--permission-mode=plan",
            io[0],
            io[1],
            io[2],
        ];
        assert_eq!(
            parse("claude", &reordered).unwrap().intent,
            Intent::ClaudeStreamNew(id.into())
        );
        for prompt in ["--session-id", &selector] {
            let literal = [io[0], io[1], io[2], "--", prompt];
            assert_eq!(
                parse("claude", &literal).unwrap().intent,
                print(&[&["--no-session-persistence"][..], &literal].concat())
            );
        }
        assert_eq!(
            parse(
                "claude",
                &[io[0], "-p", io[2], "--append-system-prompt", "auth"]
            )
            .unwrap()
            .intent
            .name(),
            "print"
        );
        for (from, to) in [
            ("--replay-user-messages", "--no-session-persistence"),
            ("--input-format", "--output-format"),
            ("stream-json", "text"),
            ("200k", "--resume"),
            ("stdio", "other"),
            ("test guidance", "--settings"),
            (
                &selector,
                "--session-id=9E230000-0000-4000-8000-000000000001",
            ),
            (&selector, "--session-id=9e230000000040008000000000000001"),
            (&selector, "--continue"),
            ("", "ordinary one-shot"),
        ] {
            let mut bad = args.clone();
            *bad.iter_mut().find(|s| **s == from).unwrap() = to;
            assert!(parse("claude", &bad).is_err(), "{from}: {to}");
        }
        for extra in [
            &selector[..],
            "--resume=9e230000-0000-4000-8000-000000000001",
            "--no-session-persistence",
            "--settings=untrusted",
            "--fork-session",
        ] {
            let mut bad = args.clone();
            bad.insert(bad.len() - 3, extra);
            assert!(parse("claude", &bad).is_err());
        }
        assert!(parse("claude", &["--print", &selector, "-p", ""]).is_err());
    }
    #[test]
    fn print_switches_preserve_option_arity_and_literal_prompts() {
        for marker in ["-p", "--print"] {
            let args = [
                "--model",
                "opus",
                "question",
                marker,
                "--permission-mode=plan",
                "--output-format",
                "json",
            ];
            let mut expected = vec!["--no-session-persistence"];
            expected.extend(args);
            assert_eq!(parse("claude", &args).unwrap().intent, print(&expected));
            let explicit = ["--no-session-persistence", marker, "--", "-p"];
            assert_eq!(parse("claude", &explicit).unwrap().intent, print(&explicit));
            for tool in ["agy", "claude", "codex"] {
                let value = format!("--model={marker}");
                assert_eq!(parse(tool, &[&value]).unwrap().intent, Intent::New);
                assert!(parse(tool, &["--model", marker]).is_err());
            }
            for args in [
                vec![
                    "--sandbox=true",
                    marker,
                    "--model",
                    "test",
                    "question",
                    "--output-format=json",
                ],
                vec![
                    "question",
                    "--sandbox=true",
                    "--model",
                    "test",
                    "--output-format=json",
                    marker,
                ],
            ] {
                assert_eq!(
                    parse("agy", &args).unwrap().intent,
                    print(&[
                        "--sandbox=true",
                        "--model",
                        "test",
                        "--output-format=json",
                        "--print=question"
                    ])
                );
            }
            assert_eq!(
                parse("agy", &[marker, "--", "--sandbox"]).unwrap().intent,
                print(&["--print=--sandbox"])
            );
            assert_eq!(
                parse("agy", &[marker]).unwrap().intent,
                print(&["--print="])
            );
            assert_eq!(
                parse("agy", &[marker, "models", "--model", "fixture"])
                    .unwrap()
                    .intent,
                print(&["--model", "fixture", "--print=models"])
            );
        }
        assert_eq!(parse("agy", &["models"]).unwrap().intent, Intent::Misc);
        for args in [
            vec!["models", "--print"],
            vec!["--print", "/usage"],
            vec!["--print=/quota"],
            vec!["-p", "/credits"],
            vec!["-p", "--conversation=60000000-0000-4000-8000-000000000002"],
            vec!["-i", "chat", "--print"],
            vec!["--print", "a", "b"],
        ] {
            assert!(parse("agy", &args).is_err(), "{args:?}");
        }
    }
    #[test]
    fn codex_requires_explicit_local_ephemeral_exec_and_never_reinterprets_profile() {
        for args in [
            vec!["--print", "question"],
            vec!["-p", "native-profile"],
            vec!["--profile=work", "exec", "--ephemeral"],
            vec!["exec", "-p", "work", "--ephemeral"],
            vec!["--ephemeral", "exec", "question"],
            vec![
                "exec",
                "--ephemeral",
                "resume",
                "60000000-0000-4000-8000-000000000002",
            ],
            vec![
                "e",
                "resume",
                "60000000-0000-4000-8000-000000000002",
                "--ephemeral",
            ],
            vec!["exec", "--ephemeral", "review"],
        ] {
            assert!(parse("codex", &args).is_err(), "{args:?}");
        }
        for args in [
            vec![
                "-m=--print",
                "exec",
                "--json",
                "--ephemeral",
                "-s",
                "read-only",
                "--",
                "--resume",
            ],
            vec!["-m", "exec", "e", "question", "--ephemeral", "--json"],
            vec!["exec", "--ephemeral", "--image=a.png", "describe"],
        ] {
            assert_eq!(parse("codex", &args).unwrap().intent, print(&args));
        }
        assert_eq!(
            parse(
                "codex",
                &[
                    "exec",
                    "--model=--ephemeral",
                    "--skip-git-repo-check",
                    "--",
                    "--ephemeral"
                ]
            )
            .unwrap()
            .intent,
            Intent::New
        );
    }
    #[test]
    fn codex_ephemeral_exec_accepts_only_the_known_background_query_settings() {
        let args = [
            "exec",
            "--ephemeral",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "--cd",
            ".",
            "--output-last-message",
            "reply",
            "--model",
            "test",
            "--config",
            "model_reasoning_effort=\"high\"",
            "--config",
            "approval_policy=\"never\"",
            "--config",
            "service_tier=\"default\"",
            "-",
        ];
        assert_eq!(parse("codex", &args).unwrap().intent, print(&args));
        let reordered = [
            "e",
            "--config",
            "approval_policy=\"never\"",
            "--ephemeral",
            "-",
        ];
        assert_eq!(
            parse("codex", &reordered).unwrap().intent,
            print(&reordered)
        );
        for value in [
            "model_provider=\"other\"",
            "model_providers={}",
            "model_catalog_json=\"other\"",
            "cli_auth_credentials_store=\"file\"",
            "service_tier=\"default\"\nprofile='other'",
            "approval_policy=",
            "service_tier=\"fast\"",
        ] {
            assert!(parse("codex", &["exec", "--ephemeral", "--config", value, "-"]).is_err());
        }
        for args in [
            vec!["exec", "--config", "approval_policy=\"never\"", "-"],
            vec!["--config", "approval_policy=\"never\""],
            vec!["exec", "--ephemeral", "--config"],
        ] {
            assert!(parse("codex", &args).is_err());
        }
    }
    #[test]
    fn configured_stdio_forwards_only_native_settings() {
        let id = "60000000-0000-4000-8000-000000000002";
        let parse = |args: &[&str]| {
            codex_stdio_command(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        let flags = [
            "app-server",
            "--listen",
            "stdio://",
            "--config",
            "features.goals=false",
            "--config",
            "features.wake_on_agent_messages=false",
            "--config",
            "features.default_mode_request_user_input=true",
        ];
        assert_eq!(
            parse(&flags).unwrap(),
            (Intent::New, flags.map(str::to_owned).to_vec())
        );
        let (intent, argv) = parse(&[&flags[..], &["--for-session", id]].concat()).unwrap();
        assert_eq!(intent, Intent::Resume(id.into()));
        assert_eq!(argv, flags);
        assert_eq!(parse(&["app-server", "--stdio"]).unwrap().1, ["app-server"]);
        for args in [
            vec!["app-server", "--for-session", id, "--config"],
            vec![
                "app-server",
                "--for-session",
                id,
                "--config",
                "features.goals=true",
            ],
            vec![
                "app-server",
                "--for-session",
                id,
                "--config",
                "model_providers={}",
            ],
            vec![
                "app-server",
                "--for-session",
                id,
                "--config",
                "model_catalog_json=\"elsewhere\"",
            ],
            vec!["app-server", "--for-session", id, "--listen", "ws://remote"],
            vec![
                "app-server",
                "--config",
                "features.goals=false",
                "--for-session",
                "--last",
            ],
        ] {
            assert!(parse(&args).is_err());
        }
    }
    #[test]
    fn agy_tui_values_do_not_select_a_command_or_session() {
        let id = "a0000000-0000-4000-8000-000000000012";
        for flag in ["--sandbox", "--dangerously-skip-permissions"] {
            for value in ["true", "false"] {
                let opt = format!("{flag}={value}");
                let i = parse("agy", &[&opt, "--conversation", id]).unwrap();
                assert_eq!(i.agy_conversation(), Some(id));
            }
        }
        for (flag, value) in [
            ("--model", "acp"),
            ("--effort", "high"),
            ("--mode", "auto"),
            ("--agent", "models"),
            ("--add-dir", "acp"),
            ("-i", "acp"),
            ("--prompt-interactive", "models"),
        ] {
            let inline = format!("{flag}={value}");
            for args in [vec![flag, value, "--sandbox"], vec![inline.as_str()]] {
                let i = parse("agy", &args).unwrap();
                assert!(i.agy_tui);
                assert_eq!(i.intent, Intent::New);
            }
        }
        let prompt = format!("-i=--conversation={id}");
        assert_eq!(parse("agy", &[&prompt]).unwrap().intent, Intent::New);
        for verb in ["models", "--version", "--help"] {
            let i = parse("agy", &[verb]).unwrap();
            assert!(!i.agy_tui);
            assert_eq!(i.intent, Intent::Misc);
        }
        let acp = parse("agy", &["acp", "--for-session", id]).unwrap();
        assert_eq!(acp.intent.id(), Some(id));
        assert!(!acp.agy_tui);
        let resume = parse("agy", &["--sandbox", "--conversation", id, "--agent=acp"]).unwrap();
        assert_eq!(resume.agy_conversation(), Some(id));
    }
    #[test]
    fn agy_tui_refuses_ambiguous_arguments() {
        let id = "a0000000-0000-4000-8000-000000000012";
        for args in [
            vec![
                "--conversation",
                id,
                "--model",
                "local",
                "--conversation",
                id,
            ],
            vec!["--conversation"],
            vec!["--conversation="],
            vec!["--conversation", "last"],
            vec!["--conversation", &id.to_uppercase()],
            vec!["--model"],
            vec!["--mode="],
            vec!["--agent", ""],
            vec!["--effort", "--sandbox"],
            vec!["--add-dir", "one", "two"],
            vec!["-i"],
            vec!["--sandbox=oops"],
            vec!["--dangerously-skip-permissions=maybe"],
            vec!["--sandbox", "false"],
            vec!["--continue"],
            vec!["--for-session", id],
            vec!["acp", "--conversation", id],
            vec!["--sandbox", "acp"],
            vec!["models", "--sandbox"],
            vec!["--model=x", "models"],
            vec!["--new-project"],
            vec!["--settings=x"],
            vec!["--remote-agent=x"],
            vec!["--gemini-dir", "elsewhere"],
            vec!["--app-data-dir", "elsewhere"],
            vec!["--log-file=x"],
            vec!["--login"],
            vec!["--print", "--new-project"],
            vec!["--stdio"],
            vec!["--", "chat"],
            vec!["unknown"],
        ] {
            assert!(parse("agy", &args).is_err(), "{args:?}");
        }
    }
    #[test]
    fn codex_local_options_and_delimiters_do_not_change_routing() {
        for flag in [
            "--no-daemon",
            "--strict-config",
            "--search",
            "--worktree",
            "--approve-for-me",
        ] {
            assert_eq!(parse("codex", &[flag]).unwrap().intent, Intent::New);
        }
        for args in [
            vec!["-m", "exec", "exec", "--json", "review this"],
            vec!["--", "exec"],
            vec![
                "exec",
                "--sandbox",
                "read-only",
                "--image=photo.png",
                "hello",
            ],
            vec![
                "-a",
                "never",
                "--no-alt-screen",
                "--add-dir",
                "../docs",
                "--",
                "--last",
            ],
            vec!["--image=a.png", "--", "resume"],
            vec!["exec", "--yolo"],
            vec![
                "--no-alt-screen",
                "--dangerously-bypass-approvals-and-sandbox",
                "prompt",
            ],
        ] {
            assert_eq!(parse("codex", &args).unwrap().intent, Intent::New);
        }
        for flag in [
            "--hook-free",
            "--remote=unix:///elsewhere",
            "--enable",
            "--remote-auth-token-env",
        ] {
            assert!(parse("codex", &[flag]).is_err());
        }
        assert_eq!(
            parse("codex", &["login", "status"]).unwrap().intent,
            Intent::Misc
        );
    }

    #[test]
    fn local_chat_options_allow_new_chats_and_exact_resumes() {
        let id = "60000000-0000-4000-8000-000000000002";
        for (tool, args) in [
            (
                "claude",
                vec!["--resume", id, "--dangerously-skip-permissions"],
            ),
            (
                "claude",
                vec![
                    "--permission-mode",
                    "plan",
                    "--name",
                    "update",
                    "-r",
                    id,
                    "--model=opus",
                    "--ax-screen-reader",
                    "--",
                    "--continue",
                ],
            ),
            (
                "claude",
                vec![
                    "--allow-dangerously-skip-permissions",
                    "-r",
                    id,
                    "-n",
                    "my session",
                    "--effort",
                    "high",
                    "go on",
                ],
            ),
            (
                "claude",
                vec![
                    "-p",
                    "--permission-prompts=none",
                    "--no-chrome",
                    "--resume",
                    id,
                    "--",
                    "doctor",
                ],
            ),
            (
                "codex",
                vec!["resume", id, "--dangerously-bypass-approvals-and-sandbox"],
            ),
            (
                "codex",
                vec![
                    "--model",
                    "resume",
                    "--no-alt-screen",
                    "resume",
                    "-s",
                    "read-only",
                    id,
                    "--",
                    "--last",
                ],
            ),
            (
                "codex",
                vec!["e", "--model=exec", "resume", "--json", id, "--yolo", "-"],
            ),
        ] {
            assert_eq!(
                parse(tool, &args).unwrap().intent,
                Intent::Resume(id.into()),
                "{args:?}"
            );
        }
        assert_eq!(
            parse("claude", &["-n", "auth", "--ax-screen-reader", "hello"])
                .unwrap()
                .intent,
            Intent::New
        );
        assert_eq!(
            parse(
                "codex",
                &["--no-alt-screen", "-m", "resume", "--", "--last"]
            )
            .unwrap()
            .intent,
            Intent::New
        );
        for (tool, args) in [
            ("claude", vec!["--name", "--resume", id]),
            ("claude", vec!["--resume", id, "-n"]),
            (
                "claude",
                vec!["--permission-prompts", "--no-chrome", "-r", id],
            ),
            ("claude", vec!["--ax-screen-reader=false", "-r", id]),
            ("claude", vec!["--name", "display", "--", "update"]),
            (
                "claude",
                vec!["--ax-screen-reader", "--fork-session", "-r", id],
            ),
            (
                "codex",
                vec!["app-server", "--for-session", id, "--model=m"],
            ),
        ] {
            assert!(parse(tool, &args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn codex_resume_images_follow_native_arity() {
        let id = "60000000-0000-4000-8000-000000000002";
        for args in [
            vec!["resume", id, "-i", "a.png", "b.png", "--", "--last"],
            vec!["--image=a.png,b.png", "resume", id, "describe"],
            vec!["--image", "exec", "-m", "m", "resume", id],
            vec!["resume", "--image=a.png", id, "describe"],
            vec!["exec", "--image=a.png", "resume", id, "describe"],
            vec!["e", "resume", "-i", "a.png,b.png", id, "describe"],
            vec!["exec", "resume", id, "-i", "a.png", "describe"],
        ] {
            assert_eq!(
                parse("codex", &args).unwrap().intent,
                Intent::Resume(id.into()),
                "{args:?}"
            );
        }
        for args in [
            vec!["-i", "a.png", "resume", id],
            vec!["resume", "-i", "a.png", id],
            vec!["exec", "-i", "a.png", "resume", id],
            vec!["--image=a.png", "cloud", "--help"],
            vec!["--image=a.png", "fork", id],
            vec!["--image=a.png", "resume", "--last"],
            vec!["resume", id, "--image"],
            vec!["resume", id, "-i", "--"],
            vec!["resume", id, "--image="],
            vec!["exec", "resume", "-i", "a.png", "b.png", id],
            vec!["app-server", "--for-session", id, "--image=a.png"],
        ] {
            assert!(parse("codex", &args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn routes_the_grammar_not_substrings() {
        let id = "60000000-0000-4000-8000-000000000002";
        for (t, args, want) in [
            (
                "claude",
                vec!["-p", "--model", "resume", "--", "--resume"],
                print(&[
                    "--no-session-persistence",
                    "-p",
                    "--model",
                    "resume",
                    "--",
                    "--resume",
                ]),
            ),
            (
                "claude",
                vec!["-r", id, "continue"],
                Intent::Resume(id.into()),
            ),
            ("codex", vec!["--", "resume"], Intent::New),
            (
                "codex",
                vec!["exec", "--skip-git-repo-check", "--json", "--", "resume"],
                Intent::New,
            ),
            (
                "codex",
                vec!["-m", "resume", "exec", "resume", id, "prompt"],
                Intent::Resume(id.into()),
            ),
            ("codex", vec!["login", "status"], Intent::Misc),
            ("claude", vec!["help"], Intent::New),
            ("claude", vec!["--help"], Intent::Misc),
            ("claude", vec!["auth", "status"], Intent::Misc),
            (
                "claude",
                vec!["-p", "--", "doctor"],
                print(&["--no-session-persistence", "-p", "--", "doctor"]),
            ),
        ] {
            assert_eq!(parse(t, &args).unwrap().intent, want)
        }
        for (t, args) in [
            ("claude", vec!["-r"]),
            ("claude", vec!["--session-id", id]),
            ("claude", vec!["--resume", id, "--fork-session"]),
            ("claude", vec!["--", "update"]),
            ("claude", vec!["--", "doctor"]),
            ("claude", vec!["-p", "doctor"]),
            ("claude", vec!["--", "sandbox"]),
            ("claude", vec!["apply-project-settings"]),
            ("codex", vec!["resume", "--last"]),
            ("codex", vec!["exec", "resume", "name"]),
            ("codex", vec!["--disable", "hooks"]),
            ("codex", vec!["--config", "a=b"]),
            ("codex", vec!["fork", id]),
            ("codex", vec!["--ephemeral"]),
        ] {
            assert!(parse(t, &args).is_err(), "{args:?}")
        }
    }
    #[test]
    fn internal_claude_commands_never_allocate_chats() {
        for name in [
            "project",
            "edit-hook",
            "edit-permission-rules",
            "edit-memory-settings",
            "edit-skill-overrides",
            "edit-sandbox-settings",
            "edit-chrome-settings",
            "design-login",
            "workspace-trust",
            "worktree-hook",
        ] {
            for args in [
                vec![name],
                vec![name, "--help"],
                vec!["--", name],
                vec!["-p", name],
                vec!["-p", "--", name],
            ] {
                assert_eq!(
                    parse("claude", &args).err().unwrap().1,
                    "unsupported_invocation"
                );
            }
        }
    }
    #[test]
    fn native_service_commands_are_not_local_chats() {
        for (tool, cmd) in [
            ("claude", "remote-control"),
            ("claude", "rc"),
            ("claude", "remote"),
            ("claude", "sync"),
            ("claude", "bridge"),
            ("codex", "cloud-tasks"),
            ("codex", "cloud"),
            ("codex", "remote-control"),
            ("codex", "tcp-tunnel"),
            ("codex", "git-credential"),
            ("codex", "execpolicy"),
            ("codex", "responses-api-proxy"),
        ] {
            for args in [vec![cmd], vec![cmd, "--help"]] {
                assert_eq!(
                    parse(tool, &args).err().unwrap().1,
                    "unsupported_invocation"
                );
            }
            let delimited = parse(tool, &["--", cmd]);
            if tool == "claude" && matches!(cmd, "rc" | "remote-control") {
                assert_eq!(delimited.err().unwrap().1, "unsupported_invocation");
                assert_eq!(
                    parse(tool, &["-p", "--", cmd]).unwrap().intent,
                    print(&["--no-session-persistence", "-p", "--", cmd])
                );
            } else {
                assert_eq!(delimited.unwrap().intent, Intent::New);
            }
            assert_eq!(parse(tool, &["--model", cmd]).unwrap().intent, Intent::New);
        }
    }

    #[test]
    fn mcp_maintenance_accepts_native_flag_values_only_under_mcp() {
        for (tool, args) in [
            (
                "claude",
                vec![
                    "mcp",
                    "add",
                    "--transport",
                    "http",
                    "--scope=user",
                    "srv",
                    "https://localhost/mcp",
                ],
            ),
            ("claude", vec!["mcp", "add-json", "-s", "user", "srv", "-"]),
            (
                "codex",
                vec![
                    "mcp",
                    "add",
                    "srv",
                    "--env",
                    "A=B",
                    "--",
                    "command",
                    "--child-option",
                ],
            ),
            (
                "codex",
                vec![
                    "mcp",
                    "add",
                    "srv",
                    "--url",
                    "https://localhost/mcp",
                    "--bearer-token-env-var",
                    "MCP_TOKEN",
                ],
            ),
        ] {
            assert_eq!(parse(tool, &args).unwrap().intent, Intent::Misc);
        }
        assert!(parse("claude", &["--scope", "user"]).is_err());
        // This Claude option prompts for the secret; it takes no inline value.
        assert!(parse("claude", &["mcp", "add", "--client-secret=fixture"]).is_err());
        assert!(parse("codex", &["--url", "https://localhost"]).is_err());
    }
}
