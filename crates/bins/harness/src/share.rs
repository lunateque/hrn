//! Sealed session sharing (`/share [--no-redact]` and `hrn share [session_id] [--no-redact]`).
//!
//! Snapshots the active branch (`{ version, meta, system_prompt, nodes }`), redacts known secret
//! patterns unless `--no-redact`, trims oversize payloads in strict deterministic order
//! (inline images -> progressive string cap 32768 -> 8192 -> 2048 -> 512 -> oldest entries),
//! seals with gzip + AES-256-GCM, and uploads strictly ciphertext to `[share] store`.

use crate::cli::ShareArgs;
use crate::config::{Config, ShareConfig, ShareStoreKind};
use crate::resume::{find_session, latest_in_cwd};
use crate::term::out;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use hrn_edge_http::redact::redact_json;
use hrn_edge_http::seal::{KEY_LEN, encode_key, format_share_url, generate_key, is_valid_id, seal};
use hrn_edge_http::{HttpClient, guard};
use hrn_edge_proc::{RunSpec, run_streaming};
use hrn_kernel::{CancelFlag, DagNode, DagPayload, HrnError, Result, SessionStore, Usage};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// Default sealed share blob size cap (1 MiB, matching `hrn-relay`'s `POST /s` ceiling).
pub(crate) const DEFAULT_MAX_SEALED_BYTES: usize = 1024 * 1024;
/// Progressive string truncation ladder enforced when a snapshot exceeds the byte budget.
const STRING_TRUNCATION_LADDER: [usize; 4] = [32_768, 8_192, 2_048, 512];
/// Marker substituted for inline image base64 payloads during Stage 1 oversize trimming.
pub(crate) const IMAGE_OMITTED_MARKER: &str = "[image omitted]";

/// Session metadata embedded inside a [`ShareSnapshot`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ShareMeta {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    pub(crate) usage: Usage,
    pub(crate) redacted: bool,
    pub(crate) trimmed: bool,
}

/// Self-contained JSON transcript snapshot sealed before upload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ShareSnapshot {
    pub(crate) version: u32,
    pub(crate) meta: ShareMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) system_prompt: Option<String>,
    pub(crate) nodes: Vec<DagNode>,
}

/// Record of which oversize-trimming stages were required to fit inside the byte budget.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TrimReport {
    pub(crate) stripped_images: bool,
    pub(crate) string_cap: Option<usize>,
    pub(crate) dropped_nodes: usize,
}

/// Output of [`prepare_sealed_share`]: opaque ciphertext ready for upload plus trim telemetry.
#[derive(Clone, Debug)]
pub(crate) struct PreparedShare {
    pub(crate) sealed: Vec<u8>,
    pub(crate) report: TrimReport,
}

/// Build an unredacted [`ShareSnapshot`] from the active branch of `session_id`.
pub(crate) fn build_snapshot(
    store: &dyn SessionStore,
    session_id: &str,
    system_prompt: Option<String>,
    redacted: bool,
) -> Result<ShareSnapshot> {
    let meta = store
        .get_session(session_id)?
        .ok_or_else(|| HrnError::Store(format!("unknown session `{session_id}`")))?;
    let Some(head_id) = meta.head_node_id.as_deref() else {
        return Err(HrnError::Config(format!(
            "session `{session_id}` has no messages yet"
        )));
    };
    let nodes = store.load_branch(session_id, head_id)?;
    let usage = store.session_usage(session_id)?;
    Ok(ShareSnapshot {
        version: 1,
        meta: ShareMeta {
            id: meta.id,
            title: meta.title,
            model: meta.model,
            created_at_ms: meta.created_at_ms,
            updated_at_ms: meta.updated_at_ms,
            usage,
            redacted,
            trimmed: false,
        },
        system_prompt,
        nodes,
    })
}

