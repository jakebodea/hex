//! Dictation rewriting through the installed Codex and Claude Code CLIs and
//! their own subscription sign-ins. Hex never reads either tool's
//! credentials: it runs the executable directly (no shell) in an empty
//! workspace, passes the prompt on stdin, strips API-key variables from the
//! child environment, and stops any session that would bill an API key.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, eyre};
use serde::Deserialize;

use crate::app_settings::RewriteProvider;
use crate::dictation_processor::ProcessingObservation;
use crate::rewrite_prompt::RewritePrompt;

const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_CHARS: usize = 300;
const STATUS_DEADLINE: Duration = Duration::from_secs(5);

const CODEX_SIGN_IN: &str = "Run `codex login` in Terminal and choose Sign in with ChatGPT.";
const CLAUDE_SIGN_IN: &str = "Run `claude` in Terminal and sign in with your Claude subscription.";

/// Variables that would route a CLI to separately billed API access. The
/// child never inherits them, so only the CLI's own sign-in remains.
const CODEX_REMOVED_ENV: &[&str] = &["OPENAI_API_KEY", "CODEX_API_KEY", "OPENAI_BASE_URL"];
const CLAUDE_REMOVED_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDECODE",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliProvider {
    Codex,
    Claude,
}

impl CliProvider {
    pub fn from_rewrite(provider: RewriteProvider) -> Option<Self> {
        match provider {
            RewriteProvider::OpenCode => None,
            RewriteProvider::Codex => Some(Self::Codex),
            RewriteProvider::Claude => Some(Self::Claude),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }

    pub fn program(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    /// An explicit executable path for installs Hex cannot discover.
    fn override_variable(self) -> &'static str {
        match self {
            Self::Codex => "HEX_CODEX_CLI",
            Self::Claude => "HEX_CLAUDE_CLI",
        }
    }

    pub fn install_url(self) -> &'static str {
        match self {
            Self::Codex => "https://developers.openai.com/codex/cli",
            Self::Claude => "https://docs.claude.com/en/docs/claude-code/setup",
        }
    }
}

/// One finished rewrite: the paste-ready text and how long the CLI took to
/// report its first structured event.
#[derive(Debug, PartialEq)]
pub struct CliRewrite {
    pub text: String,
    pub startup: Option<Duration>,
}

/// Runs one rewrite and reports it as a processing observation. The text is
/// `None` whenever the caller must fall back to the corrected transcript.
pub fn rewrite_observed(
    provider: RewriteProvider,
    model: Option<&str>,
    prompt: &RewritePrompt,
    deadline: Duration,
    cancelled: &AtomicBool,
    profile_name: &str,
) -> (Option<String>, ProcessingObservation) {
    let started = Instant::now();
    let result = CliProvider::from_rewrite(provider)
        .ok_or_else(|| eyre!("{} is not a CLI provider", provider.label()))
        .and_then(|cli| rewrite(cli, model, prompt, deadline, cancelled));
    let (text, startup, fallback) = match result {
        Ok(rewrite) if !rewrite.text.trim().is_empty() => {
            (Some(rewrite.text.trim().to_owned()), rewrite.startup, None)
        }
        Ok(rewrite) => (
            None,
            rewrite.startup,
            Some("processor returned empty text".to_owned()),
        ),
        Err(error) => (None, None, Some(error.to_string())),
    };
    let observation = ProcessingObservation {
        profile: profile_name.into(),
        latency_ms: started.elapsed().as_millis() as u64,
        fallback,
        provider: Some(provider.label().into()),
        startup_ms: startup.map(|startup| startup.as_millis() as u64),
    };
    (text, observation)
}

pub fn rewrite(
    provider: CliProvider,
    model: Option<&str>,
    prompt: &RewritePrompt,
    deadline: Duration,
    cancelled: &AtomicBool,
) -> Result<CliRewrite> {
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err(eyre!("{} rewrite was cancelled", provider.label()));
    }
    let executable = executable(provider).ok_or_else(|| {
        eyre!(
            "{} is not installed. Install the `{}` CLI and sign in.",
            provider.label(),
            provider.program()
        )
    })?;
    let workspace = crate::app_paths::cli_rewrite_workspace()?;
    std::fs::create_dir_all(&workspace)?;
    rewrite_with(
        provider,
        &executable,
        &workspace,
        model,
        prompt,
        deadline,
        cancelled,
    )
}

pub fn executable(provider: CliProvider) -> Option<PathBuf> {
    #[cfg(not(target_os = "windows"))]
    {
        let current_directory = std::env::current_dir().ok();
        crate::dictation_processor::find_cli_executable(
            provider.program(),
            std::env::var_os(provider.override_variable()).map(PathBuf::from),
            std::env::var_os("PATH").as_deref(),
            dirs::home_dir().as_deref(),
            current_directory.as_deref(),
        )
    }
    #[cfg(target_os = "windows")]
    {
        let _ = provider;
        None
    }
}

