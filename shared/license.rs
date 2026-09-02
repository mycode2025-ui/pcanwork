use chrono::{DateTime, Local};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FORMAT: &str = "pcanlic-ed25519-v1";
const MACHINE_DOMAIN: &[u8] = b"PcanWork.Modbus.CpuId.v1";
const PUBLIC_KEY_MASK: u8 = 0xA7;
const MASKED_PUBLIC_KEY: [u8; 32] = [
    0xA7, 0x0E, 0x17, 0xFB, 0x80, 0x34, 0x33, 0x66, 0x23, 0x60, 0x5B, 0xE1, 0xBB, 0xEA, 0x42, 0xB2,
    0x1E, 0x5C, 0xE6, 0xD7, 0xE1, 0x0E, 0x59, 0xBB, 0xE6, 0x64, 0x76, 0x96, 0x4F, 0x58, 0xAF, 0xDA,
];

pub(crate) const TRIAL_DURATION: Duration = Duration::from_secs(365 * 24 * 60 * 60);

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LicensePayload {
    pub version: u32,
    pub license_id: String,
    pub machine_code: String,
    pub products: Vec<String>,
    pub features: Vec<String>,
    pub issued_at: u64,
    pub expires_at: u64,
    pub nonce: String,
}

#[derive(Deserialize)]
struct SignedEnvelope {
    format: String,
    payload: String,
    signature: String,
}

#[cfg(not(debug_assertions))]
#[derive(Deserialize)]
struct IntegrityPayload {
    version: u32,
    product: String,
    app_version: String,
    file_name: String,
    sha256: String,
}

pub(crate) struct RuntimeGate {
    product: &'static str,
    deadline: Instant,
}

impl RuntimeGate {
    pub(crate) fn new(product: &'static str, duration: Duration) -> Self {
        Self {
            product,
            deadline: Instant::now() + duration,
        }
    }

    pub(crate) fn remaining_seconds(&self) -> u64 {
        seconds_remaining(self.deadline, Instant::now())
    }

    pub(crate) fn has_signed_license(&self) -> bool {
        verify_installed(self.product, "*").is_ok()
    }

    pub(crate) fn allows(&self, feature: &str) -> bool {
        self.remaining_seconds() > 0 || verify_installed(self.product, feature).is_ok()
    }

    pub(crate) fn product(&self) -> &'static str {
        self.product
    }
}

pub(crate) fn runtime_trial_duration() -> Duration {
    #[cfg(debug_assertions)]
    for name in ["PCANWORK_TRIAL_SECONDS", "MODBUS_TRIAL_SECONDS"] {
        if let Some(seconds) = std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|seconds| (1..=TRIAL_DURATION.as_secs()).contains(seconds))
        {
            return Duration::from_secs(seconds);
        }
    }
    TRIAL_DURATION
}

pub(crate) fn machine_code() -> String {
    static MACHINE_CODE: OnceLock<String> = OnceLock::new();
    MACHINE_CODE
        .get_or_init(|| machine_code_from_cpu_id(&processor_id()))
        .clone()
}

pub(crate) fn install_license(source: &Path, product: &str) -> Result<LicensePayload, String> {
    let text =
        std::fs::read_to_string(source).map_err(|error| format!("read license failed: {error}"))?;
    let payload = verify_license_text(&text, product, "*")?;
    let target = installed_license_path();
    let parent = target
        .parent()
        .ok_or_else(|| "invalid license directory".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create license directory failed: {error}"))?;
    let temporary = target.with_extension("pcanlic.tmp");
    std::fs::write(&temporary, text).map_err(|error| format!("write license failed: {error}"))?;
    if target.exists() {
        std::fs::remove_file(&target)
            .map_err(|error| format!("replace license failed: {error}"))?;
    }
    std::fs::rename(&temporary, &target)
        .map_err(|error| format!("activate license failed: {error}"))?;
    Ok(payload)
}

pub(crate) fn verify_installed(product: &str, feature: &str) -> Result<LicensePayload, String> {
    let text = std::fs::read_to_string(installed_license_path())
        .map_err(|_| "license file not installed".to_string())?;
    verify_license_text(&text, product, feature)
}

pub(crate) fn installed_license_path() -> PathBuf {
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(base).join("PcanWork").join("license.pcanlic");
    }
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("license.pcanlic")
}