/// Redact (unless disabled), trim oversize payloads in strict order, and seal `snapshot` with
/// AES-256-GCM under `key` to fit within `max_sealed_bytes`.
pub(crate) fn prepare_sealed_share(
    mut snapshot: ShareSnapshot,
    redact: bool,
    max_sealed_bytes: usize,
    key: &[u8; KEY_LEN],
) -> Result<PreparedShare> {
    snapshot.meta.redacted = redact;
    if redact {
        let mut val = serde_json::to_value(&snapshot)?;
        redact_json(&mut val);
        snapshot = serde_json::from_value(val)?;
    }

    let mut report = TrimReport::default();
    let mut sealed = seal_snapshot(&snapshot, key)?;
    if sealed.len() <= max_sealed_bytes {
        return Ok(PreparedShare { sealed, report });
    }

    // Stage 1: Strip inline image base64 payloads first.
    if strip_inline_images(&mut snapshot) {
        report.stripped_images = true;
        snapshot.meta.trimmed = true;
        sealed = seal_snapshot(&snapshot, key)?;
        if sealed.len() <= max_sealed_bytes {
            return Ok(PreparedShare { sealed, report });
        }
    }

    // Stage 2: Truncate long strings progressively (32768 -> 8192 -> 2048 -> 512).
    for cap in STRING_TRUNCATION_LADDER {
        if truncate_snapshot_strings(&mut snapshot, cap) {
            report.string_cap = Some(cap);
            snapshot.meta.trimmed = true;
            sealed = seal_snapshot(&snapshot, key)?;
            if sealed.len() <= max_sealed_bytes {
                return Ok(PreparedShare { sealed, report });
            }
        }
    }

    // Stage 3: Drop oldest transcript entries until within cap.
    while sealed.len() > max_sealed_bytes && !snapshot.nodes.is_empty() {
        snapshot.nodes.remove(0);
        report.dropped_nodes += 1;
        snapshot.meta.trimmed = true;
        sealed = seal_snapshot(&snapshot, key)?;
    }
    if sealed.len() > max_sealed_bytes && snapshot.system_prompt.take().is_some() {
        snapshot.meta.trimmed = true;
        sealed = seal_snapshot(&snapshot, key)?;
    }
    if sealed.len() > max_sealed_bytes {
        return Err(HrnError::Config(format!(
            "sealed share still exceeds {max_sealed_bytes}-byte budget after trimming"
        )));
    }
    Ok(PreparedShare { sealed, report })
}

fn seal_snapshot(snapshot: &ShareSnapshot, key: &[u8; KEY_LEN]) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(snapshot)?;
    seal(&json, key)
}

fn strip_inline_images(snapshot: &mut ShareSnapshot) -> bool {
    let mut changed = false;
    for node in &mut snapshot.nodes {
        if let DagPayload::Message { msg, .. } = &mut node.payload {
            for img in &mut msg.images {
                if img.data_base64 != IMAGE_OMITTED_MARKER {
                    IMAGE_OMITTED_MARKER.clone_into(&mut img.data_base64);
                    changed = true;
                }
            }
        }
    }
    changed
}

fn truncate_snapshot_strings(snapshot: &mut ShareSnapshot, max_chars: usize) -> bool {
    let mut changed = false;
    if let Some(sys) = &mut snapshot.system_prompt {
        changed |= truncate_in_place(sys, max_chars);
    }
    for node in &mut snapshot.nodes {
        match &mut node.payload {
            DagPayload::Message { msg, .. } => {
                changed |= truncate_in_place(&mut msg.content, max_chars);
                if let Some(r) = &mut msg.reasoning {
                    changed |= truncate_in_place(r, max_chars);
                }
                for tb in &mut msg.thinking_blocks {
                    changed |= truncate_in_place(&mut tb.text, max_chars);
                }
                for tc in &mut msg.tool_calls {
                    changed |= truncate_in_place(&mut tc.arguments, max_chars);
                }
            }
            DagPayload::Summary { text, .. }
            | DagPayload::BranchSummary { text, .. }
            | DagPayload::Label {
                label: Some(text), ..
            } => {
                changed |= truncate_in_place(text, max_chars);
            }
            DagPayload::ForkMarker { label } => {
                changed |= truncate_in_place(label, max_chars);
            }
            _ => {}
        }
    }
    changed
}

