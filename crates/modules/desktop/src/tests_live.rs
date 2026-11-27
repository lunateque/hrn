//! Live Windows `#[ignore]` integration tests spawning the real PowerShell/C# bridge.

use crate::tests::test_ctx;
use crate::{DesktopManager, DesktopTool};
use hrn_kernel::Tool;
use hrn_kernel::serde_json::json;
use std::sync::Arc;

#[tokio::test]
#[ignore = "spawns live Windows PowerShell/C# desktop bridge"]
async fn live_windows_desktop_bridge_lists_windows_and_captures_screenshot() {
    let tool = DesktopTool::new(Arc::new(DesktopManager::new()));
    let ctx = test_ctx();

    let wins = tool
        .execute(&ctx, json!({"action": "list_windows"}))
        .await
        .expect("list_windows");
    assert!(!wins.is_error, "{}", wins.content);

    match tool.execute(&ctx, json!({"action": "screenshot"})).await {
        Ok(shot) => {
            assert!(!shot.is_error, "{}", shot.content);
            assert_eq!(shot.images.len(), 1);
            let img = shot.images.first().expect("image");
            assert!(img.width.unwrap_or(0) > 0);
            assert!(img.height.unwrap_or(0) > 0);
            assert!(!img.data_base64.is_empty());
        }
        Err(err) => {
            let msg = err.to_string();
            assert!(
                msg.contains("screen capture unavailable"),
                "unexpected desktop screenshot failure: {msg}"
            );
        }
    }

    let _ = tool.execute(&ctx, json!({"action": "close"})).await;
}

#[tokio::test]
#[ignore = "spawns live Windows PowerShell/C# desktop bridge and modifies clipboard"]
async fn live_clipboard_roundtrip() {
    let tool = DesktopTool::new(Arc::new(DesktopManager::new()));
    let ctx = test_ctx();
    let probe = "hrn-desktop-wave8c-clipboard-probe-42";

    let prev = tool
        .execute(&ctx, json!({"action": "clipboard_read"}))
        .await
        .ok()
        .map(|o| o.content);

    let wrote = tool
        .execute(&ctx, json!({"action": "clipboard_write", "text": probe}))
        .await
        .expect("clipboard_write");
    assert!(!wrote.is_error, "{}", wrote.content);

    let read = tool
        .execute(&ctx, json!({"action": "clipboard_read"}))
        .await
        .expect("clipboard_read");
    assert_eq!(read.content, probe);

    if let Some(old) = prev
        && old != "Clipboard has no text."
    {
        let _ = tool
            .execute(&ctx, json!({"action": "clipboard_write", "text": old}))
            .await;
    }
    let _ = tool.execute(&ctx, json!({"action": "close"})).await;
}

#[tokio::test]
#[ignore = "launches live Notepad on Windows to verify UIA tree, set_value, and covered capture"]
async fn live_notepad_uia_set_value_and_covered_capture() {
    let tool = DesktopTool::new(Arc::new(DesktopManager::new()));
    let ctx = test_ctx();

    let mut child = std::process::Command::new("notepad.exe")
        .spawn()
        .expect("spawn notepad");
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    let list = tool
        .execute(&ctx, json!({"action": "list_windows"}))
        .await
        .expect("list_windows");
    assert!(
        list.content.to_ascii_lowercase().contains("notepad"),
        "{}",
        list.content
    );

    let ax = tool
        .execute(&ctx, json!({"action": "ax", "window": "Notepad"}))
        .await
        .expect("ax notepad");
    assert!(
        ax.content.contains("- Edit ") || ax.content.contains("- Document "),
        "expected Edit/Document in Notepad AX tree:\n{}",
        ax.content
    );
    assert!(ax.content.contains("[ref=e"), "{}", ax.content);

    let found = tool
        .execute(
            &ctx,
            json!({"action": "find", "window": "Notepad", "role": "edit"}),
        )
        .await
        .expect("find edit");
    let ref_id = extract_first_ref(&found.content)
        .or_else(|| extract_first_ref_by_role(&ax.content, "Document"))
        .expect("notepad text ref");

    let set = tool
        .execute(
            &ctx,
            json!({"action": "set_value", "ref": ref_id, "value": "hrn wave8c uia"}),
        )
        .await
        .expect("set_value");
    assert!(set.content.contains("verified"), "{}", set.content);

    let cap = tool
        .execute(
            &ctx,
            json!({"action": "window_capture", "window": "Notepad"}),
        )
        .await
        .expect("window_capture");
    assert_eq!(cap.images.len(), 1);

    let _ = tool.execute(&ctx, json!({"action": "close"})).await;
    let _ = child.kill();
    let _ = child.wait();
}

fn extract_first_ref(text: &str) -> Option<String> {
    let start = text.find("[ref=")? + 5;
    let rest = text.get(start..)?;
    let end = rest.find(']')?;
    Some(rest.get(..end)?.to_owned())
}

fn extract_first_ref_by_role(text: &str, role: &str) -> Option<String> {
    let needle = format!("- {role} ");
    for line in text.lines() {
        if line.contains(&needle) {
            return extract_first_ref(line);
        }
    }
    None
}
