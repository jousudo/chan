//! The `cs` client surface: the clap subcommand tree (`ShellAction` /
//! `TerminalAction`) and the dispatch that turns each action into a
//! control-socket round-trip. Lifted verbatim out of the `chan` binary so
//! `chan-desktop` can drive the same `cs` commands without the `chan`
//! binary on PATH.
//!
//! RISK: the clap derive here is wire-load-bearing. Every flag name,
//! `infer_subcommands`, and arg shape is part of the `cs` contract; a
//! drift breaks commands at runtime with a green build. Wire-smoke every
//! `cs` command after touching this file, not just `cargo build`.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use clap::{Parser, Subcommand};

use crate::control::{absolutize, control_socket_env, open_env, send_control_request};
use crate::submit::{submit_writes, SubmitAgent};
use crate::wire::{
    ControlRequest, PaneOp, PastePrefer, SplitDir, SurveyFollowup, SurveySpec, TeamOp,
    MAX_CLIPBOARD_BYTES,
};

/// Top-level `cs` parser. The `chan` binary reaches `cs` through its own
/// `Cli` (rewriting `cs ...` into `chan shell ...` in `parse_cli`), but
/// `chan-desktop` has no `chan` binary, so it parses `cs` argv directly
/// through this. `infer_subcommands` mirrors the `chan shell` command so
/// `cs t l` / `cs o` resolve the same way under both front ends.
#[derive(Parser, Debug)]
#[command(
    name = "cs",
    about = "Drive the current chan window from its terminal."
)]
#[command(infer_subcommands = true)]
struct CsCli {
    #[command(subcommand)]
    action: ShellAction,
}

/// Parse a full `cs` argv (`argv[0]` included) and dispatch it. The entry
/// `chan-desktop` calls when invoked through a `cs` name, so desktop users
/// get the `cs` client without a `chan` binary on PATH. clap prints help /
/// usage and exits on a parse error or `--help`, exactly like the `chan`
/// binary's `Cli::parse_from`.
pub async fn run_cs<I>(args: I) -> Result<()>
where
    I: IntoIterator,
    I::Item: Into<std::ffi::OsString> + Clone,
{
    dispatch(CsCli::parse_from(args).action).await
}

#[derive(Subcommand, Debug)]
pub enum ShellAction {
    /// Open a path in the current window. Without a path, opens the
    /// terminal's current directory in the browser.
    Open {
        #[arg(value_hint = clap::ValueHint::AnyPath)]
        path: Option<PathBuf>,
    },
    /// Open the documentation graph in the current window. With a path,
    /// focuses the graph on that file or directory.
    Graph {
        #[arg(value_hint = clap::ValueHint::AnyPath)]
        path: Option<PathBuf>,
    },
    /// Open a Dashboard tab in the current window.
    Dashboard {
        /// Initial carousel slide index (0-based). Out-of-range values
        /// land on the default slide.
        #[arg(long = "carousel-index")]
        carousel_index: Option<u32>,
        /// Open with carousel auto-rotation OFF (the new tab's
        /// `autoRotate` is false). Default leaves rotation on. Spelled
        /// one-r to match `--carousel-index`.
        #[arg(long = "carousel-off")]
        carousel_off: bool,
    },
    /// Upload files into the current window, raising the SAME upload UI as the
    /// Inspector pill (a file picker, then a progress indicator). PATH is
    /// required and names the target directory (`.` = the current directory):
    /// with a directory, files land there; with a file, they land in its
    /// parent. In a workspace window the target is workspace-relative and must
    /// stay within the workspace; in a standalone terminal it is the terminal's
    /// cwd (the shell's own reach).
    Upload {
        #[arg(value_hint = clap::ValueHint::AnyPath)]
        path: PathBuf,
    },
    /// Download a file or directory through the current window, reusing the
    /// Inspector's download-with-progress UI (a directory downloads as a tar,
    /// streamed on the fly). PATH is required (`.` = the current directory). In
    /// a workspace window the source is workspace-relative and must stay within
    /// the workspace; in a standalone terminal it resolves against the
    /// terminal's cwd (the shell's own reach).
    Download {
        #[arg(value_hint = clap::ValueHint::AnyPath)]
        path: PathBuf,
    },
    /// Copy stdin onto the window's clipboard, so a `Cmd+V` in another app
    /// pastes it. Reads all of stdin (text, HTML, or an image) and sends it to
    /// the browser / desktop clipboard. The content type is sniffed (plain
    /// text, an `<html>`/`<!doctype html>` document, or a PNG/JPEG/GIF/WebP
    /// image); non-PNG images are re-encoded to PNG, which is the format the
    /// clipboard reliably accepts. `--html` forces the input to be treated as
    /// HTML (a fragment that would not sniff as a document); `--mime` forces
    /// any type. Example: `cs copy < photo.png`, then paste into Gmail.
    Copy {
        /// Force the clipboard MIME instead of sniffing stdin
        /// (e.g. `text/html`, `image/png`).
        #[arg(long)]
        mime: Option<String>,
        /// Treat stdin as HTML (shorthand for `--mime text/html`). Use for an
        /// HTML fragment that would not sniff as a full document.
        #[arg(long, conflicts_with = "mime")]
        html: bool,
    },
    /// Paste the window's clipboard to stdout. Writes the raw bytes, so
    /// `cs paste > file.png` yields a real PNG and a bare `cs paste` prints
    /// clipboard text. When the clipboard holds several representations the
    /// default is image-first, then text; `--text` / `--html` / `--image`
    /// force one. The emitted MIME is reported on stderr. The bytes are raw:
    /// clipboard text may carry control/escape sequences, so redirect to a
    /// file (or pipe through a sanitizer) rather than dumping to a live TTY.
    Paste {
        /// Emit the plain-text representation only.
        #[arg(long, conflicts_with_all = ["html", "image"])]
        text: bool,
        /// Emit the HTML (rich text) representation.
        #[arg(long, conflicts_with_all = ["text", "image"])]
        html: bool,
        /// Emit the image representation (PNG).
        #[arg(long, conflicts_with_all = ["text", "html"])]
        image: bool,
    },
    /// Terminal operations against the current window's live sessions.
    ///
    /// Prefix matching applies here too: `cs t n` / `cs t w` / `cs t l`
    /// resolve to terminal new / write / list.
    #[command(infer_subcommands = true)]
    Terminal {
        #[command(subcommand)]
        action: TerminalAction,
    },
    /// Run the same content search the UI does, against the running
    /// window's workspace. Prints a markdown table by default; `--json`
    /// emits compact machine output and `--json --pretty` indents it.
    Search {
        /// Query string. Multiple words are joined with spaces.
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// Maximum number of result rows (one per file). Default 20.
        #[arg(long)]
        limit: Option<u32>,
        /// Emit JSON instead of the markdown table. Compact by default.
        #[arg(long)]
        json: bool,
        /// With --json, pretty-print (indent) the JSON. Ignored without
        /// --json.
        #[arg(long)]
        pretty: bool,
    },
    /// Window registry operations. `cs window list` (or `cs w l`) shows the
    /// library's authoritative window set — every window across every tenant,
    /// with its `connected` flag (a live event socket is tagged with it right
    /// now, including windows chan-desktop has hidden via the close button).
    #[command(infer_subcommands = true)]
    Window {
        #[command(subcommand)]
        action: WindowAction,
    },
    /// Manage this session's leader and followers. `cs session list` shows the
    /// participants and the leader; `self --name=` renames you; `handover`
    /// requests (or, as leader, answers) a handover; `takeover` claims
    /// leadership when the leader is gone (`--force` seizes a live one).
    #[command(infer_subcommands = true)]
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Inspect or drive a window's tab/pane layout. Bare `cs pane` reports
    /// the layout (every pane, its tabs and which is selected); the
    /// subcommands focus a pane, split it right|bottom, resize it, or close a
    /// tab / pane / everything. Targets the caller's own window
    /// ($CHAN_WINDOW_ID) by default, or any window via `--tab-name` (a tab it
    /// owns) so it works from a context with no window. Markdown by default;
    /// `--json [--pretty]` for machine output.
    #[command(infer_subcommands = true)]
    Pane {
        /// Target the window owning this tab, instead of the caller's own
        /// window. Lets `cs pane` run without a $CHAN_WINDOW_ID.
        #[arg(long = "tab-name", global = true)]
        tab_name: Option<String>,
        /// Emit JSON instead of the markdown rendering (layout or exec
        /// result). Compact by default.
        #[arg(long, global = true)]
        json: bool,
        /// Indent the JSON output. Only meaningful with `--json`.
        #[arg(long, global = true)]
        pretty: bool,
        /// A layout mutation. Omit for the read-only layout report.
        #[command(subcommand)]
        action: Option<PaneAction>,
    },
}

/// `cs window <action>`: read the window registry and drive the
/// desktop's OS windows. The lifecycle verbs (`new`/`open`/`rm`/`hide`)
/// need the chan desktop app; a standalone `chan open` refuses
/// them. `new` derives its kind from the calling tenant; the id-bearing
/// verbs act on any window by id. Titles are library-owned and
/// auto-derived; there is no rename verb.
#[derive(Subcommand, Debug)]
pub enum WindowAction {
    /// List the windows chan knows about (connected and/or with a
    /// saved layout). Markdown by default; `--json [--pretty]` for
    /// machine output.
    List {
        /// Emit the raw JSON rows instead of the markdown table.
        /// Compact by default.
        #[arg(long)]
        json: bool,
        /// With --json, pretty-print (indent) the JSON. Ignored
        /// without --json.
        #[arg(long)]
        pretty: bool,
    },
    /// Open a new desktop window. From a standalone terminal this spawns
    /// another terminal window; from a workspace it spawns another window
    /// of that workspace. Prints the new window id.
    New,
    /// Focus a window by id (un-hiding it if it was hidden). Best-effort
    /// reopens a closed-but-saved workspace window when its workspace is
    /// still running.
    Open {
        /// The window id (see `cs window list`).
        id: String,
    },
    /// Remove a window by id: destroy it (unlike the close button, which
    /// hides it) and delete its saved layout. Prompts before killing a
    /// window with live terminals; `--force` skips the prompt.
    Rm {
        /// The window id (see `cs window list`).
        id: String,
        /// Destroy even with live terminal shells, without prompting.
        #[arg(long)]
        force: bool,
    },
    /// Hide a window by id (the OS close-button behavior): keep its
    /// terminals and layout warm and reopenable.
    Hide {
        /// The window id (see `cs window list`).
        id: String,
    },
}

/// `cs session <action>`: manage the session's leader and followers over the
/// control socket. `list` is socket-only; `self`/`handover`/`takeover` carry
/// the caller's own window id ($CHAN_WINDOW_ID) so the server knows which
/// participant is acting.
#[derive(Subcommand, Debug)]
pub enum SessionAction {
    /// List the session participants, the leader, and each one's status.
    /// Markdown by default; `--json [--pretty]` for machine output.
    List {
        /// Emit the raw JSON rows instead of the markdown table.
        #[arg(long)]
        json: bool,
        /// With --json, pretty-print (indent) the JSON. Ignored without --json.
        #[arg(long)]
        pretty: bool,
    },
    /// Rename yourself in this session.
    #[command(name = "self")]
    SelfCmd {
        /// The new display name for this client.
        #[arg(long)]
        name: String,
    },
    /// Request a leader handover (default), or accept/reject a pending request
    /// when you are the leader.
    Handover {
        /// Window id to hand leadership to (default: you).
        #[arg(long)]
        to: Option<String>,
        /// Accept a pending handover request (leader only).
        #[arg(long)]
        accept: bool,
        /// Reject a pending handover request (leader only).
        #[arg(long)]
        reject: bool,
        /// Seconds to wait for the leader's answer.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Take over as leader (only when the leader is gone, unless --force).
    Takeover {
        /// Seize leadership even from a live leader.
        #[arg(long)]
        force: bool,
    },
}

/// `cs pane <action>`: the layout mutations, executed on the target window's
/// live SPA `layout`. Each maps 1:1 to a [`PaneOp`] sent in a
/// [`ControlRequest::PaneExec`].
#[derive(Subcommand, Debug)]
pub enum PaneAction {
    /// Focus (activate) a pane by id.
    Focus {
        /// The pane id to focus (from `cs pane`).
        pane_id: String,
    },
    /// Split a pane, placing a new empty pane to the `right` or `bottom`.
    Split {
        /// Where the new pane goes: `right` or `bottom`.
        dir: SplitDirArg,
        /// The pane to split (default: the active pane).
        #[arg(long = "pane")]
        pane: Option<String>,
    },
    /// Resize a pane's enclosing split by a ratio delta (e.g. `0.1`,
    /// `-0.1`); positive grows the pane. No-ops the sole pane.
    // allow_negative_numbers so a bare `-0.1` is the delta value, not parsed
    // as an (unknown) `-0` flag.
    #[command(allow_negative_numbers = true)]
    Resize {
        /// Ratio delta in -1.0..1.0.
        delta: f64,
        /// The pane to resize (default: the active pane).
        #[arg(long = "pane")]
        pane: Option<String>,
    },
    /// Close one tab (the pane's active tab by default).
    CloseTab {
        /// The pane to close a tab in (default: the active pane).
        #[arg(long = "pane")]
        pane: Option<String>,
        /// The tab id to close (default: the pane's active tab).
        #[arg(long = "tab")]
        tab: Option<String>,
        /// Close past a dirty file / live terminal.
        #[arg(long)]
        force: bool,
    },
    /// Close a whole pane (the active one by default).
    ClosePane {
        /// The pane id to close (default: the active pane).
        #[arg(long = "pane")]
        pane: Option<String>,
        /// Close past dirty files / live terminals.
        #[arg(long)]
        force: bool,
    },
    /// Close every tab in every pane.
    CloseAll {
        /// Close past dirty files / live terminals.
        #[arg(long)]
        force: bool,
    },
}

/// `right` | `bottom` for `cs pane split`, mapped to the wire [`SplitDir`].
/// Matches the hybrid pane hamburger's split options.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum SplitDirArg {
    Right,
    Bottom,
}

