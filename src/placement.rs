//! 시작할 때 창 위치 복원.
//!
//! eframe도 창 위치를 복원하지만, 배율(DPI)이 다른 모니터가 섞여 있으면
//! (예: 100% 모니터 옆에 150% 4K 모니터) 엉뚱한 모니터 기준으로 계산해서 창이 밀려난다.
//! 그래서 창이 화면에 보이기 전 첫 프레임에 저장된 물리 픽셀 위치로 직접 옮기고,
//! 그 위치가 화면 영역 밖이면(모니터를 뺀 경우 등) 가장 가까운 모니터의 가운데로 옮긴다.

use eframe::egui;

/// eframe이 설정 파일에 창 정보를 저장하는 키
pub const WINDOW_STORAGE_KEY: &str = "window";

/// eframe(egui-winit)이 저장하는 창 정보 중 여기서 쓰는 부분
#[derive(serde::Deserialize, Default, Clone, Copy, Debug)]
#[serde(default)]
pub struct SavedWindow {
    /// 창틀(제목 표시줄 포함)의 왼쪽 위, 물리 픽셀
    pub outer_position_pixels: Option<egui::Pos2>,
    pub fullscreen: bool,
    pub maximized: bool,
}

/// 물리 픽셀 사각형
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PxRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl PxRect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    fn area(&self) -> i64 {
        i64::from(self.width().max(0)) * i64::from(self.height().max(0))
    }

    fn intersect(&self, other: &PxRect) -> PxRect {
        PxRect {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        }
    }

    fn shrink(&self, by: i32) -> PxRect {
        PxRect {
            left: self.left + by,
            top: self.top + by,
            right: self.right - by,
            bottom: self.bottom - by,
        }
    }
}

/// 화면 가장자리에 붙인 창의 보이지 않는 테두리(약 7px) 같은 작은 차이는 봐준다.
const TOLERANCE_PX: i32 = 16;

/// 창이 모니터 영역 안에 다 들어와 있지 않으면 true.
/// 두 모니터에 걸쳐 있는 창은 영역 안으로 본다.
pub fn is_out_of_area(window: PxRect, monitors: &[PxRect]) -> bool {
    let inner = window.shrink(TOLERANCE_PX);
    if inner.area() == 0 || monitors.is_empty() {
        return false;
    }
    let visible: i64 = monitors.iter().map(|m| inner.intersect(m).area()).sum();
    visible < inner.area()
}