fn truncate_in_place(text: &mut String, max_chars: usize) -> bool {
    let Some((cut, _)) = text.char_indices().nth(max_chars) else {
        return false;
    };
    text.truncate(cut);
    text.push_str("... [truncated]");
    true
}

/// Upload opaque sealed bytes to `<server_base>/s` and return `<server_base>/s/<id>#<b64url_key>`.
pub(crate) async fn publish_blob(
    server_base: &str,
    sealed: &[u8],
    key: &[u8; KEY_LEN],
) -> Result<String> {
    let base = server_base.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(HrnError::Config(
            "no share server configured · set `[share] server = \"http://127.0.0.1:8787\"` or `[share] store = \"gist\"`"
                .to_owned(),
        ));
    }
    let post_url = format!("{base}/s");
    guard::check_outbound_url(&post_url)?;
    let client = HttpClient::new()?;
    let headers = vec![(
        "content-type".to_owned(),
        "application/octet-stream".to_owned(),
    )];
    let resp = client
        .post_raw(
            &post_url,
            &headers,
            Bytes::copy_from_slice(sealed),
            Duration::from_secs(30),
        )
        .await?;
    if !resp.status.is_success() {
        return Err(HrnError::Tool(format!(
            "share upload failed with HTTP {}: {}",
            resp.status.as_u16(),
            resp.text()
        )));
    }
    let parsed: serde_json::Value = serde_json::from_slice(&resp.body)
        .map_err(|e| HrnError::Tool(format!("share server returned invalid JSON: {e}")))?;
    let id = parsed
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| is_valid_id(id))
        .ok_or_else(|| HrnError::Tool("share server response missing valid `id`".to_owned()))?;
    format_share_url(base, id, key)
}

/// RAII guard guaranteeing temporary share files are deleted even on cancellation or early error.
struct TempFileGuard(std::path::PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn write_private_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .map_err(|e| HrnError::Io(format!("create temp gist file: {e}")))?;
    file.write_all(bytes)
        .map_err(|e| HrnError::Io(format!("write temp gist file: {e}")))?;
    file.flush()
        .map_err(|e| HrnError::Io(format!("flush temp gist file: {e}")))
}

/// Upload base64-encoded ciphertext to a secret GitHub Gist via `gh gist create` (safe argv,
/// zero shell interpolation, never `--public`).
pub(crate) async fn publish_gist(sealed: &[u8], key: &[u8; KEY_LEN]) -> Result<String> {
    let rand_id = hrn_edge_http::seal::generate_room_id()?;
    let tmp_dir = std::env::temp_dir();
    let tmp_path = tmp_dir.join(format!("hrn-share-{rand_id}.sealed.b64"));
    let encoded = B64.encode(sealed);
    write_private_new_file(&tmp_path, encoded.as_bytes())?;
    let _guard = TempFileGuard(tmp_path.clone());

    let spec = RunSpec {
        program: "gh".to_owned(),
        args: vec![
            "gist".to_owned(),
            "create".to_owned(),
            "--desc".to_owned(),
            "hrn sealed share".to_owned(),
            tmp_path.to_string_lossy().into_owned(),
        ],
        cwd: tmp_dir,
        env: BTreeMap::new(),
        timeout: Duration::from_mins(1),
        max_output_bytes: 32 * 1024,
    };
    let out = run_streaming(&spec, &CancelFlag::new(), |_| {}).await?;
    if out.exit_code != Some(0) {
        return Err(HrnError::Tool(format!(
            "`gh gist create` failed (exit {:?}): {}",
            out.exit_code,
            out.stderr.trim()
        )));
    }
    let gist_url = out
        .stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with("https://") || l.starts_with("http://"))
        .ok_or_else(|| {
            HrnError::Tool(format!(
                "`gh gist create` did not output a gist URL: {}",
                out.stdout.trim()
            ))
        })?;
    Ok(format!("{gist_url}#{}", encode_key(key)))
}

