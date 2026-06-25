#[cfg(windows)]
pub(crate) fn force_system_dpi_awareness() {
    use std::ffi::{c_char, c_void};

    type SetProcessDpiAwarenessContext = unsafe extern "system" fn(isize) -> i32;
    type SetProcessDpiAwareness = unsafe extern "system" fn(i32) -> i32;
    type SetProcessDpiAware = unsafe extern "system" fn() -> i32;

    unsafe extern "system" {
        fn LoadLibraryA(name: *const c_char) -> isize;
        fn GetProcAddress(module: isize, name: *const c_char) -> *const c_void;
    }

    unsafe fn load_symbol(module: &[u8], symbol: &[u8]) -> *const c_void {
        let module = unsafe { LoadLibraryA(module.as_ptr() as *const c_char) };
        if module == 0 {
            return std::ptr::null();
        }
        unsafe { GetProcAddress(module, symbol.as_ptr() as *const c_char) }
    }

    unsafe {
        const DPI_AWARENESS_CONTEXT_SYSTEM_AWARE: isize = -2;
        const PROCESS_SYSTEM_DPI_AWARE: i32 = 1;

        let set_context = load_symbol(b"user32.dll\0", b"SetProcessDpiAwarenessContext\0");
        if !set_context.is_null() {
            let set_context: SetProcessDpiAwarenessContext = std::mem::transmute(set_context);
            if set_context(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) != 0 {
                return;
            }
        }

        let set_awareness = load_symbol(b"shcore.dll\0", b"SetProcessDpiAwareness\0");
        if !set_awareness.is_null() {
            let set_awareness: SetProcessDpiAwareness = std::mem::transmute(set_awareness);
            if set_awareness(PROCESS_SYSTEM_DPI_AWARE) == 0 {
                return;
            }
        }

        let set_aware = load_symbol(b"user32.dll\0", b"SetProcessDPIAware\0");
        if !set_aware.is_null() {
            let set_aware: SetProcessDpiAware = std::mem::transmute(set_aware);
            let _ = set_aware();
        }
    }
}
