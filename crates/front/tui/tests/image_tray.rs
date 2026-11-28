//! End-to-end TUI regression tests for image staging and deletion in the prompt editor.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hrn_kernel::{
    DagNode, HrnError, ImageAttachment, PermissionMode, Result, SessionMeta, ThinkingLevel, Tier,
};
use hrn_tui::{
    App, AppearanceConfig, Backend, DoubleEscapeAction, Options, PinnedAgentsMode, Settings,
};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use ratatui::buffer::Buffer;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type Submitted = Arc<Mutex<Vec<(String, Vec<ImageAttachment>)>>>;

struct RecordingBackend {
    submitted: Submitted,
}

impl Backend for RecordingBackend {
    fn model_aliases(&self) -> Vec<String> {
        vec!["mock".to_owned()]
    }

    fn pull_goal(&self, _settings: &mut Settings) {}

    fn goal_command(&mut self, _arg: Option<&str>) -> Result<String> {
        Ok(String::new())
    }

    fn modelpreset_command(&mut self, _arg: Option<&str>) -> Result<String> {
        Ok(String::new())
    }

    fn resolve_model(&self, alias: &str) -> Result<String> {
        Ok(alias.to_owned())
    }

    fn apply_settings(&mut self, _settings: &Settings) {}

    fn submit(&mut self, prompt: &str) -> Result {
        if let Ok(mut guard) = self.submitted.lock() {
            guard.push((prompt.to_owned(), Vec::new()));
        }
        Ok(())
    }

    fn submit_with_images(&mut self, prompt: &str, images: Vec<ImageAttachment>) -> Result {
        if let Ok(mut guard) = self.submitted.lock() {
            guard.push((prompt.to_owned(), images));
        }
        Ok(())
    }

    fn steer(&mut self, _text: &str) {}

    fn interrupt(&mut self) {}

    fn compact(&mut self, _instructions: Option<&str>) -> Result<bool> {
        Ok(false)
    }

    fn new_session(&mut self) {}

    fn sessions(&self) -> Result<Vec<SessionMeta>> {
        Ok(Vec::new())
    }

