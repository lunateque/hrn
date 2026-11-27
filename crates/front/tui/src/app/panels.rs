//! The informational commands: `/settings`, `/context`, `/tools`, `/mcp`, `/jobs`, `/todo`,
//! `/pin`/`/unpin`/`/delete`/`/rename`, `/retry`, `/copy`, `/hotkeys` and the `Alt+M` model
//! list. Each renders into a scrollable view (`crate::views`) or acts on the store.

use super::App;
use crate::port::{ContextBreakdown, ToolInfo};
use crate::transcript::Level;
use crate::views;
use std::fmt::Write;

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the epoch, as the store stamps them.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// `(label, value)` lines, padded to one column.
pub(crate) fn table(rows: &[(&str, String)]) -> String {
    use std::fmt::Write;
    let width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (label, value) in rows {
        let _ = writeln!(out, "{label:<width$}  {value}");
    }
    out
}

impl App {
    /// Open a text page view.
    pub(super) fn open_view(&mut self, title: &str, text: &str) {
        self.view = Some(views::View::Text(views::TextView::new(
            title.to_owned(),
            text,
        )));
    }

    /// `/hotkeys` (and `Ctrl+H`): the merged keymap, from the embedded defaults plus the user's
    /// `~/.harness/keybindings.toml`.
    pub(super) fn open_hotkeys(&mut self) {
        let width = self
            .keymap
            .rows()
            .iter()
            .map(|(id, _, _)| id.len())
            .max()
            .unwrap_or(0);
        let chord_width = self
            .keymap
            .rows()
            .iter()
            .map(|(_, chords, _)| chords.len())
            .max()
            .unwrap_or(0);
        let target = self.appearance.keymap_paths.last().map_or_else(
            || "keybindings.toml".to_owned(),
            |p| p.display().to_string(),
        );
        let mut text = String::new();
        let _ = writeln!(text, "Remap any action in {target}");
        let _ = writeln!(
            text,
            "  \"app.retry\" = \"F5\"  ·  \"app.model.select\" = [\"Alt+M\", \"Ctrl+B\"]  ·  \"app.history.search\" = []\n"
        );
        for (id, chords, help) in self.keymap.rows() {
            let _ = writeln!(
                text,
                "{id:<width$}  {:<chord_width$}  {help}",
                chords.replace(" · ", ", ")
            );
        }
        self.open_view("key bindings", &text);
    }

    /// `/context`: where the window is going — system prompt, tool schemas, conversation.
    pub(super) fn context_report(&self) -> String {
        let ContextBreakdown {
            system_tokens,
            schema_tokens,
            message_tokens,
            window,
            last_prompt_tokens,
            nodes,
        } = self.backend.context_breakdown();
        let used = last_prompt_tokens.unwrap_or(system_tokens + schema_tokens + message_tokens);
        let pct = if window > 0 {
            format!("{}%", used.saturating_mul(100) / window.max(1))
        } else {
            "?".to_owned()
        };
        let mut text = String::new();
        text.push_str(&table(&[
            ("model", self.settings.model.clone()),
            ("context window", crate::render::tokens(window)),
            (
                "used now",
                format!("{} ({pct})", crate::render::tokens(used)),
            ),
            (
                "last request",
                last_prompt_tokens.map_or_else(|| "—".to_owned(), crate::render::tokens),
            ),
            ("system prompt", crate::render::tokens(system_tokens)),
            ("tool schemas", crate::render::tokens(schema_tokens)),
            ("conversation", crate::render::tokens(message_tokens)),
            ("branch nodes", nodes.to_string()),
            ("thinking", self.settings.thinking.as_str().to_owned()),
        ]));
        text.push_str("\nToken figures are estimates unless `last request` shows a real count.\n");
        text.push_str(
            "The gauge in the status band tracks the last real request; /compact folds old turns.",
        );
        text
    }

    /// `/tools`: everything the session can call, active and deferred.
    pub(super) fn tools_report(&self) -> String {
        let tools: Vec<ToolInfo> = self.backend.tools();
        if tools.is_empty() {
            return "no tools are registered in this backend".to_owned();
        }
        let active = tools.iter().filter(|t| !t.deferred).count();
        let mut text = String::new();
        let _ = write!(
            text,
            "{active} active · {} deferred\n\n",
            tools.len() - active
        );
        let width = tools
            .iter()
            .map(|t| t.name.chars().count())
            .max()
            .unwrap_or(0);
        for tool in &tools {
            let slot = if tool.deferred { "deferred" } else { "active" };
            let _ = writeln!(text, "{:<width$}  {:9}  {}", tool.name, slot, tool.effect);
        }
        text.push_str("\ndeferred tools ride behind `tool_search` until activated");
        text
    }