/// Model names go to the CLI as one `--model=` argument, never through a
/// shell; the allow-list still keeps them from reading as another flag.
fn validated_model(model: Option<&str>) -> Result<Option<&str>> {
    let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) else {
        return Ok(None);
    };
    let valid = model.len() <= 100
        && !model.starts_with('-')
        && model
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "._:/@[]-".contains(ch));
    if valid {
        Ok(Some(model))
    } else {
        Err(eyre!(
            "model name {model:?} contains unsupported characters"
        ))
    }
}

/// The complete CLI invocation. Both run ephemerally with no tools, no MCP
/// servers, no user or project configuration, and minimal reasoning.
fn arguments(provider: CliProvider, model: Option<&str>, workspace: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = match provider {
        CliProvider::Codex => [
            "exec",
            "--json",
            "--ephemeral",
            "--skip-git-repo-check",
            "--ignore-user-config",
            "--ignore-rules",
            "--sandbox",
            "read-only",
            "--color",
            "never",
            "-c",
            "model_reasoning_effort=\"low\"",
            "--cd",
        ]
        .into_iter()
        .map(OsString::from)
        .chain([workspace.as_os_str().to_owned()])
        .collect(),
        CliProvider::Claude => [
            "--print",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
            "--tools",
            "",
            "--strict-mcp-config",
            "--setting-sources",
            "",
            "--disable-slash-commands",
            "--effort",
            "low",
            "--system-prompt",
            crate::rewrite_prompt::SYSTEM_PROMPT,
        ]
        .into_iter()
        .map(OsString::from)
        .collect(),
    };
    if let Some(model) = model {
        args.push(format!("--model={model}").into());
    }
    if provider == CliProvider::Codex {
        // `-` reads the prompt from stdin.
        args.push("-".into());
    }
    args
}

/// What the CLI reads on stdin. Claude receives the rules as its system
/// prompt; Codex has no system-prompt flag, so it reads both.
fn input(provider: CliProvider, prompt: &RewritePrompt) -> String {
    match provider {
        CliProvider::Codex => prompt.combined(),
        CliProvider::Claude => prompt.user.clone(),
    }
}

/// Parses one CLI's JSON event stream. `observe` may stop the run early,
/// for example when Claude reports an API-key session or Codex starts a tool.
#[derive(Default)]
struct EventParser {
    text: Option<String>,
    error: Option<String>,
}

impl EventParser {
    fn observe(&mut self, provider: CliProvider, line: &str) -> Result<()> {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            return Ok(());
        };
        let kind = event
            .get("type")
            .and_then(|kind| kind.as_str())
            .unwrap_or("");
        match provider {
            CliProvider::Claude => match kind {
                "system" if event.get("subtype").and_then(|v| v.as_str()) == Some("init") => {
                    if let Some(source) = event.get("apiKeySource").and_then(|v| v.as_str())
                        && source != "none"
                    {
                        return Err(eyre!(
                            "Claude Code would use an API key ({}) instead of your Claude subscription, so Hex stopped. Remove the key or sign in with `claude auth login`.",
                            bounded(source)
                        ));
                    }
                }
                "result" => {
                    let result = event
                        .get("result")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if event.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                        self.error = Some(claude_error(result));
                    } else {
                        self.text = Some(result.to_owned());
                    }
                }
                _ => {}
            },
            CliProvider::Codex => match kind {
                "item.started" | "item.updated" | "item.completed" => {
                    let item = event.get("item");
                    let item_type = item
                        .and_then(|item| item.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    match item_type {
                        "agent_message" if kind == "item.completed" => {
                            self.text = item
                                .and_then(|item| item.get("text"))
                                .and_then(|v| v.as_str())
                                .map(str::to_owned);
                        }
                        "command_execution" | "file_change" | "mcp_tool_call" | "web_search" => {
                            return Err(eyre!(
                                "Codex tried to use a tool instead of rewriting, so Hex stopped it"
                            ));
                        }
                        _ => {}
                    }
                }
                "turn.failed" => {
                    let message = event
                        .pointer("/error/message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("turn failed");
                    self.error = Some(codex_error(message));
                }
                "error" => {
                    let message = event
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    self.error = Some(codex_error(message));
                }
                _ => {}
            },
        }
        Ok(())
    }

    fn finish(self, provider: CliProvider, status: std::process::ExitStatus) -> Result<String> {
        if let Some(error) = self.error {
            return Err(eyre!("{error}"));
        }
        match self.text {
            Some(text) => Ok(text),
            None if status.success() => Err(eyre!("{} returned no rewrite", provider.label())),
            // stderr can echo the prompt, so it never reaches diagnostics.
            None => Err(eyre!("{} exited with {status}", provider.label())),
        }
    }
}

fn claude_error(message: &str) -> String {
    if message.contains("Not logged in") || message.contains("/login") {
        format!("Claude Code is not signed in. {CLAUDE_SIGN_IN}")
    } else {
        format!("Claude Code failed: {}", bounded(message))
    }
}

