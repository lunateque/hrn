//! Screenshot-to-physical-screen coordinate mapping and hotkey parsing/normalization.

use hrn_kernel::serde_json::Value;
use hrn_kernel::{HrnError, Result};

/// Mapping between downscaled screenshot pixels and physical desktop pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenScale {
    /// Physical X origin of the captured region (0 for primary screen, may be negative on multi-monitor).
    pub origin_x: i64,
    /// Physical Y origin of the captured region.
    pub origin_y: i64,
    /// Physical width in pixels before downscaling.
    pub phys_w: u32,
    /// Physical height in pixels before downscaling.
    pub phys_h: u32,
    /// Width of the image handed to the model.
    pub img_w: u32,
    /// Height of the image handed to the model.
    pub img_h: u32,
}

impl ScreenScale {
    /// Map screenshot coordinates `(x, y)` back into physical desktop screen coordinates.
    pub fn to_physical(self, x: i64, y: i64) -> (i64, i64) {
        let img_w = i64::from(self.img_w.max(1));
        let img_h = i64::from(self.img_h.max(1));
        let phys_w = i64::from(self.phys_w.max(1));
        let phys_h = i64::from(self.phys_h.max(1));

        let cx = x.clamp(0, img_w);
        let cy = y.clamp(0, img_h);
        let px = self.origin_x + (cx * phys_w + img_w / 2) / img_w;
        let py = self.origin_y + (cy * phys_h + img_h / 2) / img_h;
        (px, py)
    }
}

/// Parse `keys` from tool arguments (accepts `"ctrl+shift+p"` or `["ctrl", "shift", "p"]`)
/// and return Windows Virtual-Key codes (`u8`) in press order.
pub(crate) fn parse_hotkey_vks(args: &Value) -> Result<(Vec<u8>, String)> {
    let raw_keys: Vec<String> = match args.get("keys") {
        Some(Value::String(s)) => s
            .split('+')
            .map(|part| part.trim().to_owned())
            .filter(|p| !p.is_empty())
            .collect(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(|part| part.trim().to_owned())
            .filter(|p| !p.is_empty())
            .collect(),
        _ => {
            return Err(HrnError::Tool(
                "`desktop` hotkey: provide `keys` as string (e.g. \"ctrl+s\") or array".to_owned(),
            ));
        }
    };
    if raw_keys.is_empty() {
        return Err(HrnError::Tool(
            "`desktop` hotkey: `keys` cannot be empty".to_owned(),
        ));
    }

    let mut vks = Vec::with_capacity(raw_keys.len());
    let mut labels = Vec::with_capacity(raw_keys.len());
    for key in &raw_keys {
        let (vk, norm) = key_name_to_vk(key)?;
        vks.push(vk);
        labels.push(norm);
    }
    Ok((vks, labels.join("+")))
}

fn key_name_to_vk(raw: &str) -> Result<(u8, String)> {
    let lower = raw.trim().to_ascii_lowercase();
    let mapped = match lower.as_str() {
        "ctrl" | "control" => (0x11, "ctrl"),
        "shift" => (0x10, "shift"),
        "alt" | "option" => (0x12, "alt"),
        "win" | "super" | "meta" | "cmd" => (0x5B, "win"),
        "enter" | "return" => (0x0D, "enter"),
        "esc" | "escape" => (0x1B, "esc"),
        "tab" => (0x09, "tab"),
        "space" => (0x20, "space"),
        "backspace" | "bs" => (0x08, "backspace"),
        "delete" | "del" => (0x2E, "delete"),
        "insert" | "ins" => (0x2D, "insert"),
        "home" => (0x24, "home"),
        "end" => (0x23, "end"),
        "pageup" | "pgup" => (0x21, "pageup"),
        "pagedown" | "pgdn" => (0x22, "pagedown"),
        "up" | "arrowup" => (0x26, "up"),
        "down" | "arrowdown" => (0x28, "down"),
        "left" | "arrowleft" => (0x25, "left"),
        "right" | "arrowright" => (0x27, "right"),
        "f1" => (0x70, "f1"),
        "f2" => (0x71, "f2"),
        "f3" => (0x72, "f3"),
        "f4" => (0x73, "f4"),
        "f5" => (0x74, "f5"),
        "f6" => (0x75, "f6"),
        "f7" => (0x76, "f7"),
        "f8" => (0x77, "f8"),
        "f9" => (0x78, "f9"),
        "f10" => (0x79, "f10"),
        "f11" => (0x7A, "f11"),
        "f12" => (0x7B, "f12"),
        _ => {
            if lower.len() == 1
                && let Some(b) = lower.bytes().next()
            {
                if b.is_ascii_lowercase() {
                    return Ok((b.to_ascii_uppercase(), lower));
                }
                if b.is_ascii_digit() {
                    return Ok((b, lower));
                }
            }
            return Err(HrnError::Tool(format!(
                "`desktop` hotkey: unsupported key `{raw}`"
            )));
        }
    };
    Ok((mapped.0, mapped.1.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::serde_json::json;

    #[test]
    fn maps_scaled_screenshot_coordinates_to_physical_pixels() {
        let scale = ScreenScale {
            origin_x: 100,
            origin_y: -50,
            phys_w: 2560,
            phys_h: 1440,
            img_w: 1600,
            img_h: 900,
        };
        assert_eq!(scale.to_physical(0, 0), (100, -50));
        assert_eq!(scale.to_physical(800, 450), (1380, 670));
        assert_eq!(scale.to_physical(1600, 900), (2660, 1390));
    }

    #[test]
    fn parses_hotkeys_from_string_and_array() {
        let (vks, label) = parse_hotkey_vks(&json!({"keys": "Ctrl+Shift+L"})).expect("str");
        assert_eq!(vks, vec![0x11, 0x10, b'L']);
        assert_eq!(label, "ctrl+shift+l");

        let (vks2, label2) = parse_hotkey_vks(&json!({"keys": ["alt", "F4"]})).expect("arr");
        assert_eq!(vks2, vec![0x12, 0x73]);
        assert_eq!(label2, "alt+f4");

        assert!(parse_hotkey_vks(&json!({"keys": "ctrl+boguskey"})).is_err());
    }
}
