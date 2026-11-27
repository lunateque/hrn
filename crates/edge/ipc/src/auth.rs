//! Cryptographic primitives and run-file management for daemon IPC:
//! 256-bit `getrandom` bearer tokens, constant-time comparison, pure-Rust SHA-256 host-key
//! fingerprints, and atomic `run.json` persistence with owner checks.

use hrn_kernel::{HrnError, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

/// Persistent cryptographic identity of a local `hrn` daemon, pinned by remote clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostIdentity {
    /// 256-bit random key material (hex-encoded, never printed to stdout).
    pub secret_hex: String,
    /// Public fingerprint (`SHA256:<64 hex chars>`), safe to display and pin.
    pub fingerprint: String,
}

/// Discovery file written by a running `hrn daemon` (`%LOCALAPPDATA%\harness\run.json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFile {
    /// Process ID of the daemon that owns this run file.
    pub pid: u32,
    /// Bound loopback socket address (`127.0.0.1:<port>`).
    pub addr: String,
    /// Ephemeral 256-bit bearer token required on every `/v1/*` request.
    pub token: String,
    /// Persistent daemon host-key fingerprint (`SHA256:<hex>`).
    pub host_key: String,
    /// Daemon start timestamp in Unix milliseconds.
    pub started_at_ms: u64,
}

impl RunFile {
    /// Parse [`Self::addr`] as a [`SocketAddr`], rejecting non-loopback addresses.
    pub fn loopback_addr(&self) -> Result<SocketAddr> {
        let addr: SocketAddr = self
            .addr
            .parse()
            .map_err(|e| HrnError::Config(format!("invalid daemon addr `{}`: {e}", self.addr)))?;
        if !addr.ip().is_loopback() {
            return Err(HrnError::Auth(format!(
                "refusing non-loopback daemon address `{addr}`"
            )));
        }
        Ok(addr)
    }
}

/// Generate a fresh 256-bit (64 lowercase hex characters) bearer token from the OS CSPRNG.
pub fn generate_token() -> Result<String> {
    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw)
        .map_err(|e| HrnError::Internal(format!("OS CSPRNG failure: {e}")))?;
    Ok(encode_hex(&raw))
}

/// Constant-time byte-slice equality check suitable for bearer tokens and fingerprints.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let max_len = a.len().max(b.len());
    let mut diff: u8 = u8::from(a.len() != b.len());
    for i in 0..max_len {
        let ba = a.get(i).copied().unwrap_or(0);
        let bb = b.get(i).copied().unwrap_or(0);
        diff |= ba ^ bb;
    }
    diff == 0
}

/// Load or initialize the persistent daemon [`HostIdentity`] at `path`.
pub fn ensure_host_identity(path: &Path) -> Result<HostIdentity> {
    if path.exists() {
        let raw = fs::read_to_string(path)?;
        let identity: HostIdentity = serde_json::from_str(&raw).map_err(|e| {
            HrnError::Config(format!("invalid host key file `{}`: {e}", path.display()))
        })?;
        let expected = format!("SHA256:{}", sha256_hex(identity.secret_hex.as_bytes()));
        if !ct_eq(identity.fingerprint.as_bytes(), expected.as_bytes()) {
            return Err(HrnError::Auth(format!(
                "host key file `{}` has mismatched fingerprint",
                path.display()
            )));
        }
        return Ok(identity);
    }
    let secret_hex = generate_token()?;
    let fingerprint = format!("SHA256:{}", sha256_hex(secret_hex.as_bytes()));
    let identity = HostIdentity {
        secret_hex,
        fingerprint,
    };
    write_secret_json(path, &identity)?;
    Ok(identity)
}

/// Atomically persist `run_file` at `path` with restrictive permissions.
pub fn write_run_file(path: &Path, run_file: &RunFile) -> Result<()> {
    write_secret_json(path, run_file)
}

/// Read and parse a [`RunFile`] from `path`.
pub fn read_run_file(path: &Path) -> Result<RunFile> {
    let raw = fs::read_to_string(path).map_err(|e| {
        HrnError::Config(format!(
            "cannot read daemon run file `{}` (is `hrn daemon` running?): {e}",
            path.display()
        ))
    })?;
    serde_json::from_str(&raw).map_err(|e| {
        HrnError::Config(format!(
            "malformed daemon run file `{}`: {e}",
            path.display()
        ))
    })
}

/// Remove `path` only if its stored `pid` still matches `expected_pid`, preventing a shutting-down
/// daemon from clobbering a newer instance's run file.
pub fn remove_run_file_if_owned(path: &Path, expected_pid: u32) {
    if let Ok(existing) = read_run_file(path)
        && existing.pid == expected_pid
    {
        let _ = fs::remove_file(path);
    }
}

/// Current wall-clock time in Unix milliseconds.
pub fn now_unix_ms() -> u64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(dur.as_millis()).unwrap_or(u64::MAX)
}