impl From<SplitDirArg> for SplitDir {
    fn from(dir: SplitDirArg) -> Self {
        match dir {
            SplitDirArg::Right => SplitDir::Right,
            SplitDirArg::Bottom => SplitDir::Bottom,
        }
    }
}

/// `--mcp-env on|off` for `cs terminal team new`: whether the team's terminals
/// get the chan MCP env vars (sets `TeamConfig.mcp_env`). Omitting the flag
/// leaves the field at its config / serde default (OFF).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum McpEnvToggle {
    On,
    Off,
}

impl McpEnvToggle {
    fn as_bool(self) -> bool {
        matches!(self, McpEnvToggle::On)
    }
}

impl PaneAction {
    /// Convert the parsed subcommand into the wire [`PaneOp`].
    fn into_op(self) -> PaneOp {
        match self {
            PaneAction::Focus { pane_id } => PaneOp::Focus { pane_id },
            PaneAction::Split { dir, pane } => PaneOp::Split {
                pane_id: pane,
                dir: dir.into(),
            },
            PaneAction::Resize { delta, pane } => PaneOp::Resize {
                pane_id: pane,
                delta,
            },
            PaneAction::CloseTab { pane, tab, force } => PaneOp::CloseTab {
                pane_id: pane,
                tab_id: tab,
                force,
            },
            PaneAction::ClosePane { pane, force } => PaneOp::ClosePane {
                pane_id: pane,
                force,
            },
            PaneAction::CloseAll { force } => PaneOp::CloseAll { force },
        }
    }
}

