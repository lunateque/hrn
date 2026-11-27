//! Desktop session state: stable numeric window handles, per-window UIA element refs, and
//! active coordinate frame tracking.

use crate::ax_fmt::format_ax_line;
use crate::coords::ScreenScale;
use crate::error::DesktopError;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// Default maximum depth walked for `ax` snapshots.
pub const DEFAULT_MAX_DEPTH: usize = 24;
/// Hard node cap for a single `ax` snapshot.
pub const MAX_AX_NODES: usize = 800;
/// Hard match limit for `find`.
pub const MAX_FIND_LIMIT: usize = 5000;

/// Metadata for a tracked top-level Windows desktop window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    /// Stable numeric window handle assigned by this session (`1, 2, ...`).
    pub id: u32,
    /// Native Win32 `HWND` value.
    pub hwnd: i64,
    /// Owning process identifier.
    pub pid: u32,
    /// Window title text.
    pub title: String,
    /// Physical X origin.
    pub x: i64,
    /// Physical Y origin.
    pub y: i64,
    /// Physical width in pixels.
    pub w: u32,
    /// Physical height in pixels.
    pub h: u32,
}

impl WindowInfo {
    /// Physical bounding rectangle `(x, y, w, h)`.
    pub const fn rect(&self) -> (i64, i64, u32, u32) {
        (self.x, self.y, self.w, self.h)
    }
}

/// Raw window record parsed from bridge JSON before stable ID assignment.
#[derive(Clone, Debug)]
pub(crate) struct RawWindow {
    pub hwnd: i64,
    pub pid: u32,
    pub title: String,
    pub x: i64,
    pub y: i64,
    pub w: u32,
    pub h: u32,
}

/// One UI Automation node decoded from bridge JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AxNode {
    /// UIA control role (e.g. `Edit`, `MenuBar`, `Button`).
    pub role: String,
    /// Accessible name / label.
    pub name: String,
    /// Optional textual value.
    pub value: Option<String>,
    /// Active UIA states (e.g. `focused`, `disabled`, `checked`).
    pub states: Vec<String>,
    /// UIA `RuntimeId` integer tuple.
    pub runtime_id: Vec<i32>,
    /// Physical screen bounding rectangle `(x, y, w, h)`.
    pub bounds: (i64, i64, u32, u32),
    /// Tree depth relative to the window root (`0` = root).
    pub depth: usize,
}

/// Stored reference to a UI Automation element (`e1`, `e2`, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AxRef {
    /// Stable session ref identifier (e.g. `"e1"`).
    pub ref_id: String,
    /// Stable window handle, if the owning window is tracked.
    pub window_id: Option<u32>,
    /// Native `HWND` of the owning top-level window.
    pub hwnd: i64,
    /// UIA `RuntimeId` used to relocate the element in the bridge.
    pub runtime_id: Vec<i32>,
    /// Accessible name at snapshot time.
    pub name: String,
    /// Role name at snapshot time.
    pub role: String,
    /// Bounding rectangle `(x, y, w, h)` at snapshot time.
    pub bounds: (i64, i64, u32, u32),
}

/// Target surface captured by a [`CoordFrame`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameTarget {
    /// Full display or virtual multi-monitor desktop capture.
    Desktop {
        /// Display selector (e.g. `"active"`, `"primary"`, `"all"`).
        display: String,
    },
    /// Specific top-level window capture.
    Window {
        /// Stable window handle, if tracked.
        window_id: Option<u32>,
        /// Native `HWND`.
        hwnd: i64,
    },
}

/// Active coordinate mapping frame established by the latest full screenshot or window capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoordFrame {
    /// Surface targeted by the capture.
    pub target: FrameTarget,
    /// Monotonic capture generation counter.
    pub capture_gen: u64,
    /// Physical bounding rectangle `(x, y, w, h)` at capture time.
    pub rect: (i64, i64, u32, u32),
    /// Image-to-physical pixel scaling parameters.
    pub scale: ScreenScale,
}

#[derive(Default)]
struct WindowRefHistory {
    current: HashMap<String, AxRef>,
    previous: HashMap<String, AxRef>,
}

/// Stateful desktop session holding window handles, per-window UIA refs, and the active frame.
#[derive(Default)]
pub(crate) struct DesktopSession {
    next_window_id: u32,
    hwnd_to_id: HashMap<i64, u32>,
    live_windows: HashMap<u32, WindowInfo>,
    next_ref_num: u64,
    ref_tables: HashMap<i64, WindowRefHistory>,
    capture_gen: u64,
    active_frame: Option<CoordFrame>,
}

impl DesktopSession {
    /// Construct an empty desktop session.
    pub(crate) fn new() -> Self {
        Self {
            next_window_id: 1,
            next_ref_num: 1,
            ..Self::default()
        }
    }

    /// Reset all window handles, element refs, and coordinate frames.
    pub(crate) fn clear(&mut self) {
        self.hwnd_to_id.clear();
        self.live_windows.clear();
        self.ref_tables.clear();
        self.active_frame = None;
    }

