//! Keyboard input. Priority: `ctrl+c`, the session picker, a pending approval, global
//! keys, the completion popup, and finally the editor.

use super::{App, ESC_DOUBLE_WINDOW, QUIT_WINDOW};
use crate::ask::AskAction;
use crate::imgview;
use crate::picker::Action;
use crate::port::DoubleEscapeAction;
use crate::slash::{self, Choice};
use crate::transcript::Level;
use crate::tree::TreeMode;
use crate::vim::{VimAction, VimMode};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use std::time::Instant;

fn is_press(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind != KeyEventKind::Release)
}

fn burst_plain_text(events: &[Event]) -> Option<String> {
    if events.len() < 4 {
        return None;
    }
    let mut out = String::with_capacity(events.len());
    for event in events {
        let Event::Key(key) = event else {
            return None;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        let mods = key.modifiers - KeyModifiers::SHIFT;
        if !mods.is_empty() {
            return None;
        }
        let KeyCode::Char(c) = key.code else {
            return None;
        };
        out.push(c);
    }
    Some(out)
}

impl App {
    pub(super) fn on_terminal(&mut self, events: Vec<Event>) {
        // On Windows consoles without bracketed paste, dragging or pasting an image file path
        // arrives as a single burst of character keys: attach it as `[Image #N]` automatically.
        if self.login.is_none()
            && self.logout.is_none()
            && self.setup.is_none()
            && self.model_hub.is_none()
            && self.settings_ui.is_none()
            && self.agent_hub.is_none()
            && self.agents_hub.is_none()
            && self.local_picker.is_none()
            && self.link_hints.is_none()
            && self.picker.is_none()
            && self.tree.is_none()
            && self.btw.is_none()
            && self.image_modal.is_none()
            && self.remote_qr.is_none()
            && self.approvals.is_empty()
            && self.active_ask.is_none()
            && let Some(burst_text) = burst_plain_text(&events)
            && let Some(path) = imgview::unquote_image_path(&burst_text, &self.cwd)
        {
            self.attach_image_path(&path);
            return;
        }
        let mut events = events.into_iter().peekable();
        while let Some(event) = events.next() {
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    // Without bracketed paste (the Windows console), a paste arrives as a burst
                    // of keys: an Enter followed by more keys in the same read is a newline.
                    let burst = events.peek().is_some_and(is_press);
                    self.on_key(key, burst);
                }
                Event::Mouse(ev) if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) => {
                    self.on_mouse_click(ev.row);
                }
                Event::FocusGained => self.notify.set_focused(true),
                Event::FocusLost => self.notify.set_focused(false),
                Event::Paste(text) => {
                    if let Some(login) = &mut self.login {
                        login.paste(&text);
                    } else if self.logout.is_none()
                        && self.setup.is_none()
                        && self.model_hub.is_none()
                        && self.settings_ui.is_none()
                        && self.agent_hub.is_none()
                        && self.agents_hub.is_none()
                        && self.picker.is_none()
                        && self.tree.is_none()
                        && self.btw.is_none()
                        && self.image_modal.is_none()
                        && self.remote_qr.is_none()
                        && self.approvals.is_empty()
                        && self.active_ask.is_none()
                    {
                        if text.is_empty() {
                            self.paste_clipboard();
                        } else if let Some(path) = imgview::unquote_image_path(&text, &self.cwd) {
                            self.attach_image_path(&path);
                        } else {
                            self.vim.paste_text_raw(&mut self.editor, &text);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent, burst: bool) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            self.ctrl_c();
            return;
        }
        if self.quit_armed.take().is_some() {
            self.flash = None;
        }
        if key.code != KeyCode::Esc && self.esc_armed.take().is_some() {
            self.flash = None;
        }
        if key.code != KeyCode::Left && self.left_armed.take().is_some() {
            self.flash = None;
        }
        if self.overlay_key(key, burst) {
            return;
        }
        if !self.approvals.is_empty() {
            if !self.on_approval_key(key) {
                // Never allow mode cycling or plan toggling while an approval dock is open (blocks
                // bypassing the 500 ms guard or accidentally switching to yolo via Ctrl+Y).
                if matches!(
                    self.keymap.action(key),
                    Some("app.mode.cycle" | "app.plan.toggle")
                ) {
                    self.flash("answer the pending approval before switching mode");
                } else {
                    self.global_key(key);
                }
            }
            return;
        }
        if let Some((panel, _)) = &mut self.active_ask {
            match panel.key(key, Instant::now()) {
                AskAction::Stay => {}
                AskAction::Submit(submission) => {
                    if let Some((_, prompt)) = self.active_ask.take() {
                        prompt.answer(Some(submission));
                    }
                    self.advance_ask();
                }
                AskAction::Cancel => {
                    if let Some((_, prompt)) = self.active_ask.take() {
                        prompt.answer(None);
                    }
                    self.advance_ask();
                }
            }
            return;
        }
        // Escape precedence: popup dismiss -> Vim Insert/Visual/Replace->Normal -> cancel pending
        // Vim operator/count -> app Escape behavior (interrupt or double-Esc).
        if key.code == KeyCode::Esc {
            if !self.completions().is_empty() {
                self.popup.dismissed_for = Some(self.editor.text());
                return;
            }
            if self.vim.handle_key(key, &mut self.editor) == VimAction::Consumed {
                return;
            }
        }
        // In Vim Normal mode, Ctrl+R performs Vim redo instead of app.history.search.
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl
            && !alt
            && self.vim.is_enabled()
            && self.vim.mode() == VimMode::Normal
            && matches!(key.code, KeyCode::Char('r' | 'R'))
            && self.vim.handle_key(key, &mut self.editor) == VimAction::Consumed
        {
            return;
        }
        // Ctrl chords, Enter, and Tab always cancel pending Vim operators/counts and run normally.
        if (ctrl && !alt) || matches!(key.code, KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab) {
            self.vim.cancel_pending();
        }
        if self.global_key(key) || self.popup_key(key) {
            return;
        }
        self.editor_key(key, burst);
    }

    /// Keys the overlays answer to before the prompt sees anything: a slash-command view, the
    /// history search, the login form, the image modal, the session picker, the tree. `true`
    /// when an overlay consumed the key.
    fn overlay_key(&mut self, key: KeyEvent, burst: bool) -> bool {
        if self.commit_modal_key(key)
            || self.git_ui_key(key)
            || self.agent_hub_key(key)
            || self.agents_hub_key(key)
            || self.model_hub_key(key)
            || self.settings_ui_key(key)
            || self.local_picker_key(key)
            || self.link_hints_key(key)
            || self.remote_qr_key(key)
        {
            return true;
        }
        if let Some(view) = &mut self.view {
            match view.key(key, self.overlay_area) {
                crate::views::ViewAction::Stay => {}
                crate::views::ViewAction::Close => self.view = None,
            }
            return true;
        }
        if let Some(search) = &mut self.history {
            match search.key(key) {
                crate::history::Action::Stay => {}
                crate::history::Action::Close => self.history = None,
                crate::history::Action::Use(prompt) => {
                    self.history = None;
                    self.editor.set(&prompt);
                }
            }
            return true;
        }
        if self.login_key(key, burst) || self.logout_key(key) || self.setup_key(key) {
            return true;
        }
        if self.image_modal.is_some() {
            if imgview::ImageModal::closes_on(key) {
                self.image_modal = None;
            }
            return true;
        }
        if self.btw_key(key) {
            return true;
        }
        if self.picker.is_some() {
            self.picker_key(key);
            return true;
        }
        self.handle_tree_key(key)
    }

    /// The session picker: navigation plus the pin/delete/import chords.
    fn picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        if !picker.in_import_view() {
            if self.keymap.is("app.session.pin", key) {
                if let Some(id) = picker.selected_id() {
                    self.toggle_pin(Some(&id), true);
                }
                return;
            }
            if self.keymap.is("app.session.unpin", key) {
                if let Some(id) = picker.selected_id() {
                    self.toggle_pin(Some(&id), false);
                }
                return;
            }
            if self.keymap.is("app.session.delete", key) {
                if let Some(id) = picker.selected_id() {
                    self.delete_session_prompt(Some(&id));
                }
                return;
            }
        }
        match picker.key(key) {
            Action::Stay => {}
            Action::Close => self.picker = None,
            Action::Resume(id) => {
                self.picker = None;
                self.resume(&id);
            }
            Action::OpenImport => self.open_import_picker(),
            Action::Import { source, id } => self.import_external_session(&source, &id),
        }
    }

    fn ctrl_c(&mut self) {
        let now = Instant::now();
        if self
            .quit_armed
            .is_some_and(|at| now.duration_since(at) < QUIT_WINDOW)
        {
            self.quit = true;
            return;
        }
        if self
            .login
            .as_ref()
            .is_some_and(crate::login::LoginView::is_oauth_active)
        {
            self.backend.cancel_login();
        }
        self.login = None;
        self.logout = None;
        self.setup = None;
        self.model_hub = None;
        if self
            .settings_ui
            .take()
            .is_some_and(|ui| ui.is_previewing_theme())
        {
            self.apply_appearance(self.appearance.clone(), false);
        }
        self.agent_hub = None;
        self.agents_hub = None;
        self.local_picker = None;
        self.link_hints = None;
        self.picker = None;
        self.tree = None;
        self.btw = None;
        self.image_modal = None;
        self.commit_modal = None;
        self.git_ui = None;
        self.remote_qr = None;
        self.esc_armed = None;
        self.left_armed = None;
        self.vim.reset_for_new_draft();
        if self.editor.is_blank() {
            self.flash("press ctrl+c again to quit");
        } else {
            self.editor
                .clear_draft(self.appearance.recall_cleared_drafts);
            self.flash("input cleared · ctrl+c again to quit");
        }
        self.quit_armed = Some(now);
    }

    fn global_key(&mut self, key: KeyEvent) -> bool {
        if let Some(action) = self.keymap.global_action(key) {
            self.dispatch_global_action(action);
            return true;
        }
        if key.code == KeyCode::Esc {
            if !self.completions().is_empty() {
                self.popup.dismissed_for = Some(self.editor.text());
            } else if self.focused_agent.is_some() {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    if self.is_busy() {
                        self.interrupt();
                    } else {
                        self.flash("subagent is not running · press esc for agent hub");
                    }
                } else {
                    self.unfocus_agent();
                    self.open_agent_hub();
                }
            } else if self.suspend_loop_on_esc() {
            } else if self.is_busy() {
                self.interrupt();
            } else {
                self.on_idle_escape();
            }
            return true;
        }
        false
    }

    fn on_idle_escape(&mut self) {
        let now = Instant::now();
        let has_input = !self.editor.is_blank() || self.images.has_pending();
        if self.focused_agent.is_some() && !has_input {
            self.unfocus_agent();
            return;
        }
        if self
            .esc_armed
            .take()
            .is_some_and(|at| now.duration_since(at) < ESC_DOUBLE_WINDOW)
        {
            self.flash = None;
            if has_input {
                self.editor.set("");
                self.images.clear_pending();
                self.vim.reset_for_new_draft();
                self.flash("input cleared");
            } else {
                match self.double_escape_action {
                    DoubleEscapeAction::Tree => self.open_tree(),
                    DoubleEscapeAction::Rewind => self.open_tree_mode(TreeMode::Branch),
                    DoubleEscapeAction::None => {}
                }
            }
            return;
        }
        self.esc_armed = Some(now);
        if has_input {
            self.flash("press esc again to clear input");
        } else {
            match self.double_escape_action {
                DoubleEscapeAction::Tree => {
                    self.flash("press esc again to open session tree (rewind/fork)");
                }
                DoubleEscapeAction::Rewind => {
                    self.flash("press esc again to rewind / branch");
                }
                DoubleEscapeAction::None => {}
            }
        }
    }

    /// Completions for the current input, unless the popup was dismissed for exactly this text.
    pub(super) fn completions(&self) -> Vec<Choice> {
        let text = self.editor.text();
        if !text.starts_with('/') || self.popup.dismissed_for.as_deref() == Some(text.as_str()) {
            return Vec::new();
        }
        let loop_help = self.loop_autocomplete_help();
        slash::complete_with_loop(
            &text,
            &self.backend.model_aliases(),
            &self.backend.custom_commands(),
            Some(&loop_help),
        )
    }

    fn popup_key(&mut self, key: KeyEvent) -> bool {
        let choices = self.completions();
        let count = choices.len();
        if count == 0 {
            return false;
        }
        let selected = self.popup.selected.min(count - 1);
        match key.code {
            KeyCode::Up => self.popup.selected = (selected + count - 1) % count,
            KeyCode::Down => self.popup.selected = (selected + 1) % count,
            KeyCode::Tab | KeyCode::Enter if key.modifiers.is_empty() => {
                let Some(choice) = choices.get(selected) else {
                    return false;
                };
                if key.code == KeyCode::Enter
                    && choice.text.trim_end() == self.editor.text().trim_end()
                {
                    return false;
                }
                self.editor.set(&choice.text);
                self.popup.selected = 0;
                if key.code == KeyCode::Enter && choice.complete {
                    self.submit();
                }
            }
            _ => return false,
        }
        true
    }

    fn editor_key(&mut self, key: KeyEvent, burst: bool) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if self.keymap.is("app.message.followUp", key) && self.is_busy() {
            self.vim.cancel_pending();
            self.queue_follow_up();
            return;
        }
        if self.keymap.is("app.message.dequeue", key) && self.dequeue_follow_up() {
            self.vim.cancel_pending();
            return;
        }
        if self.keymap.is("app.clipboard.pasteTextRaw", key) {
            self.paste_clipboard_text_raw();
            return;
        }
        if self.keymap.is("app.clipboard.pasteImage", key) {
            self.vim.cancel_pending();
            self.paste_clipboard();
            return;
        }
        if self.vim.handle_key(key, &mut self.editor) == VimAction::Consumed {
            return;
        }
        if self.keymap.is("app.editor.newline", key)
            || matches!(key.code, KeyCode::Char('\n'))
            || (matches!(key.code, KeyCode::Enter | KeyCode::Char('\r'))
                && (shift || ctrl || alt || burst))
        {
            self.editor.insert_char('\n');
            return;
        }
        let recall = self.appearance.recall_cleared_drafts;
        let e = &mut self.editor;
        match key.code {
            KeyCode::Enter => {
                if self.editor.cursor() > 0
                    && self.editor.chars().get(self.editor.cursor() - 1) == Some(&'\\')
                {
                    self.editor.backspace();
                    self.editor.insert_char('\n');
                } else if self.is_busy() && self.editor.is_blank() && !self.images.has_pending() {
                    self.promote_followup_to_steer();
                } else {
                    self.submit();
                }
            }
            // AltGr arrives as ctrl+alt on Windows: it types a character.
            KeyCode::Char(c) if ctrl && alt => e.insert_char(c),
            KeyCode::Char('j') if ctrl => e.insert_char('\n'),
            KeyCode::Char('a') if ctrl => e.home(),
            KeyCode::Char('e') if ctrl => e.end(),
            KeyCode::Char('u') if ctrl => e.kill_to_start(),
            KeyCode::Char('k') if ctrl => e.kill_to_end(),
            KeyCode::Char('w') if ctrl => e.delete_word(),
            KeyCode::Char('b') if alt => e.word_left(),
            KeyCode::Char('f') if alt => e.word_right(),
            KeyCode::Char(c) if !ctrl && !alt => e.insert_char(c),
            KeyCode::Backspace if ctrl || alt => e.delete_word(),
            KeyCode::Backspace => e.backspace(),
            KeyCode::Delete => e.delete(),
            KeyCode::Left if ctrl || alt => e.word_left(),
            KeyCode::Left => {
                if self.editor.is_blank() && !self.images.has_pending() {
                    self.on_empty_left_key();
                } else {
                    self.editor.left();
                }
            }
            KeyCode::Right if ctrl || alt => e.word_right(),
            KeyCode::Right => e.right(),
            KeyCode::Home => e.home(),
            KeyCode::End => e.end(),
            // At the first or last row the arrows browse cleared drafts (first) then prompt history.
            KeyCode::Up if !e.up() => e.history_prev_with(recall),
            KeyCode::Down if !e.down() => e.history_next(),
            _ => {}
        }
    }

    /// Send the input: a slash command (built-in or custom), a steer while a turn runs, or a new turn.
    pub(super) fn submit(&mut self) {
        let raw = self.editor.text();
        if raw.trim().is_empty() && !self.images.has_pending() {
            return;
        }
        if self.todo_editing && !raw.trim_start().starts_with('/') {
            self.todo_editing = false;
            let markdown = raw.clone();
            match self.apply_todo_edit(&markdown) {
                Ok(()) => self.flash("todo list updated"),
                Err(err) => {
                    self.notice(Level::Error, err);
                    self.editor.set(&markdown);
                }
            }
            return;
        }
        if let Some(parsed) = slash::parse(&raw) {
            self.editor.take();
            self.popup.dismissed_for = None;
            self.vim.reset_for_new_draft();
            match parsed {
                Ok(command) => self.run_command(command),
                Err(message) => {
                    if let Some((name, args)) = slash::split_command(&raw)
                        && let Some(expanded) = self.backend.expand_custom_command(name, args)
                    {
                        self.send_prompt(&expanded, Some(raw.trim().to_owned()));
                    } else {
                        self.notice(Level::Warn, message);
                    }
                }
            }
            return;
        }
        let text = self.editor.take();
        self.vim.reset_for_new_draft();
        // `//text` sends a literal leading slash.
        let prompt = if text.trim().is_empty() {
            self.images.pending_labels().join(" ")
        } else {
            match text.strip_prefix('/') {
                Some(rest) if rest.starts_with('/') => rest.to_owned(),
                _ => text,
            }
        };
        self.send_prompt(&prompt, None);
    }

    pub(crate) fn send_prompt(&mut self, prompt: &str, display: Option<String>) {
        self.dispatch_prompt(prompt, display, true);
    }
}