/// Worked examples appended to `cs terminal survey --help`. Each case
/// pairs the invocation with the JSON survey the SPA actually receives, so
/// an agent can see how the flags map onto the wire `SurveySpec`. Raw
/// string: the literal `\n` inside a body stays literal (it is what an
/// agent types), while the layout uses real line breaks.
const SURVEY_AFTER_HELP: &str = r#"EXAMPLES:
Each case shows the invocation and the JSON survey the SPA receives.
`surveyId` is empty from the CLI; the server mints it before the SPA sees
it. Every overlay shows the options PLUS [F] (follow up) and Dismiss, so
the blocking call prints one of: the chosen option label; the new followup
file path on [F] when `--followup-dir` context was passed (else a bare "host
deferred" line); or "survey dismissed" when the host drops it.

IMPORTANT: an [F] followup file is created EMPTY (the original question plus an
empty comments section). It means "deferred, not ready" -- NOT an actionable
answer. The host must WRITE their decision into the file's comments section
before an agent acts on it. An agent that gets a followup path should re-read
the file later and act ONLY once the host has populated it.

Single question, two options:
  cs terminal survey --tab-name @@Alice \
    --title "Merge order" --option "A first" --option "B first" \
    "Which patch lands first?"

  {
    "surveyId": "",
    "title": "Merge order",
    "bodyMarkdown": "Which patch lands first?",
    "options": ["A first", "B first"],
    "followup": null
  }

Four options, no title, multi-line body from stdin:
  printf 'Pick a slot:\n\n- morning\n- evening' \
    | cs terminal survey --tab-group leads --stdin \
        --option Mon --option Tue --option Wed --option Thu

  {
    "surveyId": "",
    "title": null,
    "bodyMarkdown": "Pick a slot:\n\n- morning\n- evening",
    "options": ["Mon", "Tue", "Wed", "Thu"],
    "followup": null
  }

With an [F] follow-up paper-trail file (from <- $CHAN_TAB_NAME, to <- the
survey target); passing --followup-dir is what makes [F] write the file:
  cs terminal survey --tab-name @@Host \
    --option "Ship it" --option "Hold" \
    --followup-dir teams/alpha \
    "Ready to cut v0.23.0?"

  {
    "surveyId": "",
    "title": null,
    "bodyMarkdown": "Ready to cut v0.23.0?",
    "options": ["Ship it", "Hold"],
    "followup": { "dir": "teams/alpha", "from": "@@Alice", "to": "@@Host" }
  }
"#;

/// Worked examples appended to `cs terminal team --help`. Shows the input
/// config.toml shape and the three flows (write, preview-as-script, load).
/// Raw string so the literal escapes inside the sample stay literal.
const TEAM_AFTER_HELP: &str = r#"EXAMPLES:
A team is one config.toml (the on-disk `{dir}/config.toml` shape). Members
are 1..=9, exactly one `is_lead = true`. The submit-encoding agent
(claude / codex / gemini) is DERIVED from each member's `command`: a loose
whole-word match, so `claude --resume` or `/usr/local/bin/codex-cli` resolve.
A command that matches none is a plain shell member (no submit chord). To
force the agent for an unorthodox launcher, set `CHAN_AGENT` in the member's
env (claude/codex/gemini, or none/shell to force a shell). `created_at` is
optional: the server stamps the current time when it is omitted.

  # myteam.toml
  team_name   = "alpha"
  host_name   = "Neo"
  host_handle = "@@Neo"
  tab_group   = "alpha"

  [[members]]
  handle  = "@@Lead"
  command = "claude"
  is_lead = true

  [[members]]
  handle  = "@@Alice"
  command = "codex"

  # A custom launcher the command can't reveal: name the agent explicitly.
  [[members]]
  handle  = "@@Bob"
  command = "./my-agent.sh"
  env     = { CHAN_AGENT = "gemini" }

Write the team (config.toml + the server-regenerated bootstrap.md + the
tasks/journals/followups tree) inside the workspace at `alpha/`:
  cs terminal team new alpha --config myteam.toml

Preview the WHOLE bootstrap as a runnable shell script (mutates nothing;
prints to stdout). Run it from a chan terminal at the workspace root to
spawn the team:
  cs terminal team new alpha --config myteam.toml --script

Pipe the config in instead of a file:
  cat myteam.toml | cs terminal team new alpha --stdin

Emit the bootstrap script for an already-written team:
  cs terminal team load alpha --script
"#;

#[derive(Subcommand, Debug)]
pub enum TerminalAction {
    /// Open a new terminal tab in the current window.
    New {
        /// Working directory for the new terminal (workspace-relative or
        /// absolute under the workspace root). Defaults to the workspace
        /// root.
        #[arg(value_hint = clap::ValueHint::AnyPath)]
        path: Option<PathBuf>,
        /// Tab name ($CHAN_TAB_NAME inside the new terminal).
        #[arg(long = "tab-name")]
        tab_name: Option<String>,
        /// Broadcast group ($CHAN_TAB_GROUP). Defaults to "default".
        #[arg(long = "tab-group")]
        tab_group: Option<String>,
    },
    /// Write raw bytes to live terminal session(s), selected by name
    /// and/or group. No newline is appended: `cs terminal write $'ls\n'`
    /// runs; `cs terminal write ls` only types. At least one selector is
    /// required.
    ///
    /// The write is QUEUED per target, not delivered instantly: each
    /// session's queue drains one message at a time, delivering the next only
    /// when that agent has finished generating (its output has gone idle), so
    /// chained writes submit one after another instead of stacking into one
    /// compose. The command returns the queue position. NOTE: "idle" is
    /// detected from output quiescence, so a target sitting at its prompt
    /// with a PAUSED, half-typed buffer reads as idle; that rare case is not
    /// detected. Queue bound: 100 per target; dropped when the session is
    /// recycled (restarted).
    Write {
        /// Literal bytes to write. Omit with --stdin to stream instead.
        cmd: Option<String>,
        /// Read the bytes from this process's stdin instead of `cmd`.
        #[arg(long)]
        stdin: bool,
        /// After the bytes, encode them so the named agent submits the input
        /// hands-free (the completion-poke path). Trailing newlines are
        /// stripped first. Values: `claude` (Cmd+Enter chord),
        /// `gemini` (plain CR), `codex` (bracketed-paste wrap + CR; a bare CR
        /// is coalesced into a paste burst and lands as a newline, so it never
        /// submits). Omit it to write pure bytes: the input parks in the
        /// agent's compose box unsubmitted (a bare newline is a newline to an
        /// agent, not a submit).
        #[arg(long, value_name = "AGENT")]
        submit: Option<SubmitAgent>,
        /// Target every session with this tab name.
        #[arg(long = "tab-name")]
        tab_name: Option<String>,
        /// Target every session in this group (broadcast).
        #[arg(long = "tab-group")]
        tab_group: Option<String>,
    },
    /// List live terminal sessions, grouped by group. Markdown by
    /// default; `--json` for compact machine output, `--json --pretty`
    /// for indented JSON.
    List {
        /// Emit machine-readable JSON instead of the markdown table.
        #[arg(long)]
        json: bool,
        /// Indent the JSON output. Only meaningful with `--json`.
        #[arg(long)]
        pretty: bool,
    },
    /// Restart live terminal session(s) selected by name and/or group,
    /// preserving each session's spawn command and env so an agent
    /// relaunches. At least one selector is required. Used by the Team
    /// Work bootstrap to restart its own terminal (a shell cannot restart
    /// the shell running its own script; the server does it out of band).
    Restart {
        /// Restart every session with this tab name.
        #[arg(long = "tab-name")]
        tab_name: Option<String>,
        /// Restart every session in this group.
        #[arg(long = "tab-group")]
        tab_group: Option<String>,
    },
    /// Close (tear down) live terminal session(s) selected by name and/or
    /// group: kills the PTY and removes the session, so its tab name frees
    /// for re-use. The teardown partner to `restart` / `new`; at least one
    /// selector is required. `--tab-group` tears down a whole group (e.g. a
    /// finished team) in one call.
    Close {
        /// Close every session with this tab name.
        #[arg(long = "tab-name")]
        tab_name: Option<String>,
        /// Close every session in this group.
        #[arg(long = "tab-group")]
        tab_group: Option<String>,
    },
    /// Dump a live terminal session's scrollback (its replay ring) by tab
    /// name, printing the raw bytes to stdout. Exactly one session must
    /// match the name: zero is an error, and more than one is ambiguous
    /// (there is no group axis, since scrollback reads one terminal's
    /// history). Used by the lead process to read a worker's terminal.
    Scrollback {
        /// Tab name of the session to read. Required; must match exactly
        /// one live session.
        #[arg(long = "tab-name")]
        tab_name: String,
    },
    /// Raise a survey over the SPA window(s) that own the matching
    /// terminal tab(s) and BLOCK until the user answers. Prints the chosen
    /// option label to stdout, or (on `[F]`) the path of the followup file
    /// the UI created. At least one selector is required. Used by an agent
    /// to ask @@Host a question and wait for the decision.
    #[command(after_long_help = SURVEY_AFTER_HELP)]
    Survey {
        /// Raise the survey on the window owning this tab name.
        #[arg(long = "tab-name")]
        tab_name: Option<String>,
        /// Raise the survey on every window owning a tab in this group.
        #[arg(long = "tab-group")]
        tab_group: Option<String>,
        /// Optional heading shown above the body.
        #[arg(long)]
        title: Option<String>,
        /// An answer option (1..=4). Repeat for each: `--option A
        /// --option B`. The UI numbers them `[1]`..`[4]`.
        #[arg(long = "option", value_name = "LABEL")]
        option: Vec<String>,
        /// Team directory (workspace-relative) for the `[F]` follow-up
        /// paper-trail file, created at
        /// `{dir}/followups/followup-{from}-{to}-{n}.md`. `[F]` is shown on
        /// every survey regardless; PASSING this dir is what makes `[F]` write
        /// the file (and return its path) instead of a plain no-file deferral.
        /// The file is created EMPTY (question + empty comments): "deferred,
        /// not ready", NOT an answer -- the host must populate it before an
        /// agent acts on it.
        #[arg(long = "followup-dir", value_name = "TEAM_DIR")]
        followup_dir: Option<String>,
        /// Override the follow-up author (`from`). Defaults to
        /// `$CHAN_TAB_NAME` (the surveying agent's tab). Only used with
        /// `--followup-dir`.
        #[arg(long)]
        from: Option<String>,
        /// Override the follow-up target (`to`). Defaults to the survey
        /// target (`--tab-name`, or `--tab-group` for a group). Only used
        /// with `--followup-dir`.
        #[arg(long)]
        to: Option<String>,
        /// Seconds to wait for the host's reply before giving up. On elapse the
        /// survey returns no answer, prints `no reply within <secs>s` to
        /// stderr, and exits 124 (the GNU `timeout` convention), so a caller
        /// can tell a timed-out survey from an answered or dismissed one.
        #[arg(long, value_name = "SECS", default_value_t = crate::wire::DEFAULT_SURVEY_TIMEOUT_SECS)]
        timeout: u64,
        /// Read the markdown problem body from this process's stdin
        /// instead of the positional `body` (handy for multi-line bodies).
        #[arg(long)]
        stdin: bool,
        /// The markdown problem body. Multiple words join with spaces.
        /// Omit only with `--stdin`.
        #[arg(num_args = 0..)]
        body: Vec<String>,
    },
    /// Create or load a Team Work team (the CLI equivalent of the Cmd+P
    /// team setup/load dialog). A team is one `{dir}/config.toml`; `new`
    /// writes it (plus the server-regenerated `bootstrap.md` + the
    /// tasks/journals/followups tree), `load` reads an existing one, and
    /// `--script` on either emits the whole bootstrap as a runnable shell
    /// script instead of mutating anything.
    #[command(infer_subcommands = true)]
    #[command(after_long_help = TEAM_AFTER_HELP)]
    Team {
        #[command(subcommand)]
        action: TeamAction,
    },
}

/// The `cs terminal team` subcommands. `new` takes the config to write
/// (a `--config <file>` path or `--stdin`); `load` takes only the existing
/// team's `dir`. Both accept `--script` to emit the paste-and-run bootstrap
/// instead of running the operation.
#[derive(Subcommand, Debug)]
pub enum TeamAction {
    /// Validate + write a team from a config, materializing the
    /// `{dir}/config.toml`, the server-regenerated `bootstrap.md`, and the
    /// `tasks/journals/followups` tree inside the workspace.
    New {
        /// Workspace-relative team directory (the team lives at
        /// `{dir}/config.toml`).
        dir: String,
        /// Path to the team config.toml to write. Omit with `--stdin` or
        /// `--template`.
        #[arg(long, conflicts_with_all = ["stdin", "template"])]
        config: Option<PathBuf>,
        /// Read the team config.toml from this process's stdin instead of
        /// `--config` or `--template`.
        #[arg(long, conflicts_with_all = ["config", "template"])]
        stdin: bool,
        /// Load config from a saved template name stored in
        /// `~/.chan/team-templates/<name>.toml`. Conflicts with `--config`
        /// and `--stdin`.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["config", "stdin"])]
        template: Option<String>,
        /// Path to a brief Markdown file folded VERBATIM into the generated
        /// `bootstrap.md` (its own section after the Roster), so a round's
        /// custom operating instructions survive a normal `new`/regenerate.
        /// The CLI reads the file and sends its text; the server never sees the
        /// path. Omit for the generic bootstrap.
        #[arg(long, value_name = "FILE")]
        brief: Option<PathBuf>,
        /// Turn the chan MCP env vars ON or OFF for the team's terminals
        /// (sets `mcp_env` in the written config.toml). Default when omitted:
        /// OFF, matching the config default - agents still reach `cs search`
        /// and friends with MCP env off. `on` opts the whole team in; `off`
        /// writes it explicitly. Overrides any `mcp_env` in the input config.
        #[arg(long = "mcp-env", value_name = "ON_OFF")]
        mcp_env: Option<McpEnvToggle>,
        /// Emit the paste-and-run bootstrap shell script to stdout instead
        /// of writing the team. A pure preview: it mutates nothing.
        #[arg(long)]
        script: bool,
    },
    /// Read + validate an existing team's `{dir}/config.toml`. With
    /// `--script`, emit its paste-and-run bootstrap shell script.
    Load {
        /// Workspace-relative team directory to load.
        dir: String,
        /// Emit the paste-and-run bootstrap shell script to stdout.
        #[arg(long)]
        script: bool,
    },
}

/// Dispatch a `cs <action>` against the current window's chan-server.
/// Was `cmd_shell` in the `chan` binary.
pub async fn dispatch(action: ShellAction) -> Result<()> {
    match action {
        ShellAction::Open { path } => {
            let env = open_env()?;
            // No path -> open the terminal's cwd in the browser.
            let abs = absolutize(path.unwrap_or(PathBuf::from(".")))?;
            let message = send_control_request(
                &env.control_socket,
                ControlRequest::OpenPath {
                    window_id: env.window_id,
                    path: abs,
                },
            )
            .await?;
            eprintln!("{message}");
            Ok(())
        }
        ShellAction::Graph { path } => {
            let env = open_env()?;
            let abs = path.map(absolutize).transpose()?;
            let message = send_control_request(
                &env.control_socket,
                ControlRequest::OpenGraph {
                    window_id: env.window_id,
                    path: abs,
                },
            )
            .await?;
            eprintln!("{message}");
            Ok(())
        }
        ShellAction::Dashboard {
            carousel_index,
            carousel_off,
        } => {
            let env = open_env()?;
            let message = send_control_request(
                &env.control_socket,
                ControlRequest::OpenDashboard {
                    window_id: env.window_id,
                    carousel_index,
                    carousel_off,
                },
            )
            .await?;
            eprintln!("{message}");
            Ok(())
        }
        ShellAction::Upload { path } => {
            let env = open_env()?;
            // PATH is required (`.` for the current dir). absolutize resolves it
            // against the CLI's cwd; the server relativizes it to the workspace
            // (bounded) or keeps it cwd-scoped on a standalone terminal.
            let abs = absolutize(path)?;
            let message = send_control_request(
                &env.control_socket,
                ControlRequest::Upload {
                    window_id: env.window_id,
                    path: abs,
                },
            )
            .await?;
            eprintln!("{message}");
            Ok(())
        }
        ShellAction::Download { path } => {
            let env = open_env()?;
            let abs = absolutize(path)?;
            let message = send_control_request(
                &env.control_socket,
                ControlRequest::Download {
                    window_id: env.window_id,
                    path: abs,
                },
            )
            .await?;
            eprintln!("{message}");
            Ok(())
        }
        ShellAction::Copy { mime, html } => cmd_shell_copy(mime, html).await,
        ShellAction::Paste { text, html, image } => cmd_shell_paste(text, html, image).await,
        ShellAction::Terminal { action } => cmd_shell_terminal(action).await,
        ShellAction::Window { action } => match action {
            WindowAction::List { json, pretty } => cmd_window_list(json, pretty).await,
            WindowAction::New => cmd_window_op(ControlRequest::WindowNew).await,
            WindowAction::Open { id } => cmd_window_op(ControlRequest::WindowOpen { id }).await,
            WindowAction::Rm { id, force } => {
                cmd_window_op(ControlRequest::WindowClose { id, force }).await
            }
            WindowAction::Hide { id } => cmd_window_op(ControlRequest::WindowHide { id }).await,
        },
        ShellAction::Session { action } => match action {
            SessionAction::List { json, pretty } => cmd_session_list(json, pretty).await,
            SessionAction::SelfCmd { name } => {
                let env = open_env()?;
                cmd_session_op(ControlRequest::SessionSelf {
                    window_id: env.window_id,
                    name,
                })
                .await
            }
            SessionAction::Handover {
                to,
                accept,
                reject,
                timeout,
            } => {
                let env = open_env()?;
                cmd_session_op(ControlRequest::SessionHandover {
                    window_id: env.window_id,
                    to,
                    accept,
                    reject,
                    timeout_secs: timeout,
                })
                .await
            }
            SessionAction::Takeover { force } => {
                let env = open_env()?;
                cmd_session_op(ControlRequest::SessionTakeover {
                    window_id: env.window_id,
                    force,
                })
                .await
            }
        },
        ShellAction::Search {
            query,
            limit,
            json,
            pretty,
        } => cmd_shell_search(query.join(" "), limit, json, pretty).await,
        ShellAction::Pane {
            tab_name,
            json,
            pretty,
            action,
        } => cmd_pane(tab_name, json, pretty, action).await,
    }
}

/// `cs window list`: fetch the library's authoritative window set (the same
/// `WindowRecord` feed the desktop watcher and launcher reconcile to) and
/// print it. Session-scoped like `cs terminal list`: needs only
/// $CHAN_CONTROL_SOCKET, no window id. A standalone `chan open` has no
/// library and lists no windows.
async fn cmd_window_list(json: bool, pretty: bool) -> Result<()> {
    let socket = control_socket_env()?;
    let raw = send_control_request(&socket, ControlRequest::WindowList).await?;
    if json {
        if pretty {
            let value: serde_json::Value =
                serde_json::from_str(&raw).context("parsing window list JSON")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).context("formatting window list JSON")?
            );
        } else {
            println!("{raw}");
        }
    } else {
        print!("{}", render_window_list_markdown(&raw)?);
    }
    Ok(())
}

/// `cs window <new|open|rm|hide>`: send a one-shot window-lifecycle
/// request and print the server's reply (the new window id for `new`, a
/// short confirmation otherwise). Session-scoped like `cs window list`:
/// needs only $CHAN_CONTROL_SOCKET, no window id. `rm` of a window with
/// live terminals blocks here until the desktop's confirmation dialog is
/// answered (or `--force` was passed).
async fn cmd_window_op(req: ControlRequest) -> Result<()> {
    let socket = control_socket_env()?;
    let message = send_control_request(&socket, req).await?;
    println!("{message}");
    Ok(())
}

/// Render the `cs window list` rows (library `WindowRecord`s:
/// `{window_id, library_id, kind, title, ordinal, connected, …}`) as a
/// markdown table. Titles are library-owned and auto-derived; `connected`
/// means a live `/ws` socket is tagged with the window right now. Every row
/// in the set is a persisted library record, so `connected` is the only
/// status axis.
fn render_window_list_markdown(raw: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(raw).context("parsing window list JSON")?;
    let rows = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("window list JSON is not an array"))?;
    if rows.is_empty() {
        return Ok("No windows.\n".to_string());
    }
    let mut out = String::from(
        "| window | library | kind | title | # | status |\n\
         | --- | --- | --- | --- | --- | --- |\n",
    );
    for row in rows {
        let id = row.get("window_id").and_then(|v| v.as_str()).unwrap_or("?");
        let library = row.get("library_id").and_then(|v| v.as_str()).unwrap_or("");
        let kind = row.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let title = row.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let ordinal = row.get("ordinal").and_then(|v| v.as_u64()).unwrap_or(0);
        let connected = row
            .get("connected")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let status = if connected { "connected" } else { "offline" };
        out.push_str(&format!(
            "| {id} | {library} | {kind} | {title} | {ordinal} | {status} |\n"
        ));
    }
    Ok(out)
}