/// End-to-end sealed share flow: snapshot -> redact -> trim -> seal -> upload -> link.
pub(crate) async fn share_session(
    store: &dyn SessionStore,
    share_cfg: &ShareConfig,
    session_id: &str,
    system_prompt: Option<String>,
    no_redact: bool,
    _cwd: &Path,
) -> Result<(String, TrimReport)> {
    let redact = share_cfg.redact && !no_redact;
    let snapshot = build_snapshot(store, session_id, system_prompt, redact)?;
    let key = generate_key()?;
    let prepared = prepare_sealed_share(snapshot, redact, DEFAULT_MAX_SEALED_BYTES, &key)?;
    let url = match share_cfg.store {
        ShareStoreKind::Blob => publish_blob(&share_cfg.server, &prepared.sealed, &key).await?,
        ShareStoreKind::Gist => publish_gist(&prepared.sealed, &key).await?,
    };
    Ok((url, prepared.report))
}

/// CLI entry point for `hrn share [session_id] [--no-redact]`.
pub(crate) async fn run_cli(
    store: &dyn SessionStore,
    cfg: &Config,
    cwd: &Path,
    args: &ShareArgs,
) -> Result<()> {
    let session_id = if let Some(wanted) = args
        .session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        find_session(store, wanted)?
    } else {
        let list = store.list_sessions(usize::MAX)?;
        latest_in_cwd(&list, cwd)
            .or_else(|| {
                list.into_iter()
                    .find(|s| s.head_node_id.is_some())
                    .map(|s| s.id)
            })
            .ok_or_else(|| HrnError::Store("no non-empty sessions to share".to_owned()))?
    };
    let (url, report) =
        share_session(store, &cfg.share, &session_id, None, args.no_redact, cwd).await?;
    out(&format!("{url}\n"));
    if report.stripped_images || report.string_cap.is_some() || report.dropped_nodes > 0 {
        out(&format!(
            "(trimmed: images_omitted={}, string_cap={:?}, dropped_nodes={})\n",
            report.stripped_images, report.string_cap, report.dropped_nodes
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_edge_http::seal::unseal;
    use hrn_kernel::{ImageAttachment, Msg};

    /// Deterministic incompressible ASCII text so gzip cannot collapse string payloads away.
    fn pseudo_random_text(len: usize, seed: u64) -> String {
        let mut x = seed | 1;
        let alphabet = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 ";
        let mut out = String::with_capacity(len);
        for _ in 0..len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let idx = usize::try_from(x % (alphabet.len() as u64)).unwrap_or(0);
            out.push(char::from(alphabet[idx]));
        }
        out
    }

    fn sample_node(id: &str, parent: Option<&str>, msg: Msg) -> DagNode {
        DagNode {
            id: id.to_owned(),
            session_id: "s_trim".to_owned(),
            parent_id: parent.map(str::to_owned),
            created_at_ms: 1,
            payload: DagPayload::Message {
                turn_id: "t1".to_owned(),
                msg,
            },
        }
    }

    fn make_oversize_snapshot() -> ShareSnapshot {
        let img = ImageAttachment {
            media_type: "image/png".to_owned(),
            data_base64: pseudo_random_text(16_000, 101),
            label: Some("shot.png".to_owned()),
            width: Some(100),
            height: Some(100),
        };
        let n1 = sample_node(
            "n1",
            None,
            Msg::user_with_images(pseudo_random_text(12_000, 202), vec![img]),
        );
        let n2 = sample_node(
            "n2",
            Some("n1"),
            Msg::assistant(pseudo_random_text(12_000, 303)),
        );
        let n3 = sample_node(
            "n3",
            Some("n2"),
            Msg::user("newest short question".to_owned()),
        );
        let n4 = sample_node(
            "n4",
            Some("n3"),
            Msg::assistant("newest short answer".to_owned()),
        );
        ShareSnapshot {
            version: 1,
            meta: ShareMeta {
                id: "s_trim".to_owned(),
                title: "trim test".to_owned(),
                model: Some("flash".to_owned()),
                created_at_ms: 1,
                updated_at_ms: 2,
                usage: Usage::default(),
                redacted: true,
                trimmed: false,
            },
            system_prompt: Some("sys".to_owned()),
            nodes: vec![n1, n2, n3, n4],
        }
    }

    #[test]
    fn share_trim_order() {
        let key = generate_key().expect("key");
        let base = make_oversize_snapshot();

        // Measure stage 1 size (images stripped, full strings & all 4 nodes kept).
        let mut after_img = base.clone();
        assert!(strip_inline_images(&mut after_img));
        after_img.meta.trimmed = true;
        let stage1_size = seal_snapshot(&after_img, &key).expect("s1").len();

        // Measure stage 2 size at 2048 cap (all 4 nodes kept).
        let mut after_str_2048 = after_img.clone();
        assert!(truncate_snapshot_strings(&mut after_str_2048, 2048));
        let stage2_2048_size = seal_snapshot(&after_str_2048, &key).expect("s2").len();

        // Measure stage 2 minimum size (512 cap, all 4 nodes kept).
        let mut after_str_512 = after_img.clone();
        assert!(truncate_snapshot_strings(&mut after_str_512, 512));
        let stage2_512_size = seal_snapshot(&after_str_512, &key).expect("s2_512").len();

        // 1. Budget allowing Stage 1 only: strips images, leaves strings and all 4 nodes intact.
        let p1 = prepare_sealed_share(base.clone(), false, stage1_size + 64, &key).expect("p1");
        assert!(p1.report.stripped_images);
        assert_eq!(p1.report.string_cap, None);
        assert_eq!(p1.report.dropped_nodes, 0);
        let dec1: ShareSnapshot =
            serde_json::from_slice(&unseal(&p1.sealed, &key).expect("u1")).expect("j1");
        assert_eq!(dec1.nodes.len(), 4);
        if let DagPayload::Message { msg, .. } = &dec1.nodes[0].payload {
            assert_eq!(msg.images[0].data_base64, IMAGE_OMITTED_MARKER);
            assert_eq!(msg.content.len(), 12_000);
        }

        // 2. Tighter budget forces Stage 2 (progressive string cap) while keeping all 4 nodes.
        let p2 =
            prepare_sealed_share(base.clone(), false, stage2_2048_size + 64, &key).expect("p2");
        assert!(p2.report.stripped_images);
        assert!(matches!(p2.report.string_cap, Some(2048 | 8192)));
        assert_eq!(p2.report.dropped_nodes, 0);

        // 3. Tightest budget below Stage 2 minimum forces Stage 3 (drops oldest entries first).
        let p3 = prepare_sealed_share(base, false, stage2_512_size.saturating_sub(256), &key)
            .expect("p3");
        assert!(p3.report.stripped_images);
        assert_eq!(p3.report.string_cap, Some(512));
        assert!(p3.report.dropped_nodes > 0);
        let dec3: ShareSnapshot =
            serde_json::from_slice(&unseal(&p3.sealed, &key).expect("u3")).expect("j3");
        assert_eq!(
            dec3.nodes.last().map(|n| n.id.as_str()),
            Some("n4"),
            "newest entries must survive oldest-first trimming"
        );
    }

    #[test]
    fn temp_gist_file_create_new_and_raii_cleanup() {
        let rand_id = hrn_edge_http::seal::generate_room_id().expect("id");
        let path = std::env::temp_dir().join(format!("hrn-share-test-{rand_id}.tmp"));
        write_private_new_file(&path, b"sealed").expect("create new");
        assert!(write_private_new_file(&path, b"dup").is_err());
        {
            let _guard = TempFileGuard(path.clone());
            assert!(path.exists());
        }
        assert!(!path.exists(), "RAII guard must delete temp file on drop");
    }
}