pub(crate) fn verify_license_text(
    text: &str,
    product: &str,
    feature: &str,
) -> Result<LicensePayload, String> {
    let envelope: SignedEnvelope =
        serde_json::from_str(text).map_err(|error| format!("invalid license format: {error}"))?;
    if envelope.format != FORMAT {
        return Err("unsupported license format".to_string());
    }
    let payload_bytes = decode_hex(&envelope.payload)?;
    let signature_bytes = decode_hex(&envelope.signature)?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|_| "invalid license signature length".to_string())?;
    let public_key = verifying_key()?;
    public_key
        .verify_strict(&payload_bytes, &signature)
        .map_err(|_| "license signature verification failed".to_string())?;

    let payload: LicensePayload = serde_json::from_slice(&payload_bytes)
        .map_err(|error| format!("invalid license payload: {error}"))?;
    if payload.version != 1 {
        return Err("unsupported license payload version".to_string());
    }
    if !license_matches_machine(&payload.machine_code, &machine_code()) {
        return Err("license does not match this CPU".to_string());
    }
    if !payload
        .products
        .iter()
        .any(|item| item.eq_ignore_ascii_case(product) || item == "*")
    {
        return Err(format!("license does not include product {product}"));
    }
    if feature != "*"
        && !payload
            .features
            .iter()
            .any(|item| item.eq_ignore_ascii_case(feature) || item == "*")
    {
        return Err(format!("license does not include feature {feature}"));
    }
    if payload.expires_at != 0 && unix_time()? > payload.expires_at {
        return Err("license has expired".to_string());
    }
    Ok(payload)
}

#[cfg(not(debug_assertions))]
pub(crate) fn verify_self_integrity(product: &str, app_version: &str) -> Result<(), String> {
    const INTEGRITY_FORMAT: &str = "pcanwork-integrity-ed25519-v1";
    let executable =
        std::env::current_exe().map_err(|error| format!("locate executable failed: {error}"))?;
    let manifest_path = executable.with_extension("exe.integrity");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|error| format!("read integrity manifest failed: {error}"))?;
    let envelope: SignedEnvelope = serde_json::from_str(&text)
        .map_err(|error| format!("invalid integrity manifest: {error}"))?;
    if envelope.format != INTEGRITY_FORMAT {
        return Err("unsupported integrity manifest format".to_string());
    }
    let payload_bytes = verify_signature(&envelope)?;
    let payload: IntegrityPayload = serde_json::from_slice(&payload_bytes)
        .map_err(|error| format!("invalid integrity payload: {error}"))?;
    let file_name = executable
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "invalid executable file name".to_string())?;
    if payload.version != 1
        || !payload.product.eq_ignore_ascii_case(product)
        || payload.app_version != app_version
        || !payload.file_name.eq_ignore_ascii_case(file_name)
    {
        return Err("integrity manifest does not match this application".to_string());
    }
    let bytes =
        std::fs::read(&executable).map_err(|error| format!("read executable failed: {error}"))?;
    let actual = hex_upper(&Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(&payload.sha256) {
        return Err("application integrity check failed".to_string());
    }
    Ok(())
}

#[cfg(debug_assertions)]
pub(crate) fn verify_self_integrity(_product: &str, _app_version: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(not(debug_assertions))]
fn verify_signature(envelope: &SignedEnvelope) -> Result<Vec<u8>, String> {
    let payload_bytes = decode_hex(&envelope.payload)?;
    let signature_bytes = decode_hex(&envelope.signature)?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|_| "invalid signature length".to_string())?;
    verifying_key()?
        .verify_strict(&payload_bytes, &signature)
        .map_err(|_| "signature verification failed".to_string())?;
    Ok(payload_bytes)
}

#[cfg(not(debug_assertions))]
fn hex_upper(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(output, "{byte:02X}");
    }
    output
}