/// `cs session list`: fetch the session participant roster (window id, name,
/// role, status) and print it. Session-scoped like `cs window list`: needs
/// only $CHAN_CONTROL_SOCKET, no window id.
async fn cmd_session_list(json: bool, pretty: bool) -> Result<()> {
    let socket = control_socket_env()?;
    let raw = send_control_request(&socket, ControlRequest::SessionList).await?;
    if json {
        if pretty {
            let value: serde_json::Value =
                serde_json::from_str(&raw).context("parsing session list JSON")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).context("formatting session list JSON")?
            );
        } else {
            println!("{raw}");
        }
    } else {
        print!("{}", render_session_list_markdown(&raw)?);
    }
    Ok(())
}

/// `cs session <self|handover|takeover>`: send a session command and print the
/// server's reply. A `handover` request BLOCKS here until the leader accepts /
/// rejects or the timeout elapses (the CLI exits 124 on timeout, like
/// `cs window rm` blocking on the desktop dialog).
async fn cmd_session_op(req: ControlRequest) -> Result<()> {
    let socket = control_socket_env()?;
    let message = send_control_request(&socket, req).await?;
    println!("{message}");
    Ok(())
}

/// Render the `cs session list` rows (`{window_id, name, role, status}`) as a
/// markdown table. `role` is leader or follower; `status` is the participant
/// lifecycle state (live / disconnecting / disconnected / gone).
fn render_session_list_markdown(raw: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).context("parsing session list JSON")?;
    let rows = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("session list JSON is not an array"))?;
    if rows.is_empty() {
        return Ok("No session participants.\n".to_string());
    }
    let mut out = String::from(
        "| window | name | role | status |\n\
         | --- | --- | --- | --- |\n",
    );
    for row in rows {
        let window = row.get("window_id").and_then(|v| v.as_str()).unwrap_or("?");
        let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let role = row.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let status = row.get("status").and_then(|v| v.as_str()).unwrap_or("");
        out.push_str(&format!("| {window} | {name} | {role} | {status} |\n"));
    }
    Ok(out)
}

/// `cs search <query>`: run the workspace content search on the running
/// server (the same `Workspace::search` the UI's `/api/search/content`
/// uses) and print the results. Markdown table by default; `--json`
/// compact, `--json --pretty` indented. Mirrors the `cs terminal list`
/// output convention.
async fn cmd_shell_search(
    query: String,
    limit: Option<u32>,
    json: bool,
    pretty: bool,
) -> Result<()> {
    let socket = control_socket_env()?;
    let raw = send_control_request(&socket, ControlRequest::Search { query, limit }).await?;
    if json {
        // Compact by default; --pretty re-indents. Both go to stdout so
        // the output pipes cleanly.
        if pretty {
            let value: serde_json::Value =
                serde_json::from_str(&raw).context("parsing search JSON")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).context("formatting search JSON")?
            );
        } else {
            println!("{raw}");
        }
    } else {
        print!("{}", render_search_markdown(&raw)?);
    }
    Ok(())
}

/// Render the `cs search` result JSON
/// (`{ready, mode, query, hits: [{path, heading, start_line, snippet,
/// score}]}`) as a markdown list. This is the default human output;
/// `--json` emits the raw payload instead. No hits yields a short line
/// rather than an empty list.
fn render_search_markdown(raw: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(raw).context("parsing search JSON")?;
    let hits = value
        .get("hits")
        .and_then(|h| h.as_array())
        .ok_or_else(|| anyhow::anyhow!("search JSON missing `hits`"))?;
    if hits.is_empty() {
        return Ok("No matches.\n".to_string());
    }
    let str_field = |h: &serde_json::Value, key: &str| {
        h.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let mut out = String::new();
    for h in hits {
        let path = str_field(h, "path");
        let heading = str_field(h, "heading");
        let line = h.get("start_line").and_then(|v| v.as_u64()).unwrap_or(0);
        // `path:line` locator, then the best heading, then the snippet on
        // an indented continuation so the list stays scannable.
        if heading.is_empty() {
            out.push_str(&format!("- {path}:{line}\n"));
        } else {
            out.push_str(&format!("- {path}:{line} - {heading}\n"));
        }
        let snippet = str_field(h, "snippet");
        if !snippet.is_empty() {
            // The BM25 snippet highlights matches with `<b>...</b>` (the
            // markup the frontend renders); convert to markdown `**bold**`
            // for this markdown output. Collapse newlines so one hit stays
            // on one logical block.
            let flat = snippet
                .replace('\n', " ")
                .replace("<b>", "**")
                .replace("</b>", "**");
            out.push_str(&format!("  {}\n", flat.trim()));
        }
    }
    Ok(out)
}

/// The `(window_id, tab_name)` target a `cs pane` request carries. An
/// explicit `--tab-name` targets the window owning that tab (and needs no
/// $CHAN_WINDOW_ID); otherwise the caller's own window from $CHAN_WINDOW_ID.
/// Sending one or the other (never both) keeps the server's precedence
/// unambiguous; the server errors when neither resolves.
fn pane_target(tab_name: Option<String>) -> (Option<String>, Option<String>) {
    let trimmed = |s: String| {
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    match tab_name.and_then(trimmed) {
        Some(tab) => (None, Some(tab)),
        None => (std::env::var("CHAN_WINDOW_ID").ok().and_then(trimmed), None),
    }
}

/// `cs pane`: inspect or drive the target window's tab/pane layout over the
/// control socket (the server pushes a `pane_query` / `pane_exec` to the SPA,
/// which replies). Bare = the layout report; a subcommand = a mutation. The
/// target is the caller's own window or `--tab-name`. Markdown by default;
/// `--json [--pretty]` for machine output. A close blocked by a dirty file /
/// live terminal (without `--force`) exits non-zero.
async fn cmd_pane(
    tab_name: Option<String>,
    json: bool,
    pretty: bool,
    action: Option<PaneAction>,
) -> Result<()> {
    let socket = control_socket_env()?;
    let (window_id, tab_name) = pane_target(tab_name);
    let is_query = action.is_none();
    let request = match action {
        None => ControlRequest::PaneQuery {
            window_id,
            tab_name,
        },
        Some(action) => ControlRequest::PaneExec {
            window_id,
            tab_name,
            op: action.into_op(),
        },
    };
    let raw = send_control_request(&socket, request).await?;
    if json {
        // Compact by default; --pretty re-indents. Both go to stdout so the
        // output pipes cleanly.
        if pretty {
            let value: serde_json::Value =
                serde_json::from_str(&raw).context("parsing pane reply JSON")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).context("formatting pane reply JSON")?
            );
        } else {
            println!("{raw}");
        }
    } else if is_query {
        print!("{}", render_pane_layout_markdown(&raw)?);
    } else {
        print!("{}", render_pane_exec_markdown(&raw)?);
    }
    // An exec that was blocked (a dirty file / live terminal without --force)
    // completed the round-trip but did not fully apply; surface it as a
    // non-zero exit so scripts can react. The detail is already on stdout.
    if !is_query {
        let value: serde_json::Value =
            serde_json::from_str(&raw).context("parsing pane exec reply")?;
        if !value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            anyhow::bail!("cs pane: the operation was blocked (see output above)");
        }
    }
    Ok(())
}

/// Render a `cs pane <exec>` result. Shape (the SPA builds it):
/// `{ ok, summary, blocked: [{ tab, reason }] }`. Prints the summary, then a
/// `blocked:` list when a close hit a dirty file / live terminal. Falls back
/// to a bare `ok` / `blocked` line if the SPA omitted a summary.
fn render_pane_exec_markdown(raw: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(raw).context("parsing pane exec JSON")?;
    let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut out = String::new();
    if let Some(summary) = value.get("summary").and_then(|v| v.as_str()) {
        if !summary.is_empty() {
            out.push_str(summary);
            out.push('\n');
        }
    }
    if let Some(blocked) = value.get("blocked").and_then(|v| v.as_array()) {
        if !blocked.is_empty() {
            out.push_str("blocked:\n");
            for b in blocked {
                let tab = b.get("tab").and_then(|v| v.as_str()).unwrap_or("?");
                let reason = b.get("reason").and_then(|v| v.as_str()).unwrap_or("?");
                out.push_str(&format!("  - {tab}: {reason}\n"));
            }
        }
    }
    if out.is_empty() {
        out.push_str(if ok { "ok\n" } else { "blocked\n" });
    }
    Ok(out)
}

/// Render the `cs pane` layout snapshot JSON as one markdown table per pane.
/// Shape (the SPA's `handleWindowCommand` pane responder builds it):
/// `{activePaneId, panes: [{id, active, activeTabId, tabs: [{id, kind,
/// title, active, dirty?, live?}]}]}`. The active pane is flagged in its
/// heading; per tab, a `*` marks the pane's active tab and the `flags`
/// column carries `dirty` (unsaved file) / `live` (running terminal). An
/// empty layout yields a short line rather than a blank table.
fn render_pane_layout_markdown(raw: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(raw).context("parsing pane layout JSON")?;
    let panes = value
        .get("panes")
        .and_then(|p| p.as_array())
        .ok_or_else(|| anyhow::anyhow!("pane layout JSON missing `panes`"))?;
    if panes.is_empty() {
        return Ok("No panes.\n".to_string());
    }
    let active_pane = value.get("activePaneId").and_then(|v| v.as_str());
    let str_field = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let mut out = String::new();
    for pane in panes {
        let id = str_field(pane, "id");
        let is_active = pane
            .get("active")
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| active_pane == Some(id.as_str()));
        let active_tab = pane.get("activeTabId").and_then(|v| v.as_str());
        if is_active {
            out.push_str(&format!("## pane {id} (active)\n\n"));
        } else {
            out.push_str(&format!("## pane {id}\n\n"));
        }
        let tabs = pane.get("tabs").and_then(|t| t.as_array());
        let empty = tabs.map(|t| t.is_empty()).unwrap_or(true);
        if empty {
            out.push_str("(empty)\n\n");
            continue;
        }
        out.push_str("| tab | kind | title | flags |\n");
        out.push_str("| --- | --- | --- | --- |\n");
        for tab in tabs.into_iter().flatten() {
            let tab_id = str_field(tab, "id");
            let kind = str_field(tab, "kind");
            let title = str_field(tab, "title");
            // `*` marks the active tab (either the explicit `active` flag or
            // a match against the pane's activeTabId).
            let is_active_tab = tab
                .get("active")
                .and_then(|v| v.as_bool())
                .unwrap_or_else(|| active_tab == Some(tab_id.as_str()));
            let marker = if is_active_tab { "*" } else { "" };
            let mut flags: Vec<&str> = Vec::new();
            if tab.get("dirty").and_then(|v| v.as_bool()).unwrap_or(false) {
                flags.push("dirty");
            }
            if tab.get("live").and_then(|v| v.as_bool()).unwrap_or(false) {
                flags.push("live");
            }
            out.push_str(&format!(
                "| {tab_id}{marker} | {kind} | {title} | {} |\n",
                flags.join(", ")
            ));
        }
        out.push('\n');
    }
    Ok(out)
}