fn codex_error(message: &str) -> String {
    if message.contains("401") || message.contains("Unauthorized") {
        format!("Codex is not signed in. {CODEX_SIGN_IN}")
    } else {
        format!("Codex failed: {}", bounded(message))
    }
}

fn bounded(message: &str) -> String {
    let line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= MAX_ERROR_CHARS {
        line
    } else {
        let mut bounded: String = line.chars().take(MAX_ERROR_CHARS).collect();
        bounded.push('…');
        bounded
    }
}

/// How `codex login status` describes the active sign-in.
#[derive(Debug, Eq, PartialEq)]
enum CodexSignIn {
    ChatGpt,
    ApiKey,
    SignedOut,
}

fn codex_sign_in(output: &str) -> CodexSignIn {
    if output.contains("ChatGPT") {
        CodexSignIn::ChatGpt
    } else if output.contains("API key") {
        CodexSignIn::ApiKey
    } else {
        CodexSignIn::SignedOut
    }
}

fn codex_sign_in_error(sign_in: CodexSignIn) -> Option<String> {
    match sign_in {
        CodexSignIn::ChatGpt => None,
        CodexSignIn::ApiKey => Some(format!(
            "Codex is signed in with an API key. Hex only uses a ChatGPT sign-in so rewriting never bills an API key. {CODEX_SIGN_IN}"
        )),
        CodexSignIn::SignedOut => Some(format!("Codex is not signed in. {CODEX_SIGN_IN}")),
    }
}

/// Installation and sign-in state for Settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CliStatus {
    Missing,
    Ready(String),
    /// Installed, but rewriting cannot run; the message says how to fix it.
    Unavailable(String),
}

pub fn status(provider: CliProvider) -> CliStatus {
    let Some(executable) = executable(provider) else {
        return CliStatus::Missing;
    };
    status_with(provider, &executable)
}

fn status_with(provider: CliProvider, executable: &Path) -> CliStatus {
    let args: &[&str] = match provider {
        CliProvider::Codex => &["login", "status"],
        CliProvider::Claude => &["auth", "status"],
    };
    let output = match run::capture(
        provider,
        executable,
        args,
        &std::env::temp_dir(),
        STATUS_DEADLINE,
        &AtomicBool::new(false),
    ) {
        Ok(output) => output,
        Err(error) => return CliStatus::Unavailable(error.to_string()),
    };
    match provider {
        CliProvider::Codex => match codex_sign_in_error(codex_sign_in(&output)) {
            None => CliStatus::Ready("Signed in with ChatGPT".into()),
            Some(error) => CliStatus::Unavailable(error),
        },
        CliProvider::Claude => claude_status(&output),
    }
}

fn claude_status(output: &str) -> CliStatus {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct AuthStatus {
        logged_in: bool,
        #[serde(default)]
        auth_method: String,
        #[serde(default)]
        subscription_type: Option<String>,
    }
    let Ok(status) = serde_json::from_str::<AuthStatus>(output.trim()) else {
        return CliStatus::Unavailable("Claude Code reported an unreadable sign-in status".into());
    };
    if !status.logged_in {
        return CliStatus::Unavailable(format!("Claude Code is not signed in. {CLAUDE_SIGN_IN}"));
    }
    if status.auth_method.to_ascii_lowercase().contains("api") {
        return CliStatus::Unavailable(format!(
            "Claude Code is signed in with an API key. Hex only uses a Claude subscription so rewriting never bills an API key. {CLAUDE_SIGN_IN}"
        ));
    }
    let plan = status
        .subscription_type
        .filter(|plan| !plan.is_empty())
        .map(|plan| {
            let mut chars = plan.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        });
    CliStatus::Ready(match plan {
        Some(plan) => format!("Signed in with Claude {plan}"),
        None => "Signed in with Claude".into(),
    })
}

#[cfg(not(target_os = "windows"))]
fn rewrite_with(
    provider: CliProvider,
    executable: &Path,
    workspace: &Path,
    model: Option<&str>,
    prompt: &RewritePrompt,
    deadline: Duration,
    cancelled: &AtomicBool,
) -> Result<CliRewrite> {
    let started = Instant::now();
    let model = validated_model(model)?;
    if provider == CliProvider::Codex {
        // Codex reports an API-key sign-in only here; refuse it before any request.
        let output = run::capture(
            provider,
            executable,
            &["login", "status"],
            workspace,
            deadline,
            cancelled,
        )?;
        if let Some(error) = codex_sign_in_error(codex_sign_in(&output)) {
            return Err(eyre!("{error}"));
        }
    }
    let mut parser = EventParser::default();
    let finished = run::stream(
        provider,
        run::command(
            provider,
            executable,
            &arguments(provider, model, workspace),
            workspace,
        ),
        input(provider, prompt).into_bytes(),
        deadline.saturating_sub(started.elapsed()),
        cancelled,
        |line| parser.observe(provider, line),
    )?;
    let startup = finished
        .first_output
        .map(|first| first.saturating_duration_since(started));
    let text = parser.finish(provider, finished.status)?;
    Ok(CliRewrite { text, startup })
}

