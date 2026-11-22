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
    pub workspace: PathBuf,
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
pub fn classify(tool: &str, args: &[String], cwd: &Path) -> Result<Invocation> {
    if tool == "agy" {
        let mut intent = Intent::New;
        if matches!(args, [a] if a=="--version" || a=="--help")
            || matches!(args, [a,b] if a=="acp" && b=="--help")
        {
            intent = Intent::Misc;
        } else if !args.is_empty() {
            if args.first().is_none_or(|a| a != "acp") {
                return Err(unsupported());
            }
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
        }
        return Ok(Invocation {
            intent,
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
    let mut codex = CodexArgs::default();
    let mut skip_git_check = false;
    let mut tail = false;
    let mut resume = None;
    let mut help = false;
    let mut print_mode = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let start = args.len() - it.len() - 1;
        let arg = arg.as_str();
        if tail || !arg.starts_with('-') || arg == "-" {
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
                | "--dangerously-bypass-approvals-and-sandbox"
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
                | "--effort"
                | "--output-format"
                | "--input-format"
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
            (
                true,
                "--verbose"
                | "--include-partial-messages"
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
                | "--strict-config"
                | "--dangerously-bypass-approvals-and-sandbox",
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