fn write_secret_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(value)?;
    let tmp_name = format!(
        "{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("hrn-secret"),
        std::process::id()
    );
    let tmp_path = path.with_file_name(tmp_name);
    fs::write(&tmp_path, &data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600));
    }
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Encode raw bytes as lowercase hex.
pub fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        let hi = usize::from(b >> 4);
        let lo = usize::from(b & 0x0f);
        out.push(char::from(HEX_LOWER.get(hi).copied().unwrap_or(b'0')));
        out.push(char::from(HEX_LOWER.get(lo).copied().unwrap_or(b'0')));
    }
    out
}

/// Compute FIPS 180-4 SHA-256 digest of `data` as lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    encode_hex(&sha256(data))
}

const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// Compute FIPS 180-4 SHA-256 digest of `data` in pure safe Rust.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((data.len() + 9).div_ceil(64) * 64);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        sha256_compress(&mut state, chunk);
    }

    let mut out = [0u8; 32];
    for (idx, word) in state.iter().enumerate() {
        let bytes = word.to_be_bytes();
        if let Some(dst) = out.get_mut(idx * 4..idx * 4 + 4) {
            dst.copy_from_slice(&bytes);
        }
    }
    out
}

fn sha256_compress(state: &mut [u32; 8], chunk: &[u8; 64]) {
    let mut schedule = [0u32; 64];
    for (idx, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
        if let Some(slot) = schedule.get_mut(idx) {
            *slot = u32::from_be_bytes(*word);
        }
    }
    for idx in 16..64 {
        let w15 = schedule.get(idx - 15).copied().unwrap_or(0);
        let w2 = schedule.get(idx - 2).copied().unwrap_or(0);
        let w16 = schedule.get(idx - 16).copied().unwrap_or(0);
        let w7 = schedule.get(idx - 7).copied().unwrap_or(0);
        let s0 = w15.rotate_right(7) ^ w15.rotate_right(18) ^ (w15 >> 3);
        let s1 = w2.rotate_right(17) ^ w2.rotate_right(19) ^ (w2 >> 10);
        if let Some(slot) = schedule.get_mut(idx) {
            *slot = w16.wrapping_add(s0).wrapping_add(w7).wrapping_add(s1);
        }
    }

    let [
        mut ra,
        mut rb,
        mut rc,
        mut rd,
        mut re,
        mut rf,
        mut rg,
        mut rh,
    ] = *state;
    for idx in 0..64 {
        let s1 = re.rotate_right(6) ^ re.rotate_right(11) ^ re.rotate_right(25);
        let ch = (re & rf) ^ ((!re) & rg);
        let ki = SHA256_K.get(idx).copied().unwrap_or(0);
        let wi = schedule.get(idx).copied().unwrap_or(0);
        let temp1 = rh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(ki)
            .wrapping_add(wi);
        let s0 = ra.rotate_right(2) ^ ra.rotate_right(13) ^ ra.rotate_right(22);
        let maj = (ra & rb) ^ (ra & rc) ^ (rb & rc);
        let temp2 = s0.wrapping_add(maj);

        rh = rg;
        rg = rf;
        rf = re;
        re = rd.wrapping_add(temp1);
        rd = rc;
        rc = rb;
        rb = ra;
        ra = temp1.wrapping_add(temp2);
    }

    let deltas = [ra, rb, rc, rd, re, rf, rg, rh];
    for (slot, delta) in state.iter_mut().zip(deltas) {
        *slot = slot.wrapping_add(delta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_nist_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn token_and_ct_eq_behave_correctly() {
        let t1 = generate_token().expect("token1");
        let t2 = generate_token().expect("token2");
        assert_eq!(t1.len(), 64);
        assert_ne!(t1, t2);
        assert!(ct_eq(t1.as_bytes(), t1.as_bytes()));
        assert!(!ct_eq(t1.as_bytes(), t2.as_bytes()));
        assert!(!ct_eq(b"short", b"longer"));
    }

    #[test]
    fn host_identity_and_run_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("hrn_ipc_auth_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let key_path = dir.join("host-key.json");
        let id1 = ensure_host_identity(&key_path).expect("create");
        let id2 = ensure_host_identity(&key_path).expect("reload");
        assert_eq!(id1, id2);
        assert!(id1.fingerprint.starts_with("SHA256:"));
        assert_eq!(id1.fingerprint.len(), 7 + 64);

        let run_path = dir.join("run.json");
        let rf = RunFile {
            pid: 42,
            addr: "127.0.0.1:9999".to_owned(),
            token: generate_token().expect("tok"),
            host_key: id1.fingerprint.clone(),
            started_at_ms: 123,
        };
        write_run_file(&run_path, &rf).expect("write");
        assert_eq!(read_run_file(&run_path).expect("read"), rf);
        remove_run_file_if_owned(&run_path, 999);
        assert!(run_path.exists());
        remove_run_file_if_owned(&run_path, 42);
        assert!(!run_path.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
