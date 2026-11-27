//! TUI handlers for Wave 9a2: `/handoff`, `/branch` (`/rewind`), shared tree leaf rules,
//! inline node labels, abandoned-branch summaries, and `/btw` side-thread overlay.

use super::{App, Task};
use crate::btw::{BtwAction, BtwPanel};
use crate::clipboard;
use crate::transcript::Level;
use crate::tree::{TreeAction, TreeMode, TreeOverlay};
use crossterm::event::KeyEvent;
use hrn_kernel::{BtwExchange, BtwStatus, BtwTopic};

impl App {
    pub(super) fn open_tree_mode(&mut self, mode: TreeMode) {
        match self.backend.current_tree() {
            Ok(Some((head, nodes))) if !nodes.is_empty() => {
                self.tree = Some(TreeOverlay::new(head, nodes, mode, self.branch_summary));
            }
            Ok(_) => self.notice(Level::Info, "no messages in this session yet"),
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    pub(super) fn handle_tree_key(&mut self, key: KeyEvent) -> bool {
        let Some(tree) = &mut self.tree else {
            return false;
        };
        match tree.key(key) {
            TreeAction::Stay => {}
            TreeAction::Close => self.tree = None,
            TreeAction::AlreadyAtLeaf => {
                self.tree = None;
                self.flash("Already at this point");
            }
            TreeAction::SetLabel { node_id, label } => {
                match self.backend.set_node_label(&node_id, label.as_deref()) {
                    Ok(()) => {
                        if let Some(tree) = &mut self.tree {
                            tree.apply_local_label(&node_id, label.as_deref());
                        }
                        match label {
                            Some(text) => self.flash(format!("labeled · {text}")),
                            None => self.flash("label cleared"),
                        }
                    }
                    Err(err) => self.notice(Level::Error, err.to_string()),
                }
            }
            TreeAction::Select {
                selection,
                summarize,
            } => {
                self.tree = None;
                match self
                    .backend
                    .checkout_optional(selection.target_head.as_deref(), summarize)
                {
                    Ok((meta, nodes)) => {
                        self.show_session(&meta, &nodes);
                        self.backend.apply_settings(&self.settings);
                        if let Some(draft) = selection.prefill
                            && self.editor.is_blank()
                        {
                            self.editor.set(&draft);
                        }
                        self.flash(if summarize {
                            "switched branch · summarized abandoned work"
                        } else {
                            "switched branch"
                        });
                    }
                    Err(err) => self.notice(Level::Error, err.to_string()),
                }
            }
            TreeAction::Fork(id) => {
                self.tree = None;
                self.fork_session(Some(&id));
            }
        }
        true
    }

    pub(super) fn handle_handoff(&mut self, focus: Option<&str>) {
        if matches!(self.task, Task::Compact) {
            self.notice(
                Level::Warn,
                "compaction or handoff is already running · press esc to cancel",
            );
            return;
        }
        match self.backend.handoff(focus) {
            Ok(true) => {
                if matches!(self.task, Task::Idle) {
                    self.start_task(Task::Compact);
                    self.flash("generating handoff…");
                } else {
                    self.flash("generating handoff · applies at tool boundary…");
                }
            }
            Ok(false) => self.notice(Level::Info, "nothing to hand off yet"),
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    pub(super) fn handle_btw(&mut self, arg: Option<&str>) {
        let question = arg.map(str::trim).filter(|s| !s.is_empty());
        let Some(question) = question else {
            match self.backend.btw_topics() {
                Ok(topics) => self.btw = Some(BtwPanel::history(topics, self.btw_inject)),
                Err(err) => self.notice(Level::Error, err.to_string()),
            }
            return;
        };
        if self.btw_running {
            self.notice(
                Level::Warn,
                "a /btw question is already running · wait or press esc inside /btw",
            );
            return;
        }
        match self.backend.start_btw(None, question) {
            Ok(topic_id) => {
                self.btw_running = true;
                let topic = BtwTopic {
                    id: topic_id,
                    title: question.chars().take(60).collect(),
                    exchanges: vec![BtwExchange {
                        question: question.to_owned(),
                        answer: String::new(),
                        status: BtwStatus::Streaming,
                        injected: false,
                    }],
                    updated_at_ms: 0,
                };
                self.btw = Some(BtwPanel::streaming(topic, self.btw_inject));
            }
            Err(err) => self.notice(Level::Error, err.to_string()),
        }
    }

    pub(super) fn btw_key(&mut self, key: KeyEvent) -> bool {
        let inject_chord = self.keymap.is("app.btw.inject", key);
        let Some(panel) = &mut self.btw else {
            return false;
        };
        match panel.key(key, inject_chord) {
            BtwAction::Stay => {}
            BtwAction::Close => self.btw = None,
            BtwAction::CancelStream => {
                self.backend.cancel_btw();
                self.btw_running = false;
                self.flash("/btw cancelled · partial kept");
            }
            BtwAction::Copy(text) => {
                if clipboard::write_text(&text) {
                    self.flash("copied /btw answer");
                } else {
                    self.notice(Level::Warn, "clipboard unavailable");
                }
            }
            BtwAction::FollowUp { topic_id, question } => {
                if self.btw_running {
                    self.flash("a /btw question is already running");
                } else {
                    match self.backend.start_btw(Some(&topic_id), &question) {
                        Ok(_) => self.btw_running = true,
                        Err(err) => self.notice(Level::Error, err.to_string()),
                    }
                }
            }
            BtwAction::Inject { topic, payload } => {
                let _ = self.backend.save_btw_topic(&topic);
                self.btw = None;
                self.send_prompt(&payload, None);
                self.flash("injected /btw block");
            }
            BtwAction::AlreadyInjected => {
                self.flash("/btw block was already injected");
            }
        }
        true
    }

    pub(super) fn on_btw_delta(&mut self, topic_id: &str, delta: &str) {
        if let Some(panel) = &mut self.btw {
            panel.push_delta(topic_id, delta);
        }
    }

    pub(super) fn on_btw_done(&mut self, topic: BtwTopic) {
        self.btw_running = false;
        if let Some(panel) = &mut self.btw {
            panel.finish(topic);
        }
    }
}