    /// Synchronize the live window table with a fresh `list_windows` result.
    ///
    /// Reuses existing numeric IDs whenever a previously seen `HWND` reappears, and expires all
    /// UIA refs belonging to windows that are no longer present.
    pub(crate) fn sync_windows(&mut self, incoming: &[RawWindow]) -> Vec<WindowInfo> {
        let incoming_hwnds: HashSet<i64> = incoming.iter().map(|w| w.hwnd).collect();
        let closed_hwnds: Vec<i64> = self
            .live_windows
            .values()
            .filter(|w| !incoming_hwnds.contains(&w.hwnd))
            .map(|w| w.hwnd)
            .collect();
        for hwnd in closed_hwnds {
            self.expire_window_refs(hwnd);
        }

        self.live_windows.clear();
        let mut ordered = Vec::with_capacity(incoming.len());
        for raw in incoming {
            ordered.push(self.register_window(raw));
        }
        ordered
    }

    /// Insert or refresh a single tracked window and return its stable [`WindowInfo`].
    pub(crate) fn upsert_window(&mut self, raw: &RawWindow) -> WindowInfo {
        self.register_window(raw)
    }

    fn register_window(&mut self, raw: &RawWindow) -> WindowInfo {
        let id = *self.hwnd_to_id.entry(raw.hwnd).or_insert_with(|| {
            let assigned = self.next_window_id;
            self.next_window_id = self.next_window_id.saturating_add(1);
            assigned
        });
        let info = WindowInfo {
            id,
            hwnd: raw.hwnd,
            pid: raw.pid,
            title: raw.title.clone(),
            x: raw.x,
            y: raw.y,
            w: raw.w,
            h: raw.h,
        };
        self.live_windows.insert(id, info.clone());
        info
    }

    /// Mark a window closed, removing it from live windows and expiring all of its UIA refs.
    pub(crate) fn mark_window_closed(&mut self, hwnd: i64) {
        if let Some(&id) = self.hwnd_to_id.get(&hwnd) {
            self.live_windows.remove(&id);
        }
        self.expire_window_refs(hwnd);
    }

    fn expire_window_refs(&mut self, hwnd: i64) {
        self.ref_tables.remove(&hwnd);
    }

    /// Look up a live window by its stable numeric handle.
    pub(crate) fn window_by_id(&self, id: u32) -> Option<&WindowInfo> {
        self.live_windows.get(&id)
    }

    /// Look up a live window by its native `HWND`.
    pub(crate) fn window_by_hwnd(&self, hwnd: i64) -> Option<&WindowInfo> {
        let id = *self.hwnd_to_id.get(&hwnd)?;
        self.live_windows.get(&id)
    }

    /// Look up the first live window whose title contains `needle` (case-insensitive).
    pub(crate) fn find_window_by_title(&self, needle: &str) -> Option<&WindowInfo> {
        let lower = needle.to_ascii_lowercase();
        let mut candidates: Vec<&WindowInfo> = self
            .live_windows
            .values()
            .filter(|w| w.title.to_ascii_lowercase().contains(&lower))
            .collect();
        candidates.sort_by_key(|w| w.id);
        candidates.into_iter().next()
    }

    /// Record a fresh UIA tree snapshot for `hwnd`, rotating `current` to `previous` and
    /// returning the compact formatted tree text, node count, and truncation flag.
    pub(crate) fn record_ax_snapshot(
        &mut self,
        hwnd: i64,
        window_id: Option<u32>,
        nodes: &[AxNode],
        max_depth: usize,
        max_nodes: usize,
    ) -> (String, usize, bool) {
        let cap = max_nodes.clamp(1, MAX_AX_NODES);
        let prev_map = self
            .ref_tables
            .remove(&hwnd)
            .map(|h| h.current)
            .unwrap_or_default();

        let mut next_current = HashMap::new();
        let mut out = String::new();
        let mut emitted = 0usize;
        let mut truncated = false;

        for node in nodes {
            if node.depth > max_depth {
                continue;
            }
            if emitted >= cap {
                truncated = true;
                break;
            }
            let ref_id = self.alloc_or_reuse_ref(&prev_map, &next_current, &node.runtime_id);
            let ax_ref = AxRef {
                ref_id: ref_id.clone(),
                window_id,
                hwnd,
                runtime_id: node.runtime_id.clone(),
                name: node.name.clone(),
                role: node.role.clone(),
                bounds: node.bounds,
            };
            next_current.insert(ref_id.clone(), ax_ref);
            format_ax_line(&mut out, node, &ref_id);
            emitted += 1;
        }

        self.ref_tables.insert(
            hwnd,
            WindowRefHistory {
                current: next_current,
                previous: prev_map,
            },
        );
        if truncated {
            let _ = writeln!(out, "... (truncated at {cap} nodes)");
        }
        (out.trim_end().to_owned(), emitted, truncated)
    }