#[cfg(target_os = "windows")]
fn rewrite_with(
    provider: CliProvider,
    _executable: &Path,
    _workspace: &Path,
    _model: Option<&str>,
    _prompt: &RewritePrompt,
    _deadline: Duration,
    _cancelled: &AtomicBool,
) -> Result<CliRewrite> {
    Err(eyre!(
        "{} rewriting is available on macOS and Linux",
        provider.label()
    ))
}

#[cfg(not(target_os = "windows"))]
mod run {
    use std::ffi::OsString;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, ExitStatus, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::thread;
    use std::time::{Duration, Instant};

    use color_eyre::eyre::{Result, WrapErr, eyre};

    use super::{CLAUDE_REMOVED_ENV, CODEX_REMOVED_ENV, CliProvider, MAX_OUTPUT_BYTES};

    pub(super) struct Finished {
        pub status: ExitStatus,
        pub first_output: Option<Instant>,
        /// Bounded stderr. Only status commands read it: during a rewrite
        /// the CLIs echo the prompt there.
        pub stderr: Vec<u8>,
    }

    const MAX_STDERR_BYTES: u64 = 64 * 1024;

    enum Output {
        Line(String),
        TooLarge,
        End,
    }

    /// A direct child of the CLI with a GUI-safe PATH and no API-key
    /// variables. Arguments are never interpreted by a shell.
    pub(super) fn command(
        provider: CliProvider,
        executable: &Path,
        args: &[OsString],
        workspace: &Path,
    ) -> Command {
        let mut command = Command::new(executable);
        command
            .args(args)
            .current_dir(workspace)
            .env("PATH", child_path(executable));
        let removed = match provider {
            CliProvider::Codex => CODEX_REMOVED_ENV,
            CliProvider::Claude => CLAUDE_REMOVED_ENV,
        };
        for variable in removed {
            command.env_remove(variable);
        }
        command.env("NO_COLOR", "1");
        if provider == CliProvider::Claude {
            command
                .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
                .env("DISABLE_AUTOUPDATER", "1");
        }
        command
    }

    /// The executable's own directory first, so an npm-installed script
    /// finds the `node` it was installed with, then Homebrew and system paths
    /// a Finder launch does not inherit.
    fn child_path(executable: &Path) -> OsString {
        let mut directories = Vec::new();
        directories.extend(executable.parent().map(Path::to_path_buf));
        if let Ok(resolved) = executable.canonicalize() {
            directories.extend(resolved.parent().map(Path::to_path_buf));
        }
        if let Some(path) = std::env::var_os("PATH") {
            directories.extend(std::env::split_paths(&path));
        }
        directories.extend(
            ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"]
                .into_iter()
                .map(Into::into),
        );
        let mut unique = Vec::new();
        for directory in directories {
            if !unique.contains(&directory) {
                unique.push(directory);
            }
        }
        std::env::join_paths(unique).unwrap_or_default()
    }