    /// `/mcp [resources]`: servers and state, or advertised `mcp://` resources.
    pub(super) fn handle_mcp(&mut self, arg: Option<&str>) {
        match arg.map(str::trim).filter(|s| !s.is_empty()) {
            Some(sub) if sub.eq_ignore_ascii_case("resources") => {
                let text = self.mcp_resources_report();
                self.open_view("mcp resources", &text);
            }
            None => {
                let text = self.mcp_report();
                self.open_view("mcp", &text);
            }
            Some(other) => {
                self.notice(
                    Level::Warn,
                    format!("unknown /mcp subcommand `{other}` · use `/mcp` or `/mcp resources`"),
                );
            }
        }
    }

    /// `/mcp`: configured servers, whether the session holds a live connection, tool counts.
    pub(super) fn mcp_report(&self) -> String {
        let servers = self.backend.mcp_status();
        if servers.is_empty() {
            return "no MCP servers are configured ([mcp] in config.toml, .mcp.json)".to_owned();
        }
        let mut text = String::new();
        for server in &servers {
            let state = match &server.note {
                Some(problem) => format!("error: {problem}"),
                None if !server.enabled => "disabled".to_owned(),
                None if server.connected => "connected".to_owned(),
                None => "not connected".to_owned(),
            };
            let _ = writeln!(
                text,
                "{}  {state}  ·  {} tools ({} deferred)\n    {} · {}",
                server.name, server.tools, server.deferred, server.transport, server.source
            );
        }
        text.push_str(
            "\ndeferred MCP tools activate through `tool_search` · `/mcp resources` lists mcp://",
        );
        text
    }

    /// `/mcp resources`: advertised MCP resources addressable via `read mcp://<uri>`.
    pub(super) fn mcp_resources_report(&self) -> String {
        let resources = self.backend.mcp_resources();
        if resources.is_empty() {
            return "no MCP resources are advertised by configured servers".to_owned();
        }
        let mut text = String::new();
        for res in &resources {
            let _ = write!(text, "mcp://{}  [{}]", res.uri, res.server);
            if !res.name.is_empty() && res.name != res.uri {
                let _ = write!(text, "  {}", res.name);
            }
            if let Some(mime) = &res.mime_type {
                let _ = write!(text, "  ({mime})");
            }
            text.push('\n');
            if let Some(desc) = &res.description
                && !desc.trim().is_empty()
            {
                let _ = writeln!(text, "    {}", desc.trim());
            }
        }
        text.push_str("\nread any resource with `read mcp://<uri>`");
        text
    }

    /// `/jobs`: background work addressable under `proc://`.
    pub(super) fn jobs_report(&self) -> String {
        let jobs = self.backend.jobs();
        if jobs.is_empty() {
            return "no background jobs".to_owned();
        }
        let now = now_ms();
        let mut text = String::new();
        for job in &jobs {
            let state = if job.running {
                let secs = (now - job.started_ms).max(0) / 1000;
                format!("running {:>4}", fmt_age(secs))
            } else {
                let secs = (job.finished_ms - job.started_ms).max(0) / 1000;
                format!("done   {:>4}", fmt_age(secs))
            };
            let _ = writeln!(
                text,
                "proc://{}  {state}  {}\n    {}",
                job.id,
                if job.waitable { "(service)" } else { "" },
                crate::toolfmt::one_line(&job.line)
            );
        }
        text.push_str("\nread output with `read proc://<id>`");
        text
    }

    /// `/todo [edit]`: the checklist as markdown; `edit` drops it into the prompt for editing,
    /// and sending the prompt commits it as a new board node.
    pub(super) fn handle_todo(&mut self, arg: Option<&str>) {
        if arg == Some("edit") {
            let phases = if self.todo.is_empty() {
                self.backend.todos()
            } else {
                self.todo.clone()
            };
            let markdown = hrn_kernel::todo::to_markdown(&phases);
            self.editor.set(&markdown);
            self.todo_editing = true;
            self.flash("edit the checklist and send to apply (/todo edit)");
            return;
        }
        let phases = if self.todo.is_empty() {
            self.backend.todos()
        } else {
            self.todo.clone()
        };
        if phases.is_empty() {
            self.notice(
                Level::Info,
                "the todo list is empty · the model fills it with `todo`",
            );
            return;
        }
        let text = hrn_kernel::todo::to_markdown(&phases);
        self.open_view("todo", &text);
    }

    /// Commit an edited checklist (`/todo edit` sent back to the app).
    pub(super) fn apply_todo_edit(&mut self, markdown: &str) -> Result<(), String> {
        let phases = hrn_kernel::todo::from_markdown(markdown);
        self.backend
            .set_todos(phases.clone())
            .map_err(|e| e.to_string())?;
        self.todo = phases;
        Ok(())
    }

