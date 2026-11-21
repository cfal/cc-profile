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
pub fn classify(tool: &str, args: &[String], cwd: &Path) -> Result<Invocation> {
    let claude = tool == "claude";
    let mut workspace = cwd.to_owned();
    let mut words = Vec::new();
    let mut tail = false;
    let mut resume = None;
    let mut help = false;
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
            (
                true,
                "-p"
                | "--print"
                | "--verbose"
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
    let mut interactive = false;
    let intent = if help {
        Intent::Misc
    } else if claude {
        match command {
            Some("doctor" | "auth" | "help") if resume.is_none() => Intent::Misc,
            Some("mcp")
                if resume.is_none()
                    && matches!(sub, Some("list" | "get" | "add" | "add-json" | "remove")) =>
            {
                Intent::Misc
            }
            Some(
                "agents" | "attach" | "auto-mode" | "gateway" | "import" | "install" | "logs"
                | "mcp" | "plugin" | "plugins" | "purge" | "respawn" | "rm" | "setup-token"
                | "stop" | "kill" | "ultrareview" | "update" | "upgrade" | "auth" | "doctor"
                | "help",
            ) => return Err(unsupported()),
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
                "agents" | "review" | "mcp" | "plugin" | "app-server" | "remote-control"
                | "completion" | "update" | "sandbox" | "debug" | "apply" | "a" | "queue"
                | "archive" | "delete" | "migrate-rollouts" | "migrate-nfs-db" | "unarchive"
                | "fork" | "cloud" | "exec-server" | "exec" | "e",
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
        ] {
            assert_eq!(parse(t, &args).unwrap().intent, want)
        }
        for (t, args) in [
            ("claude", vec!["-r"]),
            ("claude", vec!["--session-id", id]),
            ("claude", vec!["--resume", id, "--fork-session"]),
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
}
