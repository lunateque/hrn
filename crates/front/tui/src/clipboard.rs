//! Reliable OS clipboard access for images and text, including Windows Snipping Tool / PNG /
//! File Explorer `CF_HDROP` fallbacks where `arboard` alone may miss non-DIB formats.

use crate::imgview;
use hrn_kernel::ImageAttachment;
use std::thread;
use std::time::Duration;

const RETRIES: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(15);

/// Read an image from the OS clipboard, returning `None` when the clipboard contains no image.
pub(crate) fn read_image() -> Option<Result<ImageAttachment, String>> {
    if let Some(res) = read_arboard_image() {
        return Some(res);
    }
    #[cfg(windows)]
    if let Some(res) = read_windows_clipboard_fallback() {
        return Some(res);
    }
    None
}

/// Put `text` on the OS clipboard. `false` when the clipboard is unavailable.
pub(crate) fn write_text(text: &str) -> bool {
    for attempt in 0..RETRIES {
        if let Ok(mut cb) = arboard::Clipboard::new() {
            match cb.set_text(text.to_owned()) {
                Ok(()) => return true,
                Err(_) if attempt + 1 < RETRIES => thread::sleep(RETRY_DELAY),
                Err(_) => return false,
            }
        } else if attempt + 1 < RETRIES {
            thread::sleep(RETRY_DELAY);
        }
    }
    false
}

/// Read plain text from the OS clipboard, retrying briefly if another app holds the lock.
pub(crate) fn read_text() -> Option<String> {
    for attempt in 0..RETRIES {
        if let Ok(mut cb) = arboard::Clipboard::new() {
            match cb.get_text() {
                Ok(text) if !text.is_empty() => return Some(text),
                Err(_) if attempt + 1 < RETRIES => thread::sleep(RETRY_DELAY),
                Ok(_) | Err(_) => return None,
            }
        } else if attempt + 1 < RETRIES {
            thread::sleep(RETRY_DELAY);
        }
    }
    None
}

fn read_arboard_image() -> Option<Result<ImageAttachment, String>> {
    for attempt in 0..RETRIES {
        let Ok(mut cb) = arboard::Clipboard::new() else {
            if attempt + 1 < RETRIES {
                thread::sleep(RETRY_DELAY);
            }
            continue;
        };
        match cb.get_image() {
            Ok(img) => {
                let Ok(w) = u32::try_from(img.width) else {
                    return Some(Err("clipboard image width overflow".to_owned()));
                };
                let Ok(h) = u32::try_from(img.height) else {
                    return Some(Err("clipboard image height overflow".to_owned()));
                };
                return Some(imgview::encode_rgba(
                    w,
                    h,
                    img.bytes.into_owned(),
                    Some("clipboard.png".to_owned()),
                ));
            }
            Err(arboard::Error::ContentNotAvailable) => return None,
            Err(_) if attempt + 1 < RETRIES => thread::sleep(RETRY_DELAY),
            Err(_) => return None,
        }
    }
    None
}

/// Windows fallback via STA PowerShell + `System.Windows.Forms.Clipboard` to capture:
/// 1. Clipboard images in formats `arboard` misses (e.g. PNG-only / Snipping Tool / browser copy).
/// 2. Image files copied from File Explorer (`CF_HDROP` / `FileDropList`).
#[cfg(windows)]
fn read_windows_clipboard_fallback() -> Option<Result<ImageAttachment, String>> {
    use std::path::Path;
    use std::process::Command;

    const SCRIPT: &str = r"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
if ([System.Windows.Forms.Clipboard]::ContainsImage()) {
    $img = [System.Windows.Forms.Clipboard]::GetImage()
    if ($null -ne $img) {
        $tmp = [System.IO.Path]::Combine([System.IO.Path]::GetTempPath(), ('hrn_cb_' + [guid]::NewGuid().ToString('N') + '.png'))
        $img.Save($tmp, [System.Drawing.Imaging.ImageFormat]::Png)
        $img.Dispose()
        Write-Output ('PNG:' + $tmp)
        exit 0
    }
}
if ([System.Windows.Forms.Clipboard]::ContainsFileDropList()) {
    $files = [System.Windows.Forms.Clipboard]::GetFileDropList()
    foreach ($f in $files) {
        if ($f -match '\.(png|jpe?g|webp|gif|bmp)$') {
            Write-Output ('FILE:' + $f)
            exit 0
        }
    }
}
";

    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", SCRIPT])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().find(|l| !l.trim().is_empty())?.trim();
    if let Some(tmp_path) = line.strip_prefix("PNG:") {
        let path = Path::new(tmp_path);
        let res = imgview::load_image_file_labeled(path, Some("clipboard.png".to_owned()));
        let _ = std::fs::remove_file(path);
        return Some(res);
    }
    if let Some(file_path) = line.strip_prefix("FILE:") {
        let path = Path::new(file_path);
        if path.is_file() {
            return Some(imgview::load_image_file(path));
        }
    }
    None
}