pub(crate) fn seconds_remaining(deadline: Instant, now: Instant) -> u64 {
    let remaining = deadline.saturating_duration_since(now);
    remaining.as_millis().div_ceil(1000) as u64
}

pub(crate) fn format_remaining(seconds: u64) -> String {
    const DAY: u64 = 24 * 60 * 60;
    if seconds >= DAY {
        let days = seconds / DAY;
        let hours = (seconds % DAY) / (60 * 60);
        format!("{days}d {hours:02}h")
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

pub(crate) fn license_validity(payload: &LicensePayload, english: bool) -> String {
    if payload.expires_at == 0 {
        return if english {
            "Permanent license".to_string()
        } else {
            "永久有效".to_string()
        };
    }

    let total = payload.expires_at.saturating_sub(payload.issued_at);
    let remaining = payload
        .expires_at
        .saturating_sub(unix_time().unwrap_or(payload.expires_at));
    let expiry = i64::try_from(payload.expires_at)
        .ok()
        .and_then(|timestamp| DateTime::from_timestamp(timestamp, 0))
        .map(|utc| {
            utc.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| payload.expires_at.to_string());
    let total_text = human_duration(total, english);
    let remaining_text = human_duration(remaining, english);

    if english {
        format!("Valid for {total_text} · Expires {expiry} · {remaining_text} remaining")
    } else {
        format!("有效期 {total_text} · {expiry} 到期 · 剩余 {remaining_text}")
    }
}

fn human_duration(seconds: u64, english: bool) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    if seconds >= DAY {
        let days = seconds / DAY;
        let hours = (seconds % DAY) / HOUR;
        if english {
            if hours == 0 {
                format!("{days} day{}", if days == 1 { "" } else { "s" })
            } else {
                format!("{days}d {hours}h")
            }
        } else if hours == 0 {
            format!("{days} 天")
        } else {
            format!("{days} 天 {hours} 小时")
        }
    } else if seconds >= HOUR {
        let hours = seconds / HOUR;
        let minutes = (seconds % HOUR) / MINUTE;
        if english {
            if minutes == 0 {
                format!("{hours}h")
            } else {
                format!("{hours}h {minutes}m")
            }
        } else if minutes == 0 {
            format!("{hours} 小时")
        } else {
            format!("{hours} 小时 {minutes} 分钟")
        }
    } else {
        let minutes = seconds.div_ceil(MINUTE);
        if english {
            format!("{minutes}m")
        } else {
            format!("{minutes} 分钟")
        }
    }
}

fn verifying_key() -> Result<VerifyingKey, String> {
    let mut raw = [0u8; 32];
    for (target, masked) in raw.iter_mut().zip(MASKED_PUBLIC_KEY) {
        *target = masked ^ PUBLIC_KEY_MASK;
    }
    VerifyingKey::from_bytes(&raw).map_err(|_| "embedded public key is invalid".to_string())
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    let clean: Vec<u8> = value
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace() && *byte != b'-')
        .collect();
    if !clean.len().is_multiple_of(2) {
        return Err("hex field has an odd length".to_string());
    }
    clean
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_nibble(pair[0])?;
            let low = hex_nibble(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err("hex field contains an invalid character".to_string()),
    }
}

fn unix_time() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "system clock is before Unix epoch".to_string())
}

fn machine_code_from_cpu_id(cpu_id: &str) -> String {
    let normalized_id = normalized(cpu_id);
    let mut hash = Sha256::new();
    hash.update(MACHINE_DOMAIN);
    hash.update([0]);
    hash.update(normalized_id.as_bytes());
    grouped_hex(&hash.finalize()[..8])
}

fn grouped_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2 + bytes.len() / 2);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && index % 2 == 0 {
            output.push('-');
        }
        use std::fmt::Write;
        let _ = write!(output, "{byte:02X}");
    }
    output
}

fn normalized(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_uppercase)
        .collect()
}

fn license_matches_machine(licensed: &str, current: &str) -> bool {
    licensed.trim() == "*" || normalized(licensed) == normalized(current)
}

