// 릴리스 빌드에서는 콘솔 창을 띄우지 않음 (Windows)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod assoc;
mod fileops;
mod loader;
mod placement;
mod tree;
#[cfg(target_os = "linux")]
mod xdnd;

use eframe::egui;

/// 창 아이콘 (제목 표시줄, 작업 표시줄, macOS Dock). `examples/make_icons.rs`가 src/icon.png로 만든다.
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../assets/icons/imgview_256.png");

fn main() -> eframe::Result<()> {
    // 첫 번째 인자로 폴더나 이미지 파일 경로를 주면 바로 연다.
    let start_path = std::env::args_os().nth(1).map(std::path::PathBuf::from);

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("ImgView")
        // Linux(Wayland)에서 앱 메뉴의 ImgView.desktop과 창을 연결하는 이름.
        // eframe은 이 이름으로 설정 폴더도 정하므로 기존 설정 위치(ImgView)와 같게 둔다.
        .with_app_id("ImgView")
        .with_inner_size([1280.0, 820.0])
        .with_min_inner_size([520.0, 360.0])
        .with_drag_and_drop(true);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(WINDOW_ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        event_loop_builder: event_loop_builder(),
        ..Default::default()
    };

    // macOS: Finder에서 연 파일은 실행 인수가 아니라 "문서 열기" 이벤트로 온다. 앱이 뜨기 전에 받을 준비를 한다.
    assoc::install_open_handler();

    eframe::run_native(
        "ImgView",
        options,
        Box::new(move |cc| Ok(Box::new(app::ImgViewApp::new(cc, start_path)))),
    )
}

/// Linux: X 서버(Wayland 세션이면 XWayland)를 쓸 수 있으면 X11로 실행한다.
/// winit은 Wayland에서 파일 끌어놓기를 받지 못하고, 앱 밖으로 끌기(`xdnd`)도 X11에서만 된다.
#[cfg(target_os = "linux")]
fn event_loop_builder() -> Option<eframe::EventLoopBuilderHook> {
    std::env::var_os("DISPLAY")?;
    Some(Box::new(|builder| {
        use winit::platform::x11::EventLoopBuilderExtX11 as _;
        builder.with_x11();
    }))
}

#[cfg(not(target_os = "linux"))]
fn event_loop_builder() -> Option<eframe::EventLoopBuilderHook> {
    None
}
