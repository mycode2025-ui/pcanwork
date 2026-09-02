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

pub(crate) fn clamp_window_geometry(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    min_width: u32,
    min_height: u32,
) -> (i32, i32, u32, u32) {
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct MonitorInfo {
        size: u32,
        monitor: Rect,
        work: Rect,
        flags: u32,
    }

    unsafe extern "system" {
        fn MonitorFromPoint(point: Point, flags: u32) -> isize;
        fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
    }

    const MONITOR_DEFAULT_TO_NEAREST: u32 = 2;
    let monitor = unsafe { MonitorFromPoint(Point { x, y }, MONITOR_DEFAULT_TO_NEAREST) };
    let mut info = MonitorInfo {
        size: std::mem::size_of::<MonitorInfo>() as u32,
        ..Default::default()
    };
    if monitor == 0 || unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return (x, y, width, height);
    }

    clamp_to_work_area(
        x,
        y,
        width,
        height,
        min_width,
        min_height,
        (
            info.work.left,
            info.work.top,
            info.work.right,
            info.work.bottom,
        ),
    )
}

fn clamp_to_work_area(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    min_width: u32,
    min_height: u32,
    work: (i32, i32, i32, i32),
) -> (i32, i32, u32, u32) {
    let (left, top, right, bottom) = work;
    let work_width = (right - left).max(1) as u32;
    let work_height = (bottom - top).max(1) as u32;
    let minimum_width = min_width.min(work_width);
    let minimum_height = min_height.min(work_height);
    let width = width.clamp(minimum_width, work_width);
    let height = height.clamp(minimum_height, work_height);
    let x = x.clamp(left, right - width as i32);
    let y = y.clamp(top, bottom - height as i32);
    (x, y, width, height)
}

#[cfg(test)]
mod tests {
    use super::clamp_to_work_area;

    #[test]
    fn oversized_saved_window_is_fitted_to_work_area() {
        assert_eq!(
            clamp_to_work_area(-200, -100, 3000, 2000, 920, 540, (0, 0, 1920, 1040)),
            (0, 0, 1920, 1040)
        );
    }

    #[test]
    fn offscreen_saved_position_is_moved_inside_work_area() {
        assert_eq!(
            clamp_to_work_area(4000, 3000, 940, 580, 920, 540, (0, 0, 1920, 1040)),
            (980, 460, 940, 580)
        );
    }
}
