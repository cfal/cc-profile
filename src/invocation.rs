use crate::types::{Error, Result};
use std::ops::Range;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq)]
pub enum Intent {
    New,
    Resume(String),
    Misc,
}
impl Intent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Resume(_) => "resume",
            Self::Misc => "misc",
        }
    }
    pub fn id(&self) -> Option<&str> {
        if let Self::Resume(id) = self {
            Some(id)
        } else {
            None
        }
    }
}
pub struct Invocation {
    pub intent: Intent,
    pub codex: CodexArgs,
    pub agy_tui: bool,
    pub workspace: PathBuf,
}
impl Invocation {
    pub fn agy_conversation(&self) -> Option<&str> {
        self.intent.id().filter(|_| self.agy_tui)
    }
}
#[derive(Default)]
pub struct CodexArgs {
    pub exec: Option<usize>,
    pub cd: Option<Range<usize>>,
    pub model: Option<(Range<usize>, String)>,
}
fn app_server(args: &[String], cwd: &Path) -> Result<Invocation> {
    let mut it = args[1..].iter().map(String::as_str);
    let mut intent = Intent::New;
    let mut stdio = false;
    while let Some(flag) = it.next() {
        match flag {
            "--stdio" | "--listen=stdio://" if !stdio => stdio = true,
            "--listen" if !stdio && it.next() == Some("stdio://") => stdio = true,
            "--for-session" if intent == Intent::New => {
                intent = Intent::Resume(native_id(it.next().unwrap_or(""))?)
            }
            _ => return Err(unsupported()),
        }
    }
    Ok(Invocation {
        intent,
        codex: CodexArgs::default(),
        agy_tui: false,
        workspace: cwd.into(),
    })
}
pub fn native_id(raw: &str) -> Result<String> {
    let parsed = Uuid::parse_str(raw).ok().filter(|_| raw.len() == 36);
    parsed.map(|u|u.to_string()).ok_or_else(||Error::bad("exact_id_required","Use an exact native session UUID from Sessions. Pickers, names and --last are not supported."))
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
fn agy_tui_intent(args: &[String]) -> Result<Intent> {
    let invalid = || {
        Error::bad(
            "unsupported_invocation",
            "Unsupported or ambiguous AgY terminal arguments. Use -i <prompt> for interactive input and one --conversation=<uuid> to resume.",
        )
    };
    let mut resume = None;
    let mut it = args.iter().map(String::as_str);
    while let Some(arg) = it.next() {
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
            "--model"
            | "--effort"
            | "--mode"
            | "--agent"
            | "--add-dir"
            | "-i"
            | "--prompt-interactive" => {
                value()?;
            }
            _ => return Err(invalid()),
        }
    }
    Ok(resume.map(Intent::Resume).unwrap_or(Intent::New))
}
pub fn classify(tool: &str, args: &[String], cwd: &Path) -> Result<Invocation> {
    if tool == "agy" {
        let (intent, agy_tui) = if matches!(args, [a] if matches!(a.as_str(), "--version" | "--help" | "models"))
            || matches!(args, [a,b] if a=="acp" && b=="--help")
        {
            (Intent::Misc, false)
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
            (intent, false)
        } else {
            (agy_tui_intent(args)?, true)
        };
        return Ok(Invocation {
            intent,
            agy_tui,
            workspace: cwd.into(),
            codex: Default::default(),
        });
    }
    let claude = tool == "claude";
    if !claude
        && args.first().is_some_and(|s| s == "app-server")
        && !matches!(args.get(1).map(String::as_str), Some("--help" | "-h"))
    {
        return app_server(args, cwd);
    }
    let mut workspace = cwd.to_owned();
    let mut words = Vec::new();
    let mut first_word = None;
    let mut local_only_option = false;
    let mut codex_images = false;
    let mut codex = CodexArgs::default();
    let mut skip_git_check = false;
    let mut tail = false;
    let mut resume = None;
    let mut help = false;
    let mut print_mode = false;
    let mut it = args.iter().peekable();
    let positional = |arg: &str| !arg.starts_with('-') || arg == "-";
    while let Some(arg) = it.next() {
        let start = args.len() - it.len() - 1;
        let arg = arg.as_str();
        if tail || positional(arg) {
            first_word.get_or_insert(start);
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
        local_only_option |= matches!(
            key,
            "--no-daemon"
                | "--add-dir"
                | "--strict-config"
                | "--search"
                | "-s"
                | "--sandbox"
                | "-a"
                | "--ask-for-approval"
                | "--output-schema"
        );
        skip_git_check |= key == "--skip-git-repo-check";
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
            (true, "-r" | "--resume") => {
                if resume.is_some() {
                    return Err(unsupported());
                }
                resume = Some(native_id(value().unwrap_or(""))?);
            }
            (false, "-C" | "--cd") => {
                let s = value()?;
                if s.is_empty() || codex.cd.is_some() {
                    return Err(unsupported());
                }
                codex.cd = Some(start..args.len() - it.len());
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
                if s.is_empty() || (s.starts_with('-') && inline.is_none()) || codex.model.is_some()
                {
                    return Err(unsupported());
                }
                codex.model = Some((start..args.len() - it.len(), s.to_owned()));
            }
            (false, "-i" | "--image") => {
                let file = value()?;
                if file.is_empty() || (inline.is_none() && !positional(file)) {
                    return Err(unsupported());
                }
                codex_images = true;
                // Pinned Codex consumes a list, except for an attached value or after exec resume.
                if inline.is_none()
                    && !matches!(
                        words.as_slice(),
                        [("exec" | "e", true), ("resume", true), ..]
                    )
                {
                    while it.next_if(|arg| positional(arg)).is_some() {}
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
            (
                true,
                "--model"
                | "-n"
                | "--name"
                | "--effort"
                | "--output-format"
                | "--input-format"
                | "--max-turns"
                | "--max-budget-usd"
                | "--permission-mode"
                | "--permission-prompts"
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
            (false, "--dangerously-bypass-approvals-and-sandbox" | "--yolo")
                if inline.is_none() =>
            {
                local_only_option = true;
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
                | "--skip-git-repo-check"
                | "--strict-config",
            ) if inline.is_none() => {}
            (_, "--last" | "--continue") | (true, "-c") => return Err(native_id("").unwrap_err()),
            _ => return Err(unsupported()),
        }
    }
    let command = words.first().filter(|(_, cmd)| *cmd).map(|(w, _)| *w);
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
    let intent = if help {
        Intent::Misc
    } else if claude {
        match command {
            Some("doctor" | "auth") if resume.is_none() => Intent::Misc,
            Some("mcp")
                if resume.is_none()
                    && matches!(sub, Some("list" | "get" | "add" | "add-json" | "remove")) =>
            {
                Intent::Misc
            }
            Some(word) if claude_command(word) => return Err(unsupported()),
            _ if words.len() <= 1 => resume.map(Intent::Resume).unwrap_or(Intent::New),
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
    if !claude {
        if codex_images && !matches!(intent, Intent::Resume(_)) {
            return Err(Error::bad(
                "unsupported_invocation",
                "Image attachments require an exact Codex resume. Use resume UUID --image=FILE before the prompt.",
            ));
        }
        if matches!(command, Some("exec" | "e")) {
            codex.exec = first_word;
        }
        if intent == Intent::New {
            if local_only_option {
                return Err(Error::bad(
                    "unsupported_invocation",
                    "This option is incompatible with a new managed Codex chat; configure the profile instead.",
                ));
            }
            // Codex 0.171.0 checks this in local exec, but not in exec --remote.
            if codex.exec.is_some()
                && !skip_git_check
                && !workspace.ancestors().any(|dir| {
                    let git = dir.join(".git");
                    git.is_file() || git.join("HEAD").is_file()
                })
            {
                return Err(git_required());
            }
        }
    }
    Ok(Invocation {
        intent,
        codex,
        agy_tui: false,
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
            vec!["--print"],
            vec!["--stdio"],
            vec!["--", "chat"],
            vec!["unknown"],
        ] {
            assert!(parse("agy", &args).is_err(), "{args:?}");
        }
    }
    #[test]
    fn codex_injects_remote_after_the_exec_verb_not_an_option_value() {
        assert_eq!(
            parse(
                "codex",
                &[
                    "-m",
                    "exec",
                    "exec",
                    "--skip-git-repo-check",
                    "--json",
                    "review this"
                ]
            )
            .unwrap()
            .codex
            .exec,
            Some(2)
        );
        assert_eq!(parse("codex", &["--", "exec"]).unwrap().codex.exec, None);
        for flag in ["--hook-free", "--no-daemon", "--strict-config", "--search"] {
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
            ("codex", vec!["exec", "--skip-git-repo-check", "--yolo"]),
            (
                "claude",
                vec!["--ax-screen-reader", "--fork-session", "-r", id],
            ),
            (
                "codex",
                vec![
                    "--no-alt-screen",
                    "--dangerously-bypass-approvals-and-sandbox",
                    "prompt",
                ],
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
    fn images_follow_native_arity_only_on_exact_codex_resumes() {
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
            vec!["--image=a.png", "--", "resume"],
            vec!["--image=a.png", "cloud", "--help"],
            vec!["--image=a.png", "fork", id],
            vec!["--image=a.png", "resume", "--last"],
            vec![
                "exec",
                "--skip-git-repo-check",
                "--image=a.png",
                "new prompt",
            ],
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
                Intent::New,
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
            ("claude", vec!["-p", "--", "doctor"], Intent::New),
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
            ("codex", vec!["exec", "--ephemeral"]),
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
                assert_eq!(parse(tool, &["-p", "--", cmd]).unwrap().intent, Intent::New);
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
