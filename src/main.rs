// 릴리스 빌드에서는 콘솔 창을 띄우지 않음 (Windows)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod fileops;
mod loader;
mod placement;
mod tree;

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
        ..Default::default()
    };

    eframe::run_native(
        "ImgView",
        options,
        Box::new(move |cc| Ok(Box::new(app::ImgViewApp::new(cc, start_path)))),
    )
}