    /// Runs a short status command and returns its stdout and stderr text.
    pub(super) fn capture(
        provider: CliProvider,
        executable: &Path,
        args: &[&str],
        workspace: &Path,
        deadline: Duration,
        cancelled: &AtomicBool,
    ) -> Result<String> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let mut stdout = String::new();
        let finished = stream(
            provider,
            command(provider, executable, &args, workspace),
            Vec::new(),
            deadline,
            cancelled,
            |line| {
                stdout.push_str(line);
                stdout.push('\n');
                Ok(())
            },
        )?;
        stdout.push_str(&String::from_utf8_lossy(&finished.stderr));
        Ok(stdout)
    }

    /// Runs the child to completion, handing each stdout line to `observe` as
    /// it arrives. Cancellation, the deadline, oversized output, or an
    /// `observe` error kill the whole process group.
    pub(super) fn stream(
        provider: CliProvider,
        mut command: Command,
        input: Vec<u8>,
        deadline: Duration,
        cancelled: &AtomicBool,
        mut observe: impl FnMut(&str) -> Result<()>,
    ) -> Result<Finished> {
        let label = provider.label();
        if cancelled.load(Ordering::Acquire) {
            return Err(eyre!("{label} rewrite was cancelled"));
        }
        if deadline.is_zero() {
            return Err(eyre!("{label} rewrite deadline expired"));
        }
        let mut child = command
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    eyre!("{label} is not installed")
                } else {
                    eyre!("could not start {label}: {error}")
                }
            })?;
        let mut stdin = child.stdin.take().expect("CLI stdin must be piped");
        let stdout = child.stdout.take().expect("CLI stdout must be piped");
        let mut stderr = child.stderr.take().expect("CLI stderr must be piped");
        let writer = thread::spawn(move || stdin.write_all(&input));
        let drain = thread::spawn(move || {
            let mut kept = Vec::new();
            let _ = Read::by_ref(&mut stderr)
                .take(MAX_STDERR_BYTES)
                .read_to_end(&mut kept);
            // Keep draining so a chatty child never blocks on a full pipe.
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            kept
        });
        let (sender, lines) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut total = 0;
            loop {
                let mut line = String::new();
                match Read::by_ref(&mut reader)
                    .take((MAX_OUTPUT_BYTES - total) as u64 + 1)
                    .read_line(&mut line)
                {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        total += read;
                        if total > MAX_OUTPUT_BYTES {
                            let _ = sender.send(Output::TooLarge);
                            return;
                        }
                        if sender.send(Output::Line(line)).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = sender.send(Output::End);
        });
        let started = Instant::now();
        let mut first_output = None;
        let mut ended = false;
        let outcome: Result<ExitStatus> = loop {
            if cancelled.load(Ordering::Acquire) {
                break Err(eyre!("{label} rewrite was cancelled"));
            }
            if started.elapsed() >= deadline {
                break Err(eyre!(
                    "{label} rewrite exceeded {} seconds",
                    deadline.as_secs_f32()
                ));
            }
            if ended {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) => thread::sleep(Duration::from_millis(5)),
                    Err(error) => {
                        break Err(error).wrap_err_with(|| format!("could not inspect {label}"));
                    }
                }
                continue;
            }
            match lines.recv_timeout(Duration::from_millis(20)) {
                Ok(Output::Line(line)) => {
                    first_output.get_or_insert_with(Instant::now);
                    if let Err(error) = observe(line.trim_end()) {
                        break Err(error);
                    }
                }
                Ok(Output::TooLarge) => {
                    break Err(eyre!("{label} output exceeded {MAX_OUTPUT_BYTES} bytes"));
                }
                Ok(Output::End) | Err(RecvTimeoutError::Disconnected) => ended = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
        };
        // Leftover helpers die with the group, so pipes close and the
        // reader, drain, and writer threads all finish promptly.
        crate::dictation_processor::terminate_process_group(&mut child);
        drop(lines);
        let _ = reader.join();
        let stderr = drain.join().unwrap_or_default();
        let written = writer.join();
        let status = outcome?;
        if status.success()
            && let Ok(Err(error)) = written
        {
            return Err(error).wrap_err_with(|| format!("could not send the prompt to {label}"));
        }
        Ok(Finished {
            status,
            first_output,
            stderr,
        })
    }
}

#[cfg(target_os = "windows")]
mod run {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use color_eyre::eyre::{Result, eyre};

    use super::CliProvider;