/// `width`×`height` 창을 작업 영역 가운데에 둔다. 작업 영역보다 크면 맞게 줄인다.
pub fn centered_in(work: PxRect, width: i32, height: i32) -> PxRect {
    let width = width.min(work.width());
    let height = height.min(work.height());
    let left = work.left + (work.width() - width) / 2;
    let top = work.top + (work.height() - height) / 2;
    PxRect {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

/// 첫 프레임(창이 보이기 전)에 한 번 부른다.
#[cfg(windows)]
pub fn restore_window_position(frame: &eframe::Frame, saved: &SavedWindow) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;

    // 전체 화면으로 끝낸 창은 eframe이 복원한 상태를 그대로 둔다.
    if saved.fullscreen {
        return;
    }
    let Some(saved_pos) = saved.outer_position_pixels else {
        return; // 처음 실행
    };
    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let hwnd = win32.hwnd.get() as HWND;
    let (x, y) = (saved_pos.x.round() as i32, saved_pos.y.round() as i32);
    unsafe {
        if saved.maximized {
            win::restore_maximized(hwnd, x, y);
        } else {
            win::restore(hwnd, x, y);
        }
    }
}

/// macOS는 OS가 화면 밖 창을 알아서 안으로 옮기고, Linux는 창 관리자에 맡긴다.
#[cfg(not(windows))]
pub fn restore_window_position(_frame: &eframe::Frame, _saved: &SavedWindow) {}



#[cfg(windows)]
mod win {
    use super::{centered_in, is_out_of_area, PxRect};
    use std::mem::{size_of, zeroed};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    use windows_sys::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, MonitorFromRect, MonitorFromWindow,
        HDC, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowPlacement, GetWindowRect, IsZoomed, SetWindowPlacement, SetWindowPos, ShowWindow,
        SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWMAXIMIZED, SW_SHOWNORMAL, WINDOWPLACEMENT,
    };

    /// 최대화로 끝낸 창: 저장된 위치의 모니터(없어졌으면 가장 가까운 모니터)에서 최대화한다.
    pub unsafe fn restore_maximized(hwnd: HWND, x: i32, y: i32) {
        // 최대화된 창의 바깥 위치는 모니터 왼쪽 위보다 테두리만큼 바깥이므로 안쪽 점으로 모니터를 찾는다.
        let target = MonitorFromPoint(POINT { x: x + 50, y: y + 50 }, MONITOR_DEFAULTTONEAREST);
        if target.is_null() || MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) == target {
            return;
        }
        let Some(target_info) = monitor_info(target) else {
            return;
        };
        let Some(primary_info) =
            monitor_info(MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY))
        else {
            return;
        };
        let mut placement: WINDOWPLACEMENT = zeroed();
        placement.length = size_of::<WINDOWPLACEMENT>() as u32;
        if GetWindowPlacement(hwnd, &mut placement) == 0 {
            return;
        }
        // 최대화를 풀었을 때의 크기는 그대로 두고 대상 모니터 가운데에 놓은 뒤 최대화한다.
        // rcNormalPosition은 "작업 영역 좌표"(주 모니터 작업 영역의 왼쪽 위가 0,0)를 쓴다.
        let offset_x = primary_info.rcWork.left - primary_info.rcMonitor.left;
        let offset_y = primary_info.rcWork.top - primary_info.rcMonitor.top;
        let normal = to_px(placement.rcNormalPosition);
        let centered = centered_in(to_px(target_info.rcWork), normal.width(), normal.height());
        placement.rcNormalPosition = RECT {
            left: centered.left - offset_x,
            top: centered.top - offset_y,
            right: centered.right - offset_x,
            bottom: centered.bottom - offset_y,
        };
        // 이미 최대화된 창은 위치를 바꿔도 Windows가 무시하므로,
        // 대상 모니터에 보통 크기로 놓은 다음 그 자리에서 다시 최대화한다.
        placement.flags = 0;
        placement.showCmd = SW_SHOWNORMAL as u32;
        SetWindowPlacement(hwnd, &placement);
        ShowWindow(hwnd, SW_SHOWMAXIMIZED);
    }

    /// 저장된 바깥 위치(x, y)로 옮기고, 화면 영역 밖이면 가장 가까운 모니터 가운데로 옮긴다.
    pub unsafe fn restore(hwnd: HWND, x: i32, y: i32) {
        if IsZoomed(hwnd) != 0 {
            return;
        }
        // 1) 저장된 물리 픽셀 위치로 그대로 옮긴다.
        //    배율이 다른 모니터로 가면 winit이 크기를 바꾸면서 위치가 조금 움직일 수 있어 한 번 더 맞춘다.
        for _ in 0..2 {
            let Some(window) = window_rect(hwnd) else {
                return;
            };
            if (window.left, window.top) == (x, y) {
                break;
            }
            move_to(hwnd, x, y);
        }

        // 2) 그 자리가 화면 영역 밖이면 가장 가까운 모니터의 작업 영역 가운데로
        let visible = visible_rect(hwnd);
        if !is_out_of_area(visible, &monitor_rects()) {
            return;
        }
        let Some(info) = monitor_info(MonitorFromRect(&to_rect(visible), MONITOR_DEFAULTTONEAREST))
        else {
            return;
        };
        let work = to_px(info.rcWork);
        // 먼저 그 모니터로 옮겨서 그 모니터 배율에 맞는 크기가 되게 한 뒤 가운데를 계산한다.
        move_to(hwnd, work.left, work.top);
        let (Some(window), visible) = (window_rect(hwnd), visible_rect(hwnd)) else {
            return;
        };
        let inset_left = visible.left - window.left;
        let inset_top = visible.top - window.top;
        let inset_right = window.right - visible.right;
        let inset_bottom = window.bottom - visible.bottom;
        let target = centered_in(work, visible.width(), visible.height());
        SetWindowPos(
            hwnd,
            null_mut(),
            target.left - inset_left,
            target.top - inset_top,
            target.width() + inset_left + inset_right,
            target.height() + inset_top + inset_bottom,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }

    unsafe fn move_to(hwnd: HWND, x: i32, y: i32) {
        SetWindowPos(
            hwnd,
            null_mut(),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }

    fn to_px(r: RECT) -> PxRect {
        PxRect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }

    fn to_rect(r: PxRect) -> RECT {
        RECT {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }

    unsafe fn window_rect(hwnd: HWND) -> Option<PxRect> {
        let mut rect: RECT = zeroed();
        (GetWindowRect(hwnd, &mut rect) != 0).then(|| to_px(rect))
    }

    /// 눈에 보이는 창틀 (보이지 않는 테두리 제외). 못 얻으면 창 전체로 본다.
    unsafe fn visible_rect(hwnd: HWND) -> PxRect {
        let mut rect: RECT = zeroed();
        let ok = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut rect as *mut RECT as *mut core::ffi::c_void,
            size_of::<RECT>() as u32,
        ) == 0;
        if ok {
            to_px(rect)
        } else {
            window_rect(hwnd).unwrap_or(PxRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            })
        }
    }

    unsafe fn monitor_info(monitor: HMONITOR) -> Option<MONITORINFO> {
        let mut info: MONITORINFO = zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        (GetMonitorInfoW(monitor, &mut info) != 0).then_some(info)
    }

    unsafe extern "system" fn collect_monitor(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let list = &mut *(data as *mut Vec<PxRect>);
        if let Some(info) = monitor_info(monitor) {
            list.push(to_px(info.rcMonitor));
        }
        1 // 계속
    }

    unsafe fn monitor_rects() -> Vec<PxRect> {
        let mut monitors: Vec<PxRect> = Vec::new();
        EnumDisplayMonitors(
            null_mut(),
            null(),
            Some(collect_monitor),
            &mut monitors as *mut Vec<PxRect> as LPARAM,
        );
        monitors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> PxRect {
        PxRect {
            left,
            top,
            right,
            bottom,
        }
    }

    const LEFT_MONITOR: PxRect = PxRect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };
    const RIGHT_MONITOR: PxRect = PxRect {
        left: 1920,
        top: 0,
        right: 3840,
        bottom: 1080,
    };

    #[test]
    fn window_on_removed_monitor_is_out_of_area() {
        let on_right = rect(2100, 100, 2900, 700);
        assert!(!is_out_of_area(on_right, &[LEFT_MONITOR, RIGHT_MONITOR]));
        // 오른쪽 모니터를 뺀 경우
        assert!(is_out_of_area(on_right, &[LEFT_MONITOR]));
    }

    #[test]
    fn window_across_two_monitors_is_fine_until_one_is_removed() {
        let across = rect(1500, 100, 2300, 700);
        assert!(!is_out_of_area(across, &[LEFT_MONITOR, RIGHT_MONITOR]));
        assert!(is_out_of_area(across, &[LEFT_MONITOR]));
    }

    #[test]
    fn edge_snapped_window_with_invisible_borders_is_fine() {
        // 왼쪽 반 화면에 붙인 창: 보이지 않는 테두리 때문에 7px씩 화면 밖으로 나간다
        let snapped = rect(-7, 0, 967, 1087);
        assert!(!is_out_of_area(snapped, &[LEFT_MONITOR]));
    }

    #[test]
    fn title_bar_above_screen_is_out_of_area() {
        assert!(is_out_of_area(rect(100, -200, 900, 400), &[LEFT_MONITOR]));
    }

    #[test]
    fn centers_in_work_area_and_shrinks_if_too_big() {
        let work = rect(0, 0, 1920, 1040); // 작업 표시줄 40px 제외
        assert_eq!(centered_in(work, 800, 600), rect(560, 220, 1360, 820));
        assert_eq!(centered_in(work, 2500, 1500), work);
        // 오른쪽 모니터의 작업 영역 기준
        let right_work = rect(1920, 0, 3840, 1040);
        assert_eq!(centered_in(right_work, 800, 600), rect(2480, 220, 3280, 820));
    }
}