/// `cs copy`: read all of stdin and push it onto the window's clipboard. The
/// bytes ride a base64 string on the control socket (a JSON envelope), so an
/// image and text share one path. `--html` maps to `--mime text/html`;
/// otherwise the server sniffs the content type from the bytes.
async fn cmd_shell_copy(mime: Option<String>, html: bool) -> Result<()> {
    let env = open_env()?;
    let mut buf = Vec::new();
    {
        use std::io::Read;
        // Bound the read: the clipboard is for modest content, so cap it (and
        // read one byte past the cap to detect an oversized input) instead of
        // buffering an unbounded stdin -- `cs copy < /dev/zero` never EOFs.
        std::io::stdin()
            .take(MAX_CLIPBOARD_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .context("reading stdin for cs copy")?;
    }
    if buf.is_empty() {
        anyhow::bail!("nothing on stdin to copy");
    }
    if buf.len() > MAX_CLIPBOARD_BYTES {
        anyhow::bail!(
            "clipboard payload too large (max {} MB)",
            MAX_CLIPBOARD_BYTES / (1024 * 1024)
        );
    }
    let mime = if html {
        Some("text/html".to_string())
    } else {
        mime
    };
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
    let message = send_control_request(
        &env.control_socket,
        ControlRequest::ClipboardCopy {
            window_id: env.window_id,
            data_b64,
            mime,
        },
    )
    .await?;
    eprintln!("{message}");
    Ok(())
}

/// The `{ mime, data_b64 }` reply the SPA sends back for `cs paste`, delivered
/// as JSON in the control response `message`. `data_b64` is base64 of the raw
/// clipboard bytes, which the CLI writes verbatim to stdout.
#[derive(serde::Deserialize)]
struct ClipboardPasteReply {
    mime: String,
    data_b64: String,
}

/// `cs paste`: read the window's clipboard to stdout. The server replies with
/// a `{ mime, data_b64 }` JSON line; decode the base64 and write the RAW bytes
/// to stdout (so `cs paste > file.png` yields the real asset), reporting the
/// emitted MIME on stderr.
async fn cmd_shell_paste(text: bool, html: bool, image: bool) -> Result<()> {
    let env = open_env()?;
    // clap marks the three flags mutually exclusive, so at most one is set.
    let prefer = if text {
        PastePrefer::Text
    } else if html {
        PastePrefer::Html
    } else if image {
        PastePrefer::Image
    } else {
        PastePrefer::Auto
    };
    let message = send_control_request(
        &env.control_socket,
        ControlRequest::ClipboardPaste {
            window_id: env.window_id,
            prefer,
        },
    )
    .await?;
    let reply: ClipboardPasteReply =
        serde_json::from_str(&message).context("decoding clipboard paste reply")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(reply.data_b64.as_bytes())
        .context("decoding clipboard base64")?;
    {
        use std::io::Write;
        std::io::stdout()
            .write_all(&bytes)
            .context("writing clipboard bytes to stdout")?;
    }
    // The MIME goes to stderr so it never pollutes a `> file` redirect.
    eprintln!("{}", reply.mime);
    Ok(())
}

async fn cmd_shell_terminal(action: TerminalAction) -> Result<()> {
    match action {
        TerminalAction::New {
            path,
            tab_name,
            tab_group,
        } => {
            let env = open_env()?;
            let abs = path.map(absolutize).transpose()?;
            let message = send_control_request(
                &env.control_socket,
                ControlRequest::OpenTermNew {
                    window_id: env.window_id,
                    path: abs,
                    tab_name,
                    tab_group,
                },
            )
            .await?;
            eprintln!("{message}");
            Ok(())
        }
        TerminalAction::Write {
            cmd,
            stdin,
            submit,
            tab_name,
            tab_group,
        } => {
            if tab_name.is_none() && tab_group.is_none() {
                anyhow::bail!("cs terminal write needs --tab-name and/or --tab-group");
            }
            // Raw bytes, no implicit newline. --stdin
            // reads this process's stdin to EOF; otherwise the literal
            // `cmd`. Terminal input is UTF-8 text.
            let data = if stdin {
                use std::io::Read;
                let mut buf = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut buf)
                    .context("reading stdin")?;
                String::from_utf8(buf).context("stdin must be UTF-8 for cs terminal write")?
            } else {
                cmd.ok_or_else(|| anyhow::anyhow!("cs terminal write needs a command or --stdin"))?
            };
            // --submit=<agent>: strip trailing newlines then append that
            // agent's submit chord so a running agent submits the input
            // hands-free (the completion poke). Most agents take ONE write;
            // gemini needs the chord as a SEPARATE write (it coalesces a bulk
            // text+CR into a newline), so submit_writes may return two. Each
            // goes as its own TermWrite -> its own write-queue item, which the
            // per-session drainer delivers idle-gated, so the CR lands as a
            // distinct keypress. Mirrors submit_writes / encodeForAgentSubmit.
            let socket = control_socket_env()?;
            for write in submit_writes(data, submit) {
                let message = send_control_request(
                    &socket,
                    ControlRequest::TermWrite {
                        tab_name: tab_name.clone(),
                        tab_group: tab_group.clone(),
                        data: write,
                    },
                )
                .await?;
                eprintln!("{message}");
            }
            Ok(())
        }
        TerminalAction::List { json, pretty } => {
            let socket = control_socket_env()?;
            let raw = send_control_request(&socket, ControlRequest::TermList).await?;
            if json {
                // Compact by default; --pretty re-indents. Both go to
                // stdout so the output pipes cleanly.
                if pretty {
                    let value: serde_json::Value =
                        serde_json::from_str(&raw).context("parsing terminal list JSON")?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&value)
                            .context("formatting terminal list JSON")?
                    );
                } else {
                    println!("{raw}");
                }
            } else {
                print!("{}", render_terminal_list_markdown(&raw)?);
            }
            Ok(())
        }
        TerminalAction::Restart {
            tab_name,
            tab_group,
        } => {
            if tab_name.is_none() && tab_group.is_none() {
                anyhow::bail!("cs terminal restart needs --tab-name and/or --tab-group");
            }
            let socket = control_socket_env()?;
            let message = send_control_request(
                &socket,
                ControlRequest::TermRestart {
                    tab_name,
                    tab_group,
                },
            )
            .await?;
            eprintln!("{message}");
            Ok(())
        }
        TerminalAction::Close {
            tab_name,
            tab_group,
        } => {
            if tab_name.is_none() && tab_group.is_none() {
                anyhow::bail!("cs terminal close needs --tab-name and/or --tab-group");
            }
            let socket = control_socket_env()?;
            let message = send_control_request(
                &socket,
                ControlRequest::TermClose {
                    tab_name,
                    tab_group,
                },
            )
            .await?;
            eprintln!("{message}");
            Ok(())
        }
        TerminalAction::Scrollback { tab_name } => {
            let socket = control_socket_env()?;
            let raw =
                send_control_request(&socket, ControlRequest::TermScrollback { tab_name }).await?;
            // The scrollback is the captured artifact, so it goes to stdout
            // (pipes cleanly into a file or a pager). No trailing newline is
            // added: the ring already carries the session's own line breaks.
            print!("{raw}");
            Ok(())
        }
        TerminalAction::Survey {
            tab_name,
            tab_group,
            title,
            option,
            followup_dir,
            from,
            to,
            timeout,
            stdin,
            body,
        } => {
            cmd_shell_survey(SurveyArgs {
                tab_name,
                tab_group,
                title,
                option,
                followup_dir,
                from,
                to,
                timeout_secs: timeout,
                stdin,
                body,
            })
            .await
        }
        TerminalAction::Team { action } => cmd_shell_team(action).await,
    }
}

/// `cs terminal team new|load`: round-trip a [`ControlRequest::TerminalTeam`]
/// so the server owns the parse / validate / write / bootstrap generation
/// (the same path the `/api/team-config` route uses). `new` reads the input
/// config.toml from `--config <file>` or `--stdin`; `load` carries no
/// config. With `--script` the server returns the paste-and-run bootstrap
/// script, which prints to STDOUT (the captured artifact); otherwise the
/// one-line ack/summary goes to stderr like the other queueing commands.
async fn cmd_shell_team(action: TeamAction) -> Result<()> {
    let socket = control_socket_env()?;
    // The caller's window, when run from a chan terminal that owns one, so
    // the server binds each spawned agent session to it ($CHAN_WINDOW_ID
    // flows to the agents, like a regular SPA terminal). A windowless caller
    // (a native terminal) omits it and the agents spawn unbound, as before.
    let window_id = std::env::var("CHAN_WINDOW_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let (request, script) = match action {
        TeamAction::New {
            dir,
            config,
            stdin,
            template,
            brief,
            mcp_env,
            script,
        } => {
            let mut config_toml = if let Some(ref name) = template {
                read_team_template_input(name)?
            } else {
                read_team_config_input(config, stdin)?
            };
            // --mcp-env overrides the input config's `mcp_env` (or adds it).
            // Omitted -> leave the config as-is (server's serde default is OFF).
            if let Some(toggle) = mcp_env {
                config_toml = set_team_mcp_env(&config_toml, toggle.as_bool())?;
            }
            // Read the brief file CLIENT-side into text; the server has no
            // access to the caller's filesystem (same reason config travels as
            // text). Absent -> None, the generic bootstrap.
            let brief_content = read_brief_input(brief)?;
            (
                ControlRequest::TerminalTeam {
                    dir: resolve_team_dir(&dir)?,
                    op: TeamOp::New,
                    config_toml: Some(config_toml),
                    brief_content,
                    script,
                    window_id,
                },
                script,
            )
        }
        TeamAction::Load { dir, script } => (
            ControlRequest::TerminalTeam {
                dir: resolve_team_dir(&dir)?,
                op: TeamOp::Load,
                config_toml: None,
                // Load never regenerates the bootstrap, so a brief is moot.
                brief_content: None,
                script,
                window_id,
            },
            script,
        ),
    };
    let message = send_control_request(&socket, request).await?;
    if script {
        // The script is the result the caller captures, so it goes to
        // stdout (pipes cleanly into a file), matching `cs terminal survey`.
        println!("{message}");
    } else {
        eprintln!("{message}");
    }
    Ok(())
}

/// Read the optional `cs terminal team new --brief <file>` into text. The
/// server has no access to the caller's filesystem, so the CLI reads the file
/// and sends its CONTENT (the same reason the config travels as text). `None`
/// when `--brief` was omitted -> the generic bootstrap.
fn read_brief_input(brief: Option<PathBuf>) -> Result<Option<String>> {
    match brief {
        None => Ok(None),
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading team brief {}", path.display()))?;
            Ok(Some(text))
        }
    }
}

/// Read a saved template by name from `~/.chan/team-templates/<name>.toml`
/// and return its TOML text for `cs terminal team new --template <name>`.
fn read_team_template_input(name: &str) -> Result<String> {
    let safe: String = name
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if safe.is_empty() || safe.starts_with('-') || safe.contains("..") {
        anyhow::bail!("invalid template name: {name:?}");
    }
    // Mirror chan_workspace::paths::config_dir(): $CHAN_HOME or ~/.chan.
    // Windows home is USERPROFILE; Unix is HOME.
    let chan_home = std::env::var("CHAN_HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = if cfg!(windows) {
                std::env::var("USERPROFILE").ok()
            } else {
                std::env::var("HOME").ok()
            };
            home.map(|h| PathBuf::from(h).join(".chan"))
                .unwrap_or_else(|| PathBuf::from(".chan"))
        });
    let path = chan_home
        .join("team-templates")
        .join(format!("{safe}.toml"));
    std::fs::read_to_string(&path)
        .with_context(|| format!("template '{name}' not found at {}", path.display()))
}

/// Resolve the `cs terminal team new` config.toml input from `--config
/// <file>` XOR `--stdin`. Bails with a clear message if both or neither is
/// given, mirroring the `cs terminal write` / `survey` body precedence.
fn read_team_config_input(config: Option<PathBuf>, stdin: bool) -> Result<String> {
    match (config, stdin) {
        (Some(_), true) => {
            anyhow::bail!("pass either --config <file> or --stdin, not both")
        }
        (Some(path), false) => std::fs::read_to_string(&path)
            .with_context(|| format!("reading team config {}", path.display())),
        (None, true) => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading team config from stdin")?;
            Ok(buf)
        }
        (None, false) => {
            anyhow::bail!("cs terminal team new needs a config: --config <file> or --stdin")
        }
    }
}

/// Set the top-level `mcp_env` key in a team config TOML string, so
/// `cs terminal team new --mcp-env on|off` overrides whatever the input config
/// had (or adds it when absent). The server re-parses + regenerates
/// config.toml from this, so the only requirement is a valid TOML document
/// with `mcp_env` at the root (before the `[[members]]` tables). Parsing +
/// re-serializing via `toml` keeps the key at the document root regardless of
/// where the input put its tables, which a naive string append cannot.
fn set_team_mcp_env(config_toml: &str, value: bool) -> Result<String> {
    let mut doc: toml::Table = config_toml
        .parse()
        .context("parsing team config TOML to apply --mcp-env")?;
    doc.insert("mcp_env".to_string(), toml::Value::Boolean(value));
    toml::to_string(&doc).context("re-serializing team config after --mcp-env")
}

/// Resolve a user-typed `cs terminal team` dir to a WORKSPACE-relative dir,
/// against the caller's current directory. `cs` runs inside a chan terminal,
/// so `$CHAN_WORKSPACE_PATH` names the served workspace root and the process
/// cwd locates the caller within it. This gives `team new` / `team load` the
/// same cwd-awareness as `cs open` (a bare name, `.`, a relative path, or an
/// absolute path under the workspace all resolve) while keeping the wire
/// `dir` workspace-relative, so the server, the `--script` generator, and the
/// `/api/team-config` route stay unchanged. The env lookups live here; the
/// pure resolution is [`resolve_team_dir_in`] (the `open_env_from` split).
fn resolve_team_dir(dir: &str) -> Result<String> {
    let workspace = std::env::var("CHAN_WORKSPACE_PATH").ok();
    let cwd = std::env::current_dir().context("resolving current directory")?;
    resolve_team_dir_in(dir, workspace.as_deref(), &cwd)
}