    /// Register `find` results in `hwnd`'s current ref table without discarding its previous snapshot.
    pub(crate) fn register_find_matches(
        &mut self,
        hwnd: i64,
        window_id: Option<u32>,
        nodes: &[AxNode],
    ) -> Vec<(String, AxNode)> {
        let mut next_num = self.next_ref_num;
        let history = self.ref_tables.entry(hwnd).or_default();
        let mut out = Vec::with_capacity(nodes.len());

        for node in nodes {
            let reused = find_matching_ref(&history.current, &node.runtime_id)
                .or_else(|| find_matching_ref(&history.previous, &node.runtime_id));
            let ref_id = reused.unwrap_or_else(|| {
                let id = format!("e{next_num}");
                next_num = next_num.saturating_add(1);
                id
            });
            history.current.insert(
                ref_id.clone(),
                AxRef {
                    ref_id: ref_id.clone(),
                    window_id,
                    hwnd,
                    runtime_id: node.runtime_id.clone(),
                    name: node.name.clone(),
                    role: node.role.clone(),
                    bounds: node.bounds,
                },
            );
            out.push((ref_id, node.clone()));
        }
        self.next_ref_num = next_num;
        out
    }

    fn alloc_or_reuse_ref(
        &mut self,
        prev: &HashMap<String, AxRef>,
        current: &HashMap<String, AxRef>,
        runtime_id: &[i32],
    ) -> String {
        if let Some(existing) = find_matching_ref(prev, runtime_id)
            && !current.contains_key(&existing)
        {
            return existing;
        }
        let id = format!("e{}", self.next_ref_num);
        self.next_ref_num = self.next_ref_num.saturating_add(1);
        id
    }

    /// Resolve a UIA element reference (`eN` or `@eN`) across current and previous snapshots.
    pub(crate) fn resolve_ref(&self, raw_ref: &str) -> Result<AxRef, DesktopError> {
        let key = raw_ref.trim().trim_start_matches('@');
        if key.is_empty() {
            return Err(DesktopError::StaleRef(
                "element ref cannot be empty".to_owned(),
            ));
        }
        for history in self.ref_tables.values() {
            if let Some(found) = history.current.get(key) {
                return Ok(found.clone());
            }
        }
        for history in self.ref_tables.values() {
            if let Some(found) = history.previous.get(key) {
                return Ok(found.clone());
            }
        }
        Err(DesktopError::StaleRef(format!(
            "element ref `{key}` is stale or unknown; run `ax` or `find` to refresh"
        )))
    }

    /// Replace the active coordinate frame after a full screenshot or window capture.
    pub(crate) fn set_frame(
        &mut self,
        target: FrameTarget,
        rect: (i64, i64, u32, u32),
        scale: ScreenScale,
    ) -> CoordFrame {
        self.capture_gen = self.capture_gen.saturating_add(1);
        let frame = CoordFrame {
            target,
            capture_gen: self.capture_gen,
            rect,
            scale,
        };
        self.active_frame = Some(frame.clone());
        frame
    }

    /// Active coordinate frame, if any full capture has taken place.
    pub(crate) fn active_frame(&self) -> Option<&CoordFrame> {
        self.active_frame.as_ref()
    }

    /// Validate that the active window frame still matches `hwnd`'s live rectangle.
    pub(crate) fn validate_window_rect(
        &self,
        hwnd: i64,
        current_rect: (i64, i64, u32, u32),
    ) -> Result<(), DesktopError> {
        let Some(frame) = &self.active_frame else {
            return Err(DesktopError::InvalidCoordinateFrame(
                "no active coordinate frame; run `window_capture` or `screenshot` first".to_owned(),
            ));
        };
        match &frame.target {
            FrameTarget::Window {
                hwnd: frame_hwnd, ..
            } => {
                if *frame_hwnd != hwnd {
                    return Err(DesktopError::InvalidCoordinateFrame(format!(
                        "active coordinate frame belongs to HWND {frame_hwnd}, not {hwnd}; recapture first"
                    )));
                }
                if frame.rect != current_rect {
                    return Err(DesktopError::InvalidCoordinateFrame(format!(
                        "window rect changed from {:?} to {:?} since capture #{}; recapture before clicking",
                        frame.rect, current_rect, frame.capture_gen
                    )));
                }
                Ok(())
            }
            FrameTarget::Desktop { .. } => Err(DesktopError::InvalidCoordinateFrame(
                "active coordinate frame is a desktop capture, not this window; run `window_capture` first"
                    .to_owned(),
            )),
        }
    }
}

fn find_matching_ref(map: &HashMap<String, AxRef>, runtime_id: &[i32]) -> Option<String> {
    if runtime_id.is_empty() {
        return None;
    }
    map.values()
        .find(|r| r.runtime_id.as_slice() == runtime_id)
        .map(|r| r.ref_id.clone())
}
