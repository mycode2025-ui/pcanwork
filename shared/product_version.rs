use std::sync::OnceLock;

/// Product version stamped into the final PE file by the packaging script.
/// Keeping it outside Cargo package metadata prevents a version-only release
/// from invalidating every Slint codegen unit.
pub fn current() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            development_version()
                .or_else(executable_version)
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").into())
        })
        .as_str()
}

/// Debug binaries do not carry the release PE resource stamp. Resolve the
/// workspace product version instead of exposing the intentionally stable
/// Cargo package version (currently 0.3.2) to the UI and update checker.
fn development_version() -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }

    let mut candidates = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("product-version.txt"));
    }
    if let Ok(executable) = std::env::current_exe() {
        if let Some(workspace) = executable
            .parent()
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::parent)
        {
            candidates.push(workspace.join("product-version.txt"));
        }
    }

    candidates.into_iter().find_map(|path| {
        let value = std::fs::read_to_string(path).ok()?;
        let value = value.trim();
        is_three_part_version(value).then(|| value.to_owned())
    })
}

fn is_three_part_version(value: &str) -> bool {
    let mut parts = value.split('.');
    matches!(parts.next(), Some(part) if !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && matches!(parts.next(), Some(part) if !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && matches!(parts.next(), Some(part) if !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && parts.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::{current, is_three_part_version};

    #[test]
    fn accepts_product_semver_and_rejects_truncated_or_decorated_values() {
        assert!(is_three_part_version("0.3.20"));
        assert!(!is_three_part_version("0.3"));
        assert!(!is_three_part_version("v0.3.20"));
        assert!(!is_three_part_version("0.3.20-beta"));
    }

    #[test]
    fn debug_binary_uses_workspace_product_version() {
        let expected = std::fs::read_to_string("product-version.txt").unwrap();
        assert_eq!(current(), expected.trim());
    }
}

#[cfg(windows)]
fn executable_version() -> Option<String> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    #[repr(C)]
    struct FixedFileInfo {
        signature: u32,
        struct_version: u32,
        file_version_ms: u32,
        file_version_ls: u32,
        product_version_ms: u32,
        product_version_ls: u32,
        file_flags_mask: u32,
        file_flags: u32,
        file_os: u32,
        file_type: u32,
        file_subtype: u32,
        file_date_ms: u32,
        file_date_ls: u32,
    }

    #[link(name = "version")]
    unsafe extern "system" {
        fn GetFileVersionInfoSizeW(file_name: *const u16, handle: *mut u32) -> u32;
        fn GetFileVersionInfoW(
            file_name: *const u16,
            handle: u32,
            length: u32,
            data: *mut c_void,
        ) -> i32;
        fn VerQueryValueW(
            block: *const c_void,
            sub_block: *const u16,
            value: *mut *mut c_void,
            length: *mut u32,
        ) -> i32;
    }

    let path = std::env::current_exe().ok()?;
    let path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut ignored = 0;
    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), &mut ignored) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    if unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, data.as_mut_ptr().cast()) } == 0 {
        return None;
    }
    let root = ['\\' as u16, 0];
    let mut value = std::ptr::null_mut();
    let mut length = 0;
    if unsafe { VerQueryValueW(data.as_ptr().cast(), root.as_ptr(), &mut value, &mut length) } == 0
        || value.is_null()
        || length < std::mem::size_of::<FixedFileInfo>() as u32
    {
        return None;
    }
    let info = unsafe { &*value.cast::<FixedFileInfo>() };
    if info.signature != 0xFEEF_04BD {
        return None;
    }
    Some(format!(
        "{}.{}.{}",
        info.product_version_ms >> 16,
        info.product_version_ms & 0xFFFF,
        info.product_version_ls >> 16
    ))
}

#[cfg(not(windows))]
fn executable_version() -> Option<String> {
    None
}