/// The pure dir resolution: anchor `dir` to `workspace` (the served root)
/// via `cwd`. Resolution is LEXICAL (the target is never canonicalized) so a
/// `team new` dir that does not exist yet still resolves; `cwd` and the
/// workspace root, which do exist, ARE canonicalized so a symlinked prefix
/// (macOS `/tmp` -> `/private/tmp`) does not break the prefix match. With no
/// `workspace` (running outside a chan terminal, where the control socket is
/// missing too), the dir passes through unchanged, preserving the prior
/// workspace-relative contract.
fn resolve_team_dir_in(dir: &str, workspace: Option<&str>, cwd: &Path) -> Result<String> {
    let trimmed = dir.trim();
    if trimmed.is_empty() {
        anyhow::bail!("team directory is required");
    }
    let Some(workspace) = workspace.map(str::trim).filter(|w| !w.is_empty()) else {
        return Ok(trimmed.to_string());
    };
    let ws_root = canonical_or(Path::new(workspace));
    let input = Path::new(trimmed);
    // An absolute input stands on its own; a relative one (including ".")
    // joins the caller's cwd.
    let abs = if input.is_absolute() {
        input.to_path_buf()
    } else {
        canonical_or(cwd).join(input)
    };
    let normalized = lexical_normalize(&abs);
    let rel = normalized.strip_prefix(&ws_root).map_err(|_| {
        anyhow::anyhow!(
            "team directory {trimmed:?} is outside the workspace ({})",
            ws_root.display()
        )
    })?;
    let rel = path_to_posix(rel);
    if rel.is_empty() {
        anyhow::bail!("team directory resolves to the workspace root; name a subdirectory");
    }
    Ok(rel)
}

