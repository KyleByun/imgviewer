//! Windows: `assets/icons/imgview.ico`를 exe 리소스로 넣는다.
//! 바탕 화면, 탐색기, 바로 가기, 작업 표시줄에 이 아이콘이 보인다.
//!
//! 아이콘 파일들은 `src/icon.png`에서 만든다 (그림을 바꿨을 때만 다시 실행):
//!     cargo run --release --example make_icons
//! macOS(.icns)와 Linux(.png) 아이콘은 `packaging/` 아래 스크립트가 설치한다.

const WINDOWS_ICON: &str = "assets/icons/imgview.ico";

fn main() {
    println!("cargo:rerun-if-changed={WINDOWS_ICON}");
    println!("cargo:rerun-if-changed=build.rs");

    // 빌드하는 컴퓨터가 아니라 "만들어질 실행 파일"이 Windows용일 때만
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon(WINDOWS_ICON)
            .set("FileDescription", "ImgView")
            .set("ProductName", "ImgView");
        resource
            .compile()
            .expect("아이콘을 exe에 넣지 못했습니다 (Windows SDK의 rc.exe가 필요합니다)");
    }
}