    pub(super) fn capture(
        provider: CliProvider,
        _executable: &Path,
        _args: &[&str],
        _workspace: &Path,
        _deadline: Duration,
        _cancelled: &AtomicBool,
    ) -> Result<String> {
        Err(eyre!(
            "{} rewriting is available on macOS and Linux",
            provider.label()
        ))
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::context::ContextSnapshot;

    struct Fixture {
        directory: PathBuf,
        executable: PathBuf,
        workspace: PathBuf,
    }

    impl Fixture {
        /// A fake CLI: `body` runs as `/bin/sh` with the prompt on stdin.
        fn new(name: &str, body: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let directory = std::env::temp_dir().join(format!(
                "hex-cli-rewrite-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&directory);
            let workspace = directory.join("workspace");
            fs::create_dir_all(&workspace).unwrap();
            let executable = directory.join(name);
            fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            Self {
                directory,
                executable,
                workspace,
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.directory.join(name)
        }

        fn run(&self, provider: CliProvider, prompt: &RewritePrompt) -> Result<CliRewrite> {
            self.run_with(
                provider,
                None,
                prompt,
                Duration::from_secs(10),
                &AtomicBool::new(false),
            )
        }

        fn run_with(
            &self,
            provider: CliProvider,
            model: Option<&str>,
            prompt: &RewritePrompt,
            deadline: Duration,
            cancelled: &AtomicBool,
        ) -> Result<CliRewrite> {
            rewrite_with(
                provider,
                &self.executable,
                &self.workspace,
                model,
                prompt,
                deadline,
                cancelled,
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn prompt(transcript: &str) -> RewritePrompt {
        RewritePrompt::new(
            "Keep it short.",
            transcript,
            &ContextSnapshot {
                application: Some("Slack".into()),
                ..ContextSnapshot::default()
            },
            false,
        )
    }

    const CODEX_SIGNED_IN: &str =
        "if [ \"$1\" = login ]; then echo 'Logged in using ChatGPT'; exit 0; fi";

    fn codex(name: &str, body: &str) -> Fixture {
        Fixture::new(name, &format!("{CODEX_SIGNED_IN}\n{body}"))
    }

    const CLAUDE_INIT: &str = r#"echo '{"type":"system","subtype":"init","apiKeySource":"none"}'"#;

    fn claude_result(text: &str) -> String {
        let event = serde_json::json!({"type": "result", "is_error": false, "result": text});
        format!(
            "{CLAUDE_INIT}\necho '{}'",
            event.to_string().replace('\'', "'\\''")
        )
    }

    #[test]
    fn claude_receives_the_prompt_on_stdin_and_safe_arguments() {
        let fixture = Fixture::new("claude", "");
        let args_file = fixture.path("args");
        let stdin_file = fixture.path("stdin");
        fs::write(
            &fixture.executable,
            format!(
                "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\"; done > '{}'\ncat > '{}'\nenv > '{}'\n{}\n",
                args_file.display(),
                stdin_file.display(),
                fixture.path("env").display(),
                claude_result("Send the file tomorrow.")
            ),
        )
        .unwrap();
        let transcript = "um $(rm -rf ~) `whoami` ; send \"the\" file\n日本語 🎉 tomorrow";
        let prompt = prompt(transcript);

        let rewrite = fixture
            .run_with(
                CliProvider::Claude,
                Some("haiku"),
                &prompt,
                Duration::from_secs(10),
                &AtomicBool::new(false),
            )
            .unwrap();

        assert_eq!(rewrite.text, "Send the file tomorrow.");
        assert!(rewrite.startup.is_some());
        assert_eq!(fs::read_to_string(&stdin_file).unwrap(), prompt.user);
        let args = fs::read_to_string(&args_file).unwrap();
        assert!(
            !args.contains("rm -rf"),
            "the transcript never reaches argv"
        );
        for expected in [
            "--print",
            "--no-session-persistence",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--model=haiku",
        ] {
            assert!(
                args.lines().any(|line| line == expected),
                "{expected}: {args}"
            );
        }
        let env = fs::read_to_string(fixture.path("env")).unwrap();
        assert!(!env.contains("ANTHROPIC_API_KEY="));
    }

    #[test]
    fn codex_runs_ephemerally_in_the_workspace_and_returns_the_last_message() {
        let fixture = codex("codex", "");
        let args_file = fixture.path("args");
        let stdin_file = fixture.path("stdin");
        let pwd_file = fixture.path("pwd");
        fs::write(
            &fixture.executable,
            format!(
                "#!/bin/sh\n{CODEX_SIGNED_IN}\nprintf '%s\\n' \"$@\" > '{}'\ncat > '{}'\npwd > '{}'\n{}\n",
                args_file.display(),
                stdin_file.display(),
                pwd_file.display(),
                [
                    r#"echo '{"type":"thread.started","thread_id":"t"}'"#,
                    r#"echo 'not json'"#,
                    r#"echo '{"type":"item.completed","item":{"type":"reasoning","text":"thinking"}}'"#,
                    r#"echo '{"type":"item.completed","item":{"type":"agent_message","text":"draft"}}'"#,
                    r#"printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"Line one.\nLine two."}}'"#,
                    r#"echo '{"type":"turn.completed"}'"#,
                ]
                .join("\n")
            ),
        )
        .unwrap();
        let prompt = prompt("line one\nline two");

        let rewrite = fixture
            .run_with(
                CliProvider::Codex,
                Some("gpt-6-luna"),
                &prompt,
                Duration::from_secs(10),
                &AtomicBool::new(false),
            )
            .unwrap();

        assert_eq!(rewrite.text, "Line one.\nLine two.");
        assert_eq!(fs::read_to_string(&stdin_file).unwrap(), prompt.combined());
        let args = fs::read_to_string(&args_file).unwrap();
        for expected in [
            "exec",
            "--json",
            "--ephemeral",
            "--ignore-user-config",
            "read-only",
            "--model=gpt-6-luna",
        ] {
            assert!(
                args.lines().any(|line| line == expected),
                "{expected}: {args}"
            );
        }
        assert_eq!(args.lines().last(), Some("-"));
        let pwd = fs::read_to_string(&pwd_file).unwrap();
        assert_eq!(
            Path::new(pwd.trim()).canonicalize().unwrap(),
            fixture.workspace.canonicalize().unwrap()
        );
    }

    #[test]
    fn long_inputs_stream_through_stdin_without_blocking() {
        let fixture = Fixture::new(
            "claude",
            &format!("wc -c > /dev/null\n{}", claude_result("ok")),
        );
        let transcript = "word ".repeat(200_000);
        let rewrite = fixture
            .run(CliProvider::Claude, &prompt(&transcript))
            .unwrap();
        assert_eq!(rewrite.text, "ok");
    }

    #[test]
    fn sign_in_failures_are_actionable() {
        let claude = Fixture::new(
            "claude",
            &format!(
                "cat > /dev/null\n{CLAUDE_INIT}\necho '{}'\nexit 1",
                r#"{"type":"result","is_error":true,"result":"Not logged in · Please run /login"}"#
            ),
        );
        let error = claude.run(CliProvider::Claude, &prompt("hi")).unwrap_err();
        assert!(
            error.to_string().contains("Claude Code is not signed in"),
            "{error}"
        );

        let codex = Fixture::new(
            "codex",
            "if [ \"$1\" = login ]; then echo 'Not logged in' >&2; exit 1; fi\necho should-not-run",
        );
        let error = codex.run(CliProvider::Codex, &prompt("hi")).unwrap_err();
        assert!(error.to_string().contains("codex login"), "{error}");

        let failed_turn = codex_fixture_events(&[
            r#"{"type":"error","message":"unexpected status 401 Unauthorized"}"#,
            r#"{"type":"turn.failed","error":{"message":"unexpected status 401 Unauthorized"}}"#,
        ]);
        let error = failed_turn
            .run(CliProvider::Codex, &prompt("hi"))
            .unwrap_err();
        assert!(
            error.to_string().contains("Codex is not signed in"),
            "{error}"
        );
    }

    fn codex_fixture_events(events: &[&str]) -> Fixture {
        let lines = events
            .iter()
            .map(|event| format!("echo '{event}'"))
            .collect::<Vec<_>>()
            .join("\n");
        codex("codex", &format!("cat > /dev/null\n{lines}\nexit 1"))
    }

    #[test]
    fn api_key_sessions_are_refused_rather_than_billed() {
        let codex = Fixture::new(
            "codex",
            "if [ \"$1\" = login ]; then echo 'Logged in using an API key - sk-***'; exit 0; fi\ntouch ran",
        );
        let error = codex.run(CliProvider::Codex, &prompt("hi")).unwrap_err();
        assert!(error.to_string().contains("API key"), "{error}");
        assert!(!codex.workspace.join("ran").exists());

        let marker = std::env::temp_dir().join(format!("hex-claude-billed-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let claude = Fixture::new(
            "claude",
            &format!(
                "cat > /dev/null\necho '{}'\nsleep 4\ntouch '{}'\n{}",
                r#"{"type":"system","subtype":"init","apiKeySource":"ANTHROPIC_API_KEY"}"#,
                marker.display(),
                claude_result("billed")
            ),
        );
        let started = Instant::now();
        let error = claude.run(CliProvider::Claude, &prompt("hi")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("instead of your Claude subscription"),
            "{error}"
        );
        thread_sleep(
            Duration::from_secs(4).saturating_sub(started.elapsed()) + Duration::from_millis(500),
        );
        assert!(!marker.exists(), "the whole process group was killed");
    }

    fn thread_sleep(duration: Duration) {
        std::thread::sleep(duration);
    }

    #[test]
    fn codex_tool_use_is_stopped() {
        let fixture = codex_fixture_events(&[
            r#"{"type":"thread.started"}"#,
            r#"{"type":"item.started","item":{"type":"command_execution","command":"ls"}}"#,
        ]);
        let error = fixture
            .run(CliProvider::Codex, &prompt("list files"))
            .unwrap_err();
        assert!(error.to_string().contains("tried to use a tool"), "{error}");
    }

    #[test]
    fn failures_and_empty_output_fall_back_without_echoing_stderr() {
        let crashed = Fixture::new(
            "claude",
            "cat > /dev/null\necho 'secret transcript echoed' >&2\nexit 3",
        );
        let error = crashed
            .run(CliProvider::Claude, &prompt("secret transcript"))
            .unwrap_err();
        assert!(error.to_string().contains("exited with"), "{error}");
        assert!(!error.to_string().contains("secret"), "{error}");

        let silent = Fixture::new("claude", "cat > /dev/null\nexit 0");
        let error = silent.run(CliProvider::Claude, &prompt("hi")).unwrap_err();
        assert!(error.to_string().contains("returned no rewrite"), "{error}");

        let empty = Fixture::new(
            "claude",
            &format!("cat > /dev/null\n{}", claude_result("  ")),
        );
        let (text, observation) = rewrite_observed_with(&empty, CliProvider::Claude);
        assert_eq!(text, None);
        assert_eq!(
            observation.fallback.as_deref(),
            Some("processor returned empty text")
        );
    }

    fn rewrite_observed_with(
        fixture: &Fixture,
        provider: CliProvider,
    ) -> (Option<String>, ProcessingObservation) {
        // Mirrors `rewrite_observed` with an injected executable.
        let result = fixture.run(provider, &prompt("hi"));
        let text = result
            .as_ref()
            .ok()
            .map(|rewrite| rewrite.text.trim().to_owned())
            .filter(|text| !text.is_empty());
        let fallback = match &result {
            Ok(rewrite) if rewrite.text.trim().is_empty() => {
                Some("processor returned empty text".to_owned())
            }
            Ok(_) => None,
            Err(error) => Some(error.to_string()),
        };
        (
            text,
            ProcessingObservation {
                profile: "test".into(),
                latency_ms: 0,
                fallback,
                provider: Some(provider.label().into()),
                startup_ms: None,
            },
        )
    }

    #[test]
    fn slow_rewrites_time_out_and_cancellation_stops_promptly() {
        let slow = Fixture::new(
            "claude",
            &format!("cat > /dev/null\n{CLAUDE_INIT}\nsleep 30"),
        );
        let started = Instant::now();
        let error = slow
            .run_with(
                CliProvider::Claude,
                None,
                &prompt("hi"),
                Duration::from_millis(300),
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert!(error.to_string().contains("exceeded"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));

        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = cancelled.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            cancel.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let error = slow
            .run_with(
                CliProvider::Claude,
                None,
                &prompt("hi"),
                Duration::from_secs(30),
                &cancelled,
            )
            .unwrap_err();
        canceller.join().unwrap();
        assert!(error.to_string().contains("cancelled"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));

        let already = AtomicBool::new(true);
        let error = slow
            .run_with(
                CliProvider::Claude,
                None,
                &prompt("hi"),
                Duration::from_secs(5),
                &already,
            )
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"), "{error}");
    }

    #[test]
    fn concurrent_rewrites_keep_their_own_results() {
        let fixture = Fixture::new(
            "claude",
            &format!(
                "input=$(cat)\n{CLAUDE_INIT}\ncase \"$input\" in *first*) sleep 0.3; echo '{}';; *) echo '{}';; esac",
                r#"{"type":"result","is_error":false,"result":"First."}"#,
                r#"{"type":"result","is_error":false,"result":"Second."}"#,
            ),
        );
        let fixture = Arc::new(fixture);
        let handles: Vec<_> = ["first", "second"]
            .into_iter()
            .map(|word| {
                let fixture = fixture.clone();
                std::thread::spawn(move || {
                    fixture
                        .run(CliProvider::Claude, &prompt(word))
                        .unwrap()
                        .text
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results, ["First.", "Second."]);
    }

    #[test]
    fn model_names_cannot_become_flags() {
        assert_eq!(validated_model(Some(" haiku ")).unwrap(), Some("haiku"));
        assert_eq!(validated_model(Some("")).unwrap(), None);
        assert_eq!(
            validated_model(Some("claude-haiku-4-5@20251001")).unwrap(),
            Some("claude-haiku-4-5@20251001")
        );
        assert!(validated_model(Some("--dangerously-skip-permissions")).is_err());
        assert!(validated_model(Some("haiku; rm -rf /")).is_err());
        let args = arguments(CliProvider::Claude, Some("sonnet"), Path::new("/tmp"));
        assert!(args.contains(&OsString::from("--model=sonnet")));
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions")
        );
    }

    #[test]
    fn status_reports_install_and_subscription_sign_in() {
        let codex = Fixture::new(
            "codex",
            "if [ \"$1 $2\" = 'login status' ]; then echo 'Logged in using ChatGPT'; fi",
        );
        assert_eq!(
            status_with(CliProvider::Codex, &codex.executable),
            CliStatus::Ready("Signed in with ChatGPT".into())
        );
        let api_key = Fixture::new("codex", "echo 'Logged in using an API key - sk-***'");
        assert!(matches!(
            status_with(CliProvider::Codex, &api_key.executable),
            CliStatus::Unavailable(message) if message.contains("API key")
        ));
        assert_eq!(
            claude_status(r#"{"loggedIn":true,"authMethod":"claude.ai","subscriptionType":"max"}"#),
            CliStatus::Ready("Signed in with Claude Max".into())
        );
        assert!(matches!(
            claude_status(r#"{"loggedIn":false,"authMethod":"none"}"#),
            CliStatus::Unavailable(message) if message.contains("not signed in")
        ));
        assert!(matches!(
            claude_status(r#"{"loggedIn":true,"authMethod":"api_key"}"#),
            CliStatus::Unavailable(message) if message.contains("API key")
        ));
    }

    /// Live latency probe against the installed, signed-in CLIs:
    /// `cargo test --bin voice-control live_cli_rewrite -- --ignored --nocapture`.
    #[test]
    #[ignore = "uses the installed Codex and Claude CLIs and their network sign-ins"]
    fn live_cli_rewrite() {
        let transcript =
            "um so like can you uh send me the the updated config file tomorrow, no wait, today";
        let workspace = std::env::temp_dir().join("hex-live-cli-rewrite");
        fs::create_dir_all(&workspace).unwrap();
        for (provider, model) in [
            (CliProvider::Claude, Some("haiku")),
            (CliProvider::Codex, Some("gpt-6-luna")),
        ] {
            let Some(executable) = executable(provider) else {
                eprintln!("{}: not installed", provider.label());
                continue;
            };
            eprintln!(
                "{}: {:?}",
                provider.label(),
                status_with(provider, &executable)
            );
            for _ in 0..3 {
                let started = Instant::now();
                let result = rewrite_with(
                    provider,
                    &executable,
                    &workspace,
                    model,
                    &prompt(transcript),
                    Duration::from_secs(30),
                    &AtomicBool::new(false),
                );
                eprintln!(
                    "{}: total {:?} startup {:?} -> {:?}",
                    provider.label(),
                    started.elapsed(),
                    result.as_ref().ok().and_then(|rewrite| rewrite.startup),
                    result.map(|rewrite| rewrite.text)
                );
            }
        }
    }

    #[test]
    fn long_errors_are_bounded_single_lines() {
        let message = format!("bad\nmodel {}", "x".repeat(1_000));
        let error = claude_error(&message);
        assert!(!error.contains('\n'));
        assert!(error.chars().count() < MAX_ERROR_CHARS + 40);
    }
}