/// Canonicalize `path`, falling back to the path verbatim when it cannot be
/// resolved (e.g. it does not exist). Used on the cwd + workspace root, which
/// normally exist, so the fallback is just defensive.
fn canonical_or(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Resolve `.` and `..` components lexically, without touching the
/// filesystem, so a not-yet-existing `team new` dir still normalizes. A `..`
/// that would climb above the accumulated path just pops the last component.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Join a relative path's `Normal` components with `/` for the workspace-
/// relative wire string. Mirrors the server's `path_to_posix`; defined here
/// so the CLI does not depend on a server-private helper.
fn path_to_posix(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The parsed `cs terminal survey` arguments, grouped so the dispatch does
/// not pass ten positional parameters around.
struct SurveyArgs {
    tab_name: Option<String>,
    tab_group: Option<String>,
    title: Option<String>,
    option: Vec<String>,
    followup_dir: Option<String>,
    from: Option<String>,
    to: Option<String>,
    timeout_secs: u64,
    stdin: bool,
    body: Vec<String>,
}

/// `cs terminal survey`: build a [`SurveySpec`] and round-trip a BLOCKING
/// [`ControlRequest::TermSurvey`]. The server holds the connection open
/// until the user answers, so this call blocks; the reply (the chosen
/// option label, or the followup-file path on `[F]`) goes to stdout so it
/// pipes cleanly, matching the "the tool returns that option" contract.
async fn cmd_shell_survey(args: SurveyArgs) -> Result<()> {
    let SurveyArgs {
        tab_name,
        tab_group,
        title,
        option,
        followup_dir,
        from,
        to,
        timeout_secs,
        stdin,
        body,
    } = args;

    if tab_name.is_none() && tab_group.is_none() {
        anyhow::bail!("cs terminal survey needs --tab-name and/or --tab-group");
    }
    // The contract caps options at 1..=4 (the UI numbers them [1]..[4]).
    if option.is_empty() || option.len() > 4 {
        anyhow::bail!(
            "cs terminal survey needs 1..=4 --option values (got {})",
            option.len()
        );
    }
    // Body comes from stdin (multi-line bodies) or the positional words.
    let body_markdown = if stdin {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading survey body from stdin")?;
        buf
    } else {
        body.join(" ")
    };
    if body_markdown.trim().is_empty() {
        anyhow::bail!("cs terminal survey needs a markdown body (positional words or --stdin)");
    }
    // Part C: [F] is standard on every survey. PASSING --followup-dir is what
    // attaches the team context so [F] writes a paper-trail file; without it
    // the survey carries `followup: null` and [F] is a plain no-file deferral.
    let followup_ctx = if followup_dir.is_some() {
        Some(build_followup(
            followup_dir,
            from,
            to,
            &tab_name,
            &tab_group,
        )?)
    } else {
        None
    };
    let spec = SurveySpec {
        // Server-minted; left empty here (see SurveySpec docs).
        survey_id: String::new(),
        title,
        body_markdown,
        options: option,
        followup: followup_ctx,
    };
    let socket = control_socket_env()?;
    let result = send_control_request(
        &socket,
        ControlRequest::TermSurvey {
            tab_name,
            tab_group,
            spec,
            timeout_secs,
        },
    )
    .await;
    match classify_survey_result(result)? {
        // The reply is the result the caller wants captured, so it goes to
        // stdout (unlike the queued-request acks the other commands eprintln).
        SurveyOutcome::Answered(message) => {
            println!("{message}");
            Ok(())
        }
        // No answer within `--timeout`: the notice goes to STDERR so stdout
        // stays empty for a `$(cs terminal survey ...)` capture, and exit 124
        // lets a script branch on the timeout. stderr is unbuffered, so the
        // line lands before the hard exit skips the runtime shutdown.
        SurveyOutcome::TimedOut(message) => {
            eprintln!("{message}");
            std::process::exit(crate::exit_code::SURVEY_TIMEOUT);
        }
    }
}

/// The terminal outcome of a blocking `cs terminal survey` round-trip. Split
/// from [`cmd_shell_survey`] so the print-stream + exit-code decision is
/// unit-testable without a live server or a `process::exit`.
#[derive(Debug)]
enum SurveyOutcome {
    /// The host answered, deferred (`[F]`), or dismissed: the reply line is
    /// printed to stdout and the process exits 0.
    Answered(String),
    /// `--timeout` elapsed with no reply: the message is printed to stderr and
    /// the process exits [`crate::exit_code::SURVEY_TIMEOUT`] (124).
    TimedOut(String),
}

/// Classify a [`send_control_request`] result for the survey command: a plain
/// reply is [`SurveyOutcome::Answered`]; the typed timeout error
/// ([`crate::exit_code::ControlTimeout`], from a `ControlResponse::Timeout`)
/// becomes [`SurveyOutcome::TimedOut`]; any other error propagates (exit 1).
fn classify_survey_result(result: Result<String>) -> Result<SurveyOutcome> {
    match result {
        Ok(message) => Ok(SurveyOutcome::Answered(message)),
        Err(err) => match err.downcast::<crate::exit_code::ControlTimeout>() {
            Ok(timeout) => Ok(SurveyOutcome::TimedOut(timeout.message)),
            Err(other) => Err(other),
        },
    }
}

/// Resolve the `[F]` followup team context, reading `$CHAN_TAB_NAME` from the
/// process env and delegating the pure precedence to [`resolve_followup`] (so
/// the derivation is unit-testable without touching the environment, the same
/// split as `open_env` / `open_env_from`).
fn build_followup(
    followup_dir: Option<String>,
    from: Option<String>,
    to: Option<String>,
    tab_name: &Option<String>,
    tab_group: &Option<String>,
) -> Result<SurveyFollowup> {
    resolve_followup(
        followup_dir,
        std::env::var("CHAN_TAB_NAME").ok(),
        from,
        to,
        tab_name.clone(),
        tab_group.clone(),
    )
}

/// The pure followup-context precedence:
/// `from` <- `$CHAN_TAB_NAME` (fallback `--from`); `to` <- the survey target
/// (`--tab-name`, then `--tab-group`; fallback `--to`). `dir` comes straight
/// from `--followup-dir` (only called when that was passed). Bails with a clear
/// message if `dir`/`from`/`to` cannot be resolved, so a followup is always
/// well-named and team-scoped.
fn resolve_followup(
    followup_dir: Option<String>,
    env_tab_name: Option<String>,
    from: Option<String>,
    to: Option<String>,
    tab_name: Option<String>,
    tab_group: Option<String>,
) -> Result<SurveyFollowup> {
    let trimmed = |s: String| {
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    let dir = followup_dir
        .and_then(trimmed)
        .ok_or_else(|| anyhow::anyhow!("--followup-dir is required to write a follow-up file"))?;
    // from: the surveying agent's own tab, overridable with --from.
    let from = env_tab_name
        .and_then(trimmed)
        .or_else(|| from.and_then(trimmed))
        .ok_or_else(|| {
            anyhow::anyhow!("--followup-dir needs a `from`: set $CHAN_TAB_NAME or pass --from")
        })?;
    // to: --to is the explicit OVERRIDE (the common case: surveying via the
    // lead's tab on behalf of a host who has no live tab of their own, so the
    // followup is addressed to the host, not the lead's tab). Falls back to the
    // survey target (tab name / group), which is always present per the caller.
    let to = to
        .and_then(trimmed)
        .or_else(|| tab_name.and_then(trimmed))
        .or_else(|| tab_group.and_then(trimmed))
        .ok_or_else(|| {
            anyhow::anyhow!("--followup-dir needs a `to` target (--to / --tab-name / --tab-group)")
        })?;
    Ok(SurveyFollowup { dir, from, to })
}

/// Render the `cs terminal list` registry JSON
/// (`{groups: {group: [{name, session_id, cwd}]}}`) as a markdown table
/// grouped by terminal group. This is the default human output; `--json`
/// emits the raw payload instead. An empty registry yields a short line
/// rather than a blank table.
fn render_terminal_list_markdown(raw: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).context("parsing terminal list JSON")?;
    let groups = value
        .get("groups")
        .and_then(|g| g.as_object())
        .ok_or_else(|| anyhow::anyhow!("terminal list JSON missing `groups`"))?;
    if groups.is_empty() {
        return Ok("No live terminal sessions.\n".to_string());
    }
    let str_field = |s: &serde_json::Value, key: &str| {
        s.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("-")
            .to_string()
    };
    let mut out = String::new();
    for (group, sessions) in groups {
        out.push_str(&format!("## {group}\n\n"));
        out.push_str("| name | session | window | pane | tab | kind | status | cwd |\n");
        out.push_str("| --- | --- | --- | --- | --- | --- | --- | --- |\n");
        if let Some(arr) = sessions.as_array() {
            for s in arr {
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                    str_field(s, "name"),
                    str_field(s, "session_id"),
                    str_field(s, "window"),
                    str_field(s, "pane"),
                    str_field(s, "tab"),
                    str_field(s, "window_kind"),
                    str_field(s, "window_status"),
                    str_field(s, "cwd"),
                ));
            }
        }
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_markdown_converts_bold_highlight_and_locator() {
        let raw = r#"{"hits":[{"path":"a.md","start_line":3,"heading":"H","snippet":"the <b>fox</b> ran"}]}"#;
        let out = render_search_markdown(raw).expect("render");
        assert!(out.contains("- a.md:3 - H"), "locator: {out}");
        // <b>...</b> highlight -> markdown **bold**.
        assert!(out.contains("the **fox** ran"), "bold: {out}");
        assert!(!out.contains("<b>"), "no raw html: {out}");
    }

    #[test]
    fn terminal_list_markdown_empty_is_short_line() {
        let out = render_terminal_list_markdown(r#"{"groups":{}}"#).expect("render");
        assert_eq!(out, "No live terminal sessions.\n");
    }

    #[test]
    fn terminal_list_markdown_renders_window_columns() {
        let raw = r#"{"groups":{"default":[{"name":"probe","session_id":"s1","window":"w-abc","pane":"p-1","tab":"t-1","window_kind":"standalone-terminal","window_status":"alive","cwd":"/tmp"}]}}"#;
        let out = render_terminal_list_markdown(raw).expect("render");
        assert!(
            out.contains("| name | session | window | pane | tab | kind | status | cwd |"),
            "header: {out}"
        );
        assert!(
            out.contains("| probe | s1 | w-abc | p-1 | t-1 | standalone-terminal | alive | /tmp |"),
            "row: {out}"
        );
    }

    #[test]
    fn terminal_list_markdown_tolerates_a_pre_identity_server() {
        // A server that omits the window/pane/tab/kind/status fields renders `-`
        // in those columns rather than erroring.
        let raw = r#"{"groups":{"default":[{"name":"probe","session_id":"s1","cwd":"/tmp"}]}}"#;
        let out = render_terminal_list_markdown(raw).expect("render");
        assert!(
            out.contains("| probe | s1 | - | - | - | - | - | /tmp |"),
            "row: {out}"
        );
    }

    #[test]
    fn session_list_markdown_renders_participant_rows() {
        let raw = r#"[{"window_id":"w-abc","name":"alice","role":"leader","status":"live"},{"window_id":"w-def","name":"bob","role":"follower","status":"disconnecting"}]"#;
        let out = render_session_list_markdown(raw).expect("render");
        assert!(
            out.contains("| window | name | role | status |"),
            "header: {out}"
        );
        assert!(
            out.contains("| w-abc | alice | leader | live |"),
            "leader: {out}"
        );
        assert!(
            out.contains("| w-def | bob | follower | disconnecting |"),
            "follower: {out}"
        );
    }

    #[test]
    fn session_list_markdown_empty_is_short_line() {
        let out = render_session_list_markdown("[]").expect("render");
        assert_eq!(out, "No session participants.\n");
    }

    #[test]
    fn survey_timeout_flag_parses_and_defaults_to_600() {
        // Omitted: the baked-in default carries the window so the agent never
        // blocks forever, and default-vs-custom stays visible in the message.
        let cli = CsCli::parse_from(["cs", "terminal", "survey", "--tab-name", "@@Alex", "q"]);
        match cli.action {
            ShellAction::Terminal {
                action: TerminalAction::Survey { timeout, .. },
            } => assert_eq!(timeout, crate::wire::DEFAULT_SURVEY_TIMEOUT_SECS),
            other => panic!("expected survey, got {other:?}"),
        }
        // Explicit override is taken verbatim.
        let cli = CsCli::parse_from([
            "cs",
            "terminal",
            "survey",
            "--tab-name",
            "@@Alex",
            "--timeout",
            "30",
            "q",
        ]);
        match cli.action {
            ShellAction::Terminal {
                action: TerminalAction::Survey { timeout, .. },
            } => assert_eq!(timeout, 30),
            other => panic!("expected survey, got {other:?}"),
        }
    }

    #[test]
    fn classify_survey_result_maps_reply_timeout_and_error() {
        // A plain reply is the answered outcome (stdout, exit 0).
        match classify_survey_result(Ok("Ship it".into())).unwrap() {
            SurveyOutcome::Answered(m) => assert_eq!(m, "Ship it"),
            SurveyOutcome::TimedOut(m) => panic!("unexpected timeout: {m}"),
        }
        // The typed timeout error becomes the timed-out outcome (stderr, 124),
        // carrying the server's elapsed-window line verbatim.
        let timed_out = classify_survey_result(Err(crate::exit_code::ControlTimeout {
            message: "no reply within 30s".into(),
        }
        .into()))
        .unwrap();
        match timed_out {
            SurveyOutcome::TimedOut(m) => assert_eq!(m, "no reply within 30s"),
            SurveyOutcome::Answered(m) => panic!("expected timeout, got answer: {m}"),
        }
        // Any other error propagates unchanged (the generic exit-1 path).
        let err = classify_survey_result(Err(anyhow::anyhow!("connection refused"))).unwrap_err();
        assert!(err.to_string().contains("connection refused"));
    }

    #[test]
    fn parses_pane_query_json_pretty() {
        let cli = CsCli::parse_from(["cs", "pane", "--json", "--pretty"]);
        match cli.action {
            ShellAction::Pane {
                tab_name,
                json,
                pretty,
                action,
            } => {
                assert!(json);
                assert!(pretty);
                assert!(tab_name.is_none());
                assert!(action.is_none(), "bare cs pane is the query");
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn parses_pane_exec_subcommands_and_global_tab_name() {
        // --tab-name is global, so it works on a subcommand; focus carries
        // the pane id.
        let cli = CsCli::parse_from(["cs", "pane", "--tab-name", "@@Alice", "focus", "pane-1"]);
        match cli.action {
            ShellAction::Pane {
                tab_name,
                action: Some(PaneAction::Focus { pane_id }),
                ..
            } => {
                assert_eq!(tab_name.as_deref(), Some("@@Alice"));
                assert_eq!(pane_id, "pane-1");
            }
            other => panic!("unexpected parse: {other:?}"),
        }

        // split bottom --pane.
        let cli = CsCli::parse_from(["cs", "pane", "split", "bottom", "--pane", "pane-2"]);
        match cli.action {
            ShellAction::Pane {
                action: Some(PaneAction::Split { dir, pane }),
                ..
            } => {
                assert!(matches!(dir, SplitDirArg::Bottom));
                assert_eq!(pane.as_deref(), Some("pane-2"));
            }
            other => panic!("unexpected parse: {other:?}"),
        }

        // close-tab --force.
        let cli = CsCli::parse_from(["cs", "pane", "close-tab", "--force"]);
        match cli.action {
            ShellAction::Pane {
                action: Some(PaneAction::CloseTab { force, tab, pane }),
                ..
            } => {
                assert!(force);
                assert!(tab.is_none() && pane.is_none());
            }
            other => panic!("unexpected parse: {other:?}"),
        }

        // resize delta.
        let cli = CsCli::parse_from(["cs", "pane", "resize", "-0.1"]);
        match cli.action {
            ShellAction::Pane {
                action: Some(PaneAction::Resize { delta, .. }),
                ..
            } => assert!((delta - (-0.1)).abs() < 1e-9),
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn parses_window_lifecycle_subcommands() {
        // Full names.
        let cli = CsCli::parse_from(["cs", "window", "new"]);
        assert!(matches!(
            cli.action,
            ShellAction::Window {
                action: WindowAction::New
            }
        ));

        let cli = CsCli::parse_from(["cs", "window", "open", "terminal-win-2"]);
        match cli.action {
            ShellAction::Window {
                action: WindowAction::Open { id },
            } => assert_eq!(id, "terminal-win-2"),
            other => panic!("unexpected parse: {other:?}"),
        }

        // rm with and without --force.
        let cli = CsCli::parse_from(["cs", "window", "rm", "workspace-aa-0"]);
        match cli.action {
            ShellAction::Window {
                action: WindowAction::Rm { id, force },
            } => {
                assert_eq!(id, "workspace-aa-0");
                assert!(!force);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        let cli = CsCli::parse_from(["cs", "window", "rm", "--force", "terminal-win-1"]);
        match cli.action {
            ShellAction::Window {
                action: WindowAction::Rm { id, force },
            } => {
                assert_eq!(id, "terminal-win-1");
                assert!(force);
            }
            other => panic!("unexpected parse: {other:?}"),
        }

        let cli = CsCli::parse_from(["cs", "window", "hide", "terminal-win-3"]);
        assert!(matches!(
            cli.action,
            ShellAction::Window {
                action: WindowAction::Hide { .. }
            }
        ));
    }

    #[test]
    fn window_subcommand_prefixes_are_unambiguous() {
        // `infer_subcommands` resolves each verb from a unique prefix — a
        // regression here is a runtime break clap won't flag at compile
        // time. Note `hide` needs "hi": a bare "h" is ambiguous with the
        // auto-generated `help` subcommand, so it (correctly) does NOT
        // resolve to `hide`.
        type Case = (&'static str, fn(&WindowAction) -> bool);
        let cases: [Case; 5] = [
            ("l", |a| matches!(a, WindowAction::List { .. })),
            ("n", |a| matches!(a, WindowAction::New)),
            ("o", |a| matches!(a, WindowAction::Open { .. })),
            ("hi", |a| matches!(a, WindowAction::Hide { .. })),
            ("r", |a| matches!(a, WindowAction::Rm { .. })),
        ];
        for (prefix, check) in cases {
            // Each verb that needs args gets dummy ones; extras are ignored
            // by the variants that don't take them.
            let args = match prefix {
                "o" | "hi" | "r" => vec!["cs", "window", prefix, "id-0"],
                _ => vec!["cs", "window", prefix],
            };
            let cli = CsCli::try_parse_from(args)
                .unwrap_or_else(|e| panic!("`cs window {prefix}` failed to parse: {e}"));
            match cli.action {
                ShellAction::Window { action } => assert!(
                    check(&action),
                    "`cs window {prefix}` resolved wrong: {action:?}"
                ),
                other => panic!("unexpected parse for `cs window {prefix}`: {other:?}"),
            }
        }

        // A bare "h" is ambiguous (help vs hide); confirm it's rejected so
        // the comment above stays honest.
        assert!(CsCli::try_parse_from(["cs", "window", "h", "id-0"]).is_err());
    }

    #[test]
    fn upload_download_require_a_path_argument() {
        // PATH is required on both (no default form); a bare verb is a usage error.
        assert!(CsCli::try_parse_from(["cs", "upload"]).is_err());
        assert!(CsCli::try_parse_from(["cs", "download"]).is_err());
        // `.` (and any relative path) parses to the given path.
        match CsCli::try_parse_from(["cs", "upload", "."]).unwrap().action {
            ShellAction::Upload { path } => assert_eq!(path.to_str(), Some(".")),
            other => panic!("unexpected parse for `cs upload .`: {other:?}"),
        }
        match CsCli::try_parse_from(["cs", "download", "notes/a.md"])
            .unwrap()
            .action
        {
            ShellAction::Download { path } => assert_eq!(path.to_str(), Some("notes/a.md")),
            other => panic!("unexpected parse for `cs download notes/a.md`: {other:?}"),
        }
    }

    #[test]
    fn copy_parses_bare_and_with_mime_flags() {
        // A bare `cs copy` reads stdin and sniffs the type (no path arg).
        match CsCli::try_parse_from(["cs", "copy"]).unwrap().action {
            ShellAction::Copy { mime, html } => {
                assert_eq!(mime, None);
                assert!(!html);
            }
            other => panic!("unexpected parse for `cs copy`: {other:?}"),
        }
        match CsCli::try_parse_from(["cs", "copy", "--mime", "image/png"])
            .unwrap()
            .action
        {
            ShellAction::Copy { mime, html } => {
                assert_eq!(mime.as_deref(), Some("image/png"));
                assert!(!html);
            }
            other => panic!("unexpected parse for `cs copy --mime`: {other:?}"),
        }
        match CsCli::try_parse_from(["cs", "copy", "--html"])
            .unwrap()
            .action
        {
            ShellAction::Copy { html, .. } => assert!(html),
            other => panic!("unexpected parse for `cs copy --html`: {other:?}"),
        }
        // `--html` and `--mime` are mutually exclusive.
        assert!(CsCli::try_parse_from(["cs", "copy", "--html", "--mime", "text/html"]).is_err());
    }

    #[test]
    fn paste_parses_and_rejects_conflicting_prefer_flags() {
        match CsCli::try_parse_from(["cs", "paste"]).unwrap().action {
            ShellAction::Paste { text, html, image } => {
                assert!(!text && !html && !image);
            }
            other => panic!("unexpected parse for `cs paste`: {other:?}"),
        }
        match CsCli::try_parse_from(["cs", "paste", "--image"])
            .unwrap()
            .action
        {
            ShellAction::Paste { image, .. } => assert!(image),
            other => panic!("unexpected parse for `cs paste --image`: {other:?}"),
        }
        // The three representation flags are mutually exclusive.
        assert!(CsCli::try_parse_from(["cs", "paste", "--text", "--image"]).is_err());
        assert!(CsCli::try_parse_from(["cs", "paste", "--html", "--text"]).is_err());
    }

    #[test]
    fn pane_action_into_op_maps_each_variant() {
        assert!(matches!(
            PaneAction::Focus {
                pane_id: "p".into()
            }
            .into_op(),
            PaneOp::Focus { .. }
        ));
        assert!(matches!(
            PaneAction::CloseAll { force: true }.into_op(),
            PaneOp::CloseAll { force: true }
        ));
        // SplitDirArg maps to the wire SplitDir.
        match (PaneAction::Split {
            dir: SplitDirArg::Right,
            pane: None,
        })
        .into_op()
        {
            PaneOp::Split { dir, .. } => assert!(matches!(dir, SplitDir::Right)),
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn pane_exec_markdown_lists_blocked() {
        let raw = r#"{"ok":false,"summary":"closed 1, blocked 1","blocked":[
            {"tab":"notes.md","reason":"unsaved changes"}]}"#;
        let out = render_pane_exec_markdown(raw).expect("render");
        assert!(out.contains("closed 1, blocked 1"), "{out}");
        assert!(out.contains("- notes.md: unsaved changes"), "{out}");
    }

    #[test]
    fn pane_layout_markdown_renders_panes_tabs_and_flags() {
        let raw = r#"{
            "activePaneId": "p1",
            "panes": [
                { "id": "p1", "active": true, "activeTabId": "t3", "tabs": [
                    { "id": "t3", "kind": "file", "title": "notes.md", "active": true, "dirty": true },
                    { "id": "t4", "kind": "terminal", "title": "@@Alice", "live": true }
                ] },
                { "id": "p2", "active": false, "activeTabId": null, "tabs": [] }
            ]
        }"#;
        let out = render_pane_layout_markdown(raw).expect("render");
        // Active pane is flagged; the inactive one is not.
        assert!(out.contains("## pane p1 (active)"), "active heading: {out}");
        assert!(
            out.contains("## pane p2\n") && !out.contains("## pane p2 (active)"),
            "inactive heading: {out}"
        );
        // The active tab carries the `*` marker; flags carry dirty + live.
        assert!(out.contains("| t3* | file | notes.md | dirty |"), "{out}");
        assert!(out.contains("| t4 | terminal | @@Alice | live |"), "{out}");
        // An empty pane renders `(empty)`, not a header-only table.
        assert!(out.contains("(empty)"), "empty pane: {out}");
    }

    #[test]
    fn pane_layout_markdown_empty_is_short_line() {
        let out = render_pane_layout_markdown(r#"{"activePaneId":"","panes":[]}"#).expect("render");
        assert_eq!(out, "No panes.\n");
    }

    #[test]
    fn parses_terminal_close_by_name_or_group() {
        let cli = CsCli::parse_from(["cs", "terminal", "close", "--tab-name", "@@Alice"]);
        match cli.action {
            ShellAction::Terminal {
                action:
                    TerminalAction::Close {
                        tab_name,
                        tab_group,
                    },
            } => {
                assert_eq!(tab_name.as_deref(), Some("@@Alice"));
                assert_eq!(tab_group, None);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        // --tab-group is accepted too (whole-group teardown). The
        // "needs a selector" guard is a dispatch-time bail (like restart),
        // not a parse error.
        let cli = CsCli::parse_from(["cs", "terminal", "close", "--tab-group", "chan-team"]);
        match cli.action {
            ShellAction::Terminal {
                action: TerminalAction::Close { tab_group, .. },
            } => assert_eq!(tab_group.as_deref(), Some("chan-team")),
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn parses_terminal_team_new_with_config_and_script() {
        let cli = CsCli::parse_from([
            "cs",
            "terminal",
            "team",
            "new",
            "alpha",
            "--config",
            "spec.toml",
            "--script",
        ]);
        match cli.action {
            ShellAction::Terminal {
                action:
                    TerminalAction::Team {
                        action:
                            TeamAction::New {
                                dir,
                                config,
                                stdin,
                                brief,
                                mcp_env,
                                script,
                                template: _,
                            },
                    },
            } => {
                assert_eq!(dir, "alpha");
                assert_eq!(config.as_deref(), Some(std::path::Path::new("spec.toml")));
                assert!(!stdin);
                // Omitting --brief leaves it unset (the generic bootstrap).
                assert_eq!(brief, None);
                // Omitting --mcp-env leaves the field unset (server default OFF).
                assert_eq!(mcp_env, None);
                assert!(script);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn parses_terminal_team_new_mcp_env_on_off() {
        let on = CsCli::parse_from([
            "cs",
            "terminal",
            "team",
            "new",
            "alpha",
            "--stdin",
            "--mcp-env",
            "on",
        ]);
        match on.action {
            ShellAction::Terminal {
                action:
                    TerminalAction::Team {
                        action: TeamAction::New { mcp_env, .. },
                    },
            } => assert_eq!(mcp_env, Some(McpEnvToggle::On)),
            other => panic!("unexpected parse: {other:?}"),
        }
        let off = CsCli::parse_from([
            "cs",
            "terminal",
            "team",
            "new",
            "alpha",
            "--stdin",
            "--mcp-env",
            "off",
        ]);
        match off.action {
            ShellAction::Terminal {
                action:
                    TerminalAction::Team {
                        action: TeamAction::New { mcp_env, .. },
                    },
            } => assert_eq!(mcp_env, Some(McpEnvToggle::Off)),
            other => panic!("unexpected parse: {other:?}"),
        }
        // Only on|off parse; a bogus value is a clap error, not a silent miss.
        assert!(CsCli::try_parse_from([
            "cs",
            "terminal",
            "team",
            "new",
            "alpha",
            "--stdin",
            "--mcp-env",
            "maybe",
        ])
        .is_err());
    }

    #[test]
    fn set_team_mcp_env_sets_root_key_before_members() {
        let input = "team_name = \"alpha\"\nhost_handle = \"@@Neo\"\n\n\
                     [[members]]\nhandle = \"@@Lead\"\ncommand = \"claude\"\nis_lead = true\n";
        // ON injects mcp_env = true at the root; the member table is preserved
        // and (per TOML) still serializes after the root scalar keys, so the
        // server parses it back into TeamConfig.mcp_env.
        let on: toml::Table = set_team_mcp_env(input, true).unwrap().parse().unwrap();
        assert_eq!(on.get("mcp_env"), Some(&toml::Value::Boolean(true)));
        assert!(on.get("members").and_then(|m| m.as_array()).is_some());
        // OFF writes it explicitly.
        let off: toml::Table = set_team_mcp_env(input, false).unwrap().parse().unwrap();
        assert_eq!(off.get("mcp_env"), Some(&toml::Value::Boolean(false)));
    }

    #[test]
    fn set_team_mcp_env_overrides_existing_value() {
        // An input that already turned it on is overridden by --mcp-env off.
        let input = "team_name = \"a\"\nmcp_env = true\n\n[[members]]\n\
                     handle = \"@@L\"\ncommand = \"claude\"\nis_lead = true\n";
        let out: toml::Table = set_team_mcp_env(input, false).unwrap().parse().unwrap();
        assert_eq!(out.get("mcp_env"), Some(&toml::Value::Boolean(false)));
    }

    #[test]
    fn parses_terminal_scrollback_tab_name() {
        let cli = CsCli::parse_from(["cs", "terminal", "scrollback", "--tab-name", "@@Alice"]);
        match cli.action {
            ShellAction::Terminal {
                action: TerminalAction::Scrollback { tab_name },
            } => assert_eq!(tab_name, "@@Alice"),
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn terminal_scrollback_requires_tab_name() {
        // `--tab-name` is a required clap arg (the field is a plain String),
        // so omitting it is a parse error, not a runtime one.
        assert!(CsCli::try_parse_from(["cs", "terminal", "scrollback"]).is_err());
    }

    #[test]
    fn parses_terminal_team_load_script() {
        let cli = CsCli::parse_from(["cs", "terminal", "team", "load", "alpha", "--script"]);
        match cli.action {
            ShellAction::Terminal {
                action:
                    TerminalAction::Team {
                        action: TeamAction::Load { dir, script },
                    },
            } => {
                assert_eq!(dir, "alpha");
                assert!(script);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn resolve_team_dir_joins_relative_against_cwd_under_workspace() {
        // A bare name resolves cwd-relative within the workspace; "." is the
        // cwd's own workspace-relative path; a "../" normalizes lexically.
        // Synthetic non-existent paths exercise the canonicalize fallback, so
        // the test is filesystem-free and deterministic.
        assert_eq!(
            resolve_team_dir_in("alpha", Some("/ws"), Path::new("/ws/a/b")).unwrap(),
            "a/b/alpha"
        );
        assert_eq!(
            resolve_team_dir_in(".", Some("/ws"), Path::new("/ws/teams/x")).unwrap(),
            "teams/x"
        );
        assert_eq!(
            resolve_team_dir_in("../y", Some("/ws"), Path::new("/ws/teams/x")).unwrap(),
            "teams/y"
        );
    }

    #[test]
    fn resolve_team_dir_accepts_absolute_under_workspace_and_keeps_root_name() {
        assert_eq!(
            resolve_team_dir_in("/ws/teams/alpha", Some("/ws"), Path::new("/ws/elsewhere"))
                .unwrap(),
            "teams/alpha"
        );
        // A bare name at the workspace root stays that name.
        assert_eq!(
            resolve_team_dir_in("alpha", Some("/ws"), Path::new("/ws")).unwrap(),
            "alpha"
        );
    }

    #[test]
    fn resolve_team_dir_rejects_outside_workspace_and_bare_root() {
        // Escapes the workspace -> error.
        assert!(resolve_team_dir_in("/etc", Some("/ws"), Path::new("/ws")).is_err());
        assert!(resolve_team_dir_in("../../etc", Some("/ws"), Path::new("/ws")).is_err());
        // "." at the root resolves to the workspace root itself -> error (a
        // team needs a subdirectory; the server rejects an empty dir too).
        assert!(resolve_team_dir_in(".", Some("/ws"), Path::new("/ws")).is_err());
        assert!(resolve_team_dir_in("   ", Some("/ws"), Path::new("/ws")).is_err());
    }

    #[test]
    fn resolve_team_dir_passes_through_without_a_workspace_env() {
        // Outside a chan terminal ($CHAN_WORKSPACE_PATH unset) the dir is sent
        // verbatim, preserving the prior workspace-relative contract.
        assert_eq!(
            resolve_team_dir_in("teams/alpha", None, Path::new("/anywhere")).unwrap(),
            "teams/alpha"
        );
    }

    #[test]
    fn team_config_input_requires_exactly_one_source() {
        // Both sources -> error; neither -> error. (The single-source happy
        // paths read a file / stdin, exercised end-to-end by the handler.)
        assert!(read_team_config_input(Some("a.toml".into()), true).is_err());
        assert!(read_team_config_input(None, false).is_err());
    }

    #[test]
    fn resolve_followup_to_flag_overrides_tab_name() {
        // from <- $CHAN_TAB_NAME (over --from). to <- --to OVERRIDE (over
        // --tab-name/--tab-group): the common case of surveying via the lead's
        // tab on behalf of a host with no live tab, so the followup is
        // addressed to the host, not the lead's tab.
        let f = resolve_followup(
            Some("team-a".into()),
            Some("Alice".into()),        // env_tab_name -> from
            Some("ignored-from".into()), // --from (ignored; env wins)
            Some("@@Host".into()),       // --to -> to (overrides --tab-name)
            Some("Bob".into()),          // --tab-name (overridden by --to)
            Some("group-x".into()),
        )
        .expect("resolve");
        assert_eq!(f.dir, "team-a");
        assert_eq!(f.from, "Alice");
        assert_eq!(f.to, "@@Host");
    }

    #[test]
    fn resolve_followup_falls_back_to_from_flag_and_group_then_to_flag() {
        // No env tab name -> --from. No tab name -> --tab-group for `to`.
        let f = resolve_followup(
            Some("team-a".into()),
            None,
            Some("flag-from".into()),
            None,
            None,
            Some("group-x".into()),
        )
        .expect("resolve");
        assert_eq!(f.from, "flag-from");
        assert_eq!(f.to, "group-x");

        // No tab name and no group -> --to fallback.
        let f = resolve_followup(
            Some("team-a".into()),
            Some("Alice".into()),
            None,
            Some("flag-to".into()),
            None,
            None,
        )
        .expect("resolve");
        assert_eq!(f.to, "flag-to");
    }

    #[test]
    fn resolve_followup_requires_dir_from_and_to() {
        // Missing / blank dir.
        assert!(resolve_followup(
            Some("  ".into()),
            Some("Alice".into()),
            None,
            None,
            Some("Bob".into()),
            None,
        )
        .is_err());
        // No from anywhere.
        assert!(resolve_followup(
            Some("team-a".into()),
            None,
            None,
            None,
            Some("Bob".into()),
            None,
        )
        .is_err());
        // No to anywhere.
        assert!(resolve_followup(
            Some("team-a".into()),
            Some("Alice".into()),
            None,
            None,
            None,
            None,
        )
        .is_err());
    }
}