#[cfg(windows)]
fn processor_id() -> String {
    processor_id_from_cpuid().unwrap_or_else(processor_fallback)
}

#[cfg(not(windows))]
fn processor_id() -> String {
    processor_fallback()
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn processor_id_from_cpuid() -> Option<String> {
    let signature = std::arch::x86_64::__cpuid(1);
    Some(format!("{:08X}{:08X}", signature.edx, signature.eax))
}

#[cfg(all(windows, target_arch = "x86"))]
fn processor_id_from_cpuid() -> Option<String> {
    let signature = std::arch::x86::__cpuid(1);
    Some(format!("{:08X}{:08X}", signature.edx, signature.eax))
}

#[cfg(all(windows, not(any(target_arch = "x86", target_arch = "x86_64"))))]
fn processor_id_from_cpuid() -> Option<String> {
    None
}

#[cfg(target_arch = "x86_64")]
fn processor_fallback() -> String {
    let vendor = std::arch::x86_64::__cpuid(0);
    let signature = std::arch::x86_64::__cpuid(1);
    format!(
        "{:08X}{:08X}{:08X}{:08X}{:08X}",
        vendor.ebx, vendor.edx, vendor.ecx, signature.eax, signature.edx
    )
}

#[cfg(target_arch = "x86")]
fn processor_fallback() -> String {
    let vendor = std::arch::x86::__cpuid(0);
    let signature = std::arch::x86::__cpuid(1);
    format!(
        "{:08X}{:08X}{:08X}{:08X}{:08X}",
        vendor.ebx, vendor.edx, vendor.ecx, signature.eax, signature.edx
    )
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn processor_fallback() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn native_processor_id_matches_windows_processor_id_layout() {
        let signature = std::arch::x86_64::__cpuid(1);
        let expected = format!("{:08X}{:08X}", signature.edx, signature.eax);
        assert_eq!(processor_id_from_cpuid().as_deref(), Some(expected.as_str()));
        assert_eq!(expected.len(), 16);
    }

    #[test]
    fn malformed_or_unsigned_license_is_rejected() {
        assert!(verify_license_text("{}", "pcanwork", "*").is_err());
        assert!(
            verify_license_text(
                r#"{"format":"pcanlic-ed25519-v1","payload":"00","signature":"00"}"#,
                "pcanwork",
                "*",
            )
            .is_err()
        );
    }

    #[test]
    fn countdown_rounds_up() {
        let start = Instant::now();
        let deadline = start + TRIAL_DURATION;
        assert_eq!(seconds_remaining(deadline, start), 365 * 24 * 60 * 60);
        assert_eq!(format_remaining(TRIAL_DURATION.as_secs()), "365d 00h");
        assert_eq!(format_remaining(3600), "60:00");
    }

    #[test]
    fn wildcard_machine_license_is_portable_but_specific_license_is_not() {
        assert!(license_matches_machine("*", "AAAA-BBBB-CCCC-DDDD"));
        assert!(license_matches_machine(
            "aaaa-bbbb-cccc-dddd",
            "AAAA-BBBB-CCCC-DDDD"
        ));
        assert!(!license_matches_machine(
            "1111-2222-3333-4444",
            "AAAA-BBBB-CCCC-DDDD"
        ));
    }

    #[test]
    fn license_duration_is_human_readable() {
        assert_eq!(human_duration(30 * 24 * 60 * 60, false), "30 天");
        assert_eq!(
            human_duration(29 * 24 * 60 * 60 + 23 * 60 * 60, false),
            "29 天 23 小时"
        );
        assert_eq!(human_duration(60 * 60, true), "1h");
        assert_eq!(human_duration(59 * 60, true), "59m");
    }

    #[test]
    fn externally_signed_unbound_fixture_is_accepted_when_requested() {
        let Some(path) = std::env::var_os("PCANWORK_TEST_LICENSE") else {
            return;
        };
        let text = std::fs::read_to_string(path).expect("read external license fixture");
        let payload = verify_license_text(&text, "pcanwork", "*")
            .expect("verify externally signed unbound license");
        assert_eq!(payload.machine_code, "*");
    }
}