    fn resume(&mut self, _session_id: &str) -> Result<(SessionMeta, Vec<DagNode>)> {
        Err(HrnError::Config("unused".to_owned()))
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn write_tiny_png(path: &Path, shade: u8) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let img = RgbaImage::from_pixel(4, 4, Rgba([shade, 120, 200, 255]));
    DynamicImage::ImageRgba8(img).save_with_format(path, ImageFormat::Png)?;
    Ok(())
}

fn live_text(buf: &Buffer) -> String {
    let area = buf.area;
    let mut lines = Vec::with_capacity(usize::from(area.height));
    for y in area.y..area.bottom() {
        let mut row = String::new();
        for x in area.x..area.right() {
            if let Some(cell) = buf.cell((x, y)) {
                row.push_str(cell.symbol());
            }
        }
        lines.push(row.trim_end().to_owned());
    }
    lines.join("\n")
}

fn make_app(cwd: PathBuf) -> (App, Submitted) {
    let submitted: Submitted = Arc::new(Mutex::new(Vec::new()));
    let backend = RecordingBackend {
        submitted: Arc::clone(&submitted),
    };
    let options = Options {
        settings: Settings {
            model: "mock".to_owned(),
            thinking: ThinkingLevel::Off,
            mode: PermissionMode::Default,
            advisor: false,
            advisor_model: None,
            hide_thinking: false,
            goal: None,
            service_tier: Tier::default(),
        },
        cwd,
        resumed: None,
        recent_sessions: Vec::new(),
        ask_timeout: None,
        theme: None,
        appearance: AppearanceConfig::default(),
        initial_prompt: None,
        initial_flash: None,
        initial_notices: Vec::new(),
        start_restore: false,
        branch_summary: false,
        double_escape_action: DoubleEscapeAction::Tree,
        btw_inject: false,
        pinned_agents: PinnedAgentsMode::default(),
        subagent_live_preview: false,
        mouse: false,
    };
    (App::new(Box::new(backend), options), submitted)
}

fn make_temp_dir(tag: &str) -> std::result::Result<PathBuf, Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!("hrn-tui-img-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[test]
fn backspace_removing_first_image_updates_pending_bar_and_payload()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let dir = make_temp_dir("backspace-first")?;
    let first = dir.join("first.png");
    let second = dir.join("second.png");
    write_tiny_png(&first, 40)?;
    write_tiny_png(&second, 180)?;

    let (mut app, submitted) = make_app(dir.clone());
    app.paste(&first.to_string_lossy());
    app.paste(&second.to_string_lossy());

    let before = live_text(&app.paint(120, 30).live);
    assert!(before.contains("[Image #1] first.png"), "{before}");
    assert!(before.contains("[Image #2] second.png"), "{before}");

    // Editor now holds "[Image #1] [Image #2] |". Move left past space, "[Image #2]", and space,
    // so cursor sits immediately after "[Image #1]", then press Backspace to delete it atomically.
    app.press(key(KeyCode::Left));
    app.press(key(KeyCode::Left));
    app.press(key(KeyCode::Left));
    app.press(key(KeyCode::Backspace));

    let after = live_text(&app.paint(120, 30).live);
    assert!(
        !after.contains("[Image #1]"),
        "deleted image #1 should disappear from pending bar:\n{after}"
    );
    assert!(after.contains("[Image #2] second.png"), "{after}");

    app.press(key(KeyCode::End));
    app.paste("keep second");
    app.press(key(KeyCode::Enter));

    let calls = submitted.lock().map_err(|e| e.to_string())?;
    assert_eq!(calls.len(), 1);
    let (prompt, images) = calls.first().ok_or("missing submitted turn")?;
    assert!(prompt.contains("[Image #2]"), "{prompt}");
    assert!(!prompt.contains("[Image #1]"), "{prompt}");
    assert_eq!(images.len(), 1);
    assert_eq!(
        images.first().and_then(|img| img.label.as_deref()),
        Some("second.png")
    );
    drop(calls);

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn backspace_removing_only_image_hides_pending_bar_and_blocks_empty_submit()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let dir = make_temp_dir("backspace-only")?;
    let only = dir.join("only.png");
    write_tiny_png(&only, 90)?;

    let (mut app, submitted) = make_app(dir.clone());
    app.paste(&only.to_string_lossy());
    let before = live_text(&app.paint(120, 30).live);
    assert!(before.contains("[Image #1] only.png"), "{before}");

    // Delete trailing space, then atomically delete "[Image #1]".
    app.press(key(KeyCode::Backspace));
    app.press(key(KeyCode::Backspace));

    let after = live_text(&app.paint(120, 30).live);
    assert!(
        !after.contains("[Image #1] only.png"),
        "pending bar should clear when all placeholders are deleted:\n{after}"
    );
    assert!(
        !after.contains("[img]"),
        "pending image strip should disappear when no images remain:\n{after}"
    );

    app.press(key(KeyCode::Enter));
    let calls = submitted.lock().map_err(|e| e.to_string())?;
    assert!(calls.is_empty(), "empty prompt must not submit stale image");
    drop(calls);

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn ctrl_c_clears_pending_images_and_up_recall_restores_them()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let dir = make_temp_dir("ctrl-c-recall")?;
    let shot = dir.join("shot.png");
    write_tiny_png(&shot, 150)?;

    let (mut app, _submitted) = make_app(dir.clone());
    app.paste(&shot.to_string_lossy());
    app.paste("draft text");
    assert!(
        live_text(&app.paint(120, 30).live).contains("[Image #1] shot.png"),
        "expected staged image before Ctrl+C"
    );

    app.press(ctrl('c'));
    let cleared = live_text(&app.paint(120, 30).live);
    assert!(
        !cleared.contains("[Image #1] shot.png") && !cleared.contains("[img]"),
        "Ctrl+C should clear pending bar:\n{cleared}"
    );

    app.press(key(KeyCode::Up));
    let recalled = live_text(&app.paint(120, 30).live);
    assert!(
        recalled.contains("[Image #1] shot.png"),
        "Up draft recall should restore staged image in pending bar:\n{recalled}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
