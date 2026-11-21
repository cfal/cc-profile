use crate::types::{Error, Result};
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
    pub interactive_codex: bool,
    pub workspace: PathBuf,
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
    let claude = tool == "claude";
    let mut workspace = cwd.to_owned();
    let mut words = Vec::new();
    let mut tail = false;
    let mut resume = None;
    let mut help = false;
    let mut print_mode = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let arg = arg.as_str();
        if tail || !arg.starts_with('-') || arg == "-" {
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
                if s.is_empty() {
                    return Err(unsupported());
                }
                workspace = cwd
                    .join(s)
                    .canonicalize()
                    .map_err(|_| Error::bad("bad_cwd", "Working directory is unavailable."))?;
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
                "-m"
                | "--model"
                | "-s"
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
    let mut interactive = false;
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
            Some("resume") => {
                interactive = true;
                exact(1)?
            }
            Some(
                "agents" | "review" | "mcp" | "plugin" | "app-server" | "completion" | "update"
                | "sandbox" | "debug" | "apply" | "a" | "queue" | "archive" | "delete"
                | "migrate-rollouts" | "migrate-nfs-db" | "unarchive" | "fork" | "exec-server"
                | "exec" | "e",
            ) => return Err(unsupported()),
            _ if words.len() <= 1 => {
                interactive = true;
                Intent::New
            }
            _ => return Err(unsupported()),
        }
    };
    Ok(Invocation {
        intent,
        interactive_codex: interactive,
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
            ("codex", vec!["exec", "--json", "--", "resume"], Intent::New),
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