    /// `/pin` / `/unpin`, and the picker's `alt+k`: pin a session to the top of `/sessions`.
    pub(super) fn toggle_pin(&mut self, session: Option<&str>, pinned: bool) {
        match self.backend.set_pinned(session, pinned) {
            Ok(()) => {
                if let (Some(id), Some(picker)) = (session, self.picker.as_mut()) {
                    picker.set_pinned(id, pinned);
                }
                self.flash(if pinned {
                    "session pinned"
                } else {
                    "session unpinned"
                });
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    /// `/delete [session]` and the picker's Backspace: remove a session and its nodes.
    pub(super) fn delete_session_prompt(&mut self, session: Option<&str>) {
        let Some(id) = self.resolve_session(session) else {
            self.notice(
                Level::Warn,
                "no session selected · /delete <id> or pick one in /sessions",
            );
            return;
        };
        if self.pending_delete.as_deref() != Some(id.as_str()) {
            self.pending_delete = Some(id.clone());
            self.flash("delete this session? press again to confirm");
            return;
        }
        self.pending_delete = None;
        match self.backend.delete_session(&id) {
            Ok(true) => {
                self.notice(Level::Info, format!("deleted session {id}"));
                if let Some(picker) = self.picker.as_mut() {
                    picker.remove(&id);
                }
                if session.is_none() {
                    self.backend.new_session();
                    self.transcript.clear();
                    self.restart_screen(true);
                    self.title = None;
                }
            }
            Ok(false) => self.notice(Level::Warn, format!("no such session {id}")),
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    /// `/rename [title]`: title the current session, or regenerate via the title model when omitted.
    pub(super) fn handle_rename(&mut self, title: Option<&str>) {
        let Some(title) = title.map(str::trim).filter(|t| !t.is_empty()) else {
            match self.backend.regenerate_title() {
                Ok(()) => self.flash("regenerating session title…"),
                Err(err) => self.notice(Level::Error, err.to_string()),
            }
            return;
        };
        match self.backend.rename_session(None, title) {
            Ok(()) => {
                self.title = Some(title.to_owned());
                self.flash("session renamed");
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    /// `I` inside `/sessions` picker: open the read-only Claude/Codex import sub-view.
    pub(super) fn open_import_picker(&mut self) {
        match self.backend.discover_external_sessions() {
            Ok(items) => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.enter_import(items);
                }
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    /// Enter inside `/sessions` import sub-view: import the external session and switch to it.
    pub(super) fn import_external_session(&mut self, source: &str, id: &str) {
        self.picker = None;
        match self.backend.import_external_session(source, id) {
            Ok((meta, nodes)) => {
                self.show_session(&meta, &nodes);
                self.backend.apply_settings(&self.settings);
                let title = self.title.clone().unwrap_or_else(|| meta.id.clone());
                self.flash(format!("imported from {source} · {title}"));
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    /// `F5` / `/retry`: run the last prompt again.
    pub(super) fn retry_last_turn(&mut self) {
        if self.is_busy() {
            self.notice(Level::Warn, "the agent is working · esc to interrupt first");
            return;
        }
        match &self.last_prompt {
            Some(prompt) => {
                let prompt = prompt.clone();
                self.send_prompt(&prompt, None);
            }
            None => self.notice(Level::Warn, "nothing to retry yet"),
        }
    }

    /// `/copy` / `Alt+Shift+C`: the last assistant answer onto the clipboard.
    pub(super) fn copy_last_answer(&mut self) {
        match self.transcript.last_assistant_text() {
            Some((_, text)) => {
                let text = text.to_owned();
                if crate::clipboard::write_text(&text) {
                    let lines = text.lines().count();
                    self.flash(format!("copied last answer ({lines} lines)"));
                } else {
                    self.notice(Level::Error, "clipboard unavailable".to_owned());
                }
            }
            None => self.notice(Level::Warn, "no answer to copy yet"),
        }
    }

    /// `Ctrl+G`: hand the draft to `$VISUAL`/`$EDITOR`, notepad as the Windows fallback.
    pub(super) fn open_external_editor(&mut self) {
        let draft = self.editor.text();
        match crate::exted::edit(&draft) {
            Ok(text) => {
                let trimmed = text.trim_end().to_owned();
                self.editor.set(&trimmed);
                self.flow.replay = true;
                self.flash("draft edited externally");
            }
            Err(err) => {
                self.flow.replay = true;
                self.notice(Level::Error, err);
            }
        }
    }

    /// The session `/delete` should act on: the argument, else the current one.
    fn resolve_session(&self, session: Option<&str>) -> Option<String> {
        session
            .map(str::to_owned)
            .or_else(|| self.backend.current_session_id())
    }
}

/// `95` → `1m35s` job ages.
fn fmt_age(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}
