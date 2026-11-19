//! 이미지 파일 확장자를 ImgView와 연결한다 (설정 창의 "Associate image files" 버튼).
//!
//! 운영체제마다 방식이 다르다.
//! - Windows: 현재 사용자 레지스트리(HKCU\Software\Classes)에 ImgView를 등록한 뒤 "기본 앱" 설정 화면을 연다.
//!   Windows 10/11은 앱이 스스로 기본 앱이 되는 것을 막아 두었으므로 마지막 선택은 사용자가 한다.
//! - Linux: `~/.local/share/applications/ImgView.desktop`을 만들고 `~/.config/mimeapps.list`에 기본 앱으로 적는다.
//! - macOS: ImgView.app 번들을 Launch Services에 등록하고 확장자마다 기본 앱으로 지정한다.
//!   Finder에서 연 파일은 실행 인수가 아니라 "문서 열기" 이벤트로 오므로 그것도 받는다 ([`install_open_handler`]).

use std::path::PathBuf;

use eframe::egui;

/// 설정 창에서 버튼 위에 보여줄 설명
pub fn description() -> &'static str {
    platform::DESCRIPTION
}

/// 설정 창에 보여줄, 실제로 연결되는 형식 목록.
/// Linux는 확장자가 아니라 MIME 형식으로 연결하므로 그 목록을 보여준다 (hdr, qoi, pam은 빠짐).
pub fn covered_types() -> String {
    #[cfg(target_os = "linux")]
    return freedesktop::mime_types().join(", ");
    #[cfg(not(target_os = "linux"))]
    crate::app::IMAGE_EXTENSIONS.join(", ")
}

/// 이미지 확장자들을 ImgView와 연결한다. 성공하면 사용자에게 보여줄 안내 문구를 돌려준다.
pub fn associate() -> Result<String, String> {
    platform::associate()
}

/// macOS: Finder에서 연 파일을 받을 준비를 한다. eframe을 시작하기 전에 불러야 한다.
pub fn install_open_handler() {
    #[cfg(target_os = "macos")]
    platform::install_open_handler();
}

/// macOS: 파일 열기 이벤트가 오면 화면을 다시 그리게 할 egui 컨텍스트를 알려 준다.
pub fn set_repaint_context(ctx: &egui::Context) {
    #[cfg(target_os = "macos")]
    platform::set_repaint_context(ctx);
    #[cfg(not(target_os = "macos"))]
    let _ = ctx;
}

/// macOS: Finder에서 열어 달라고 온 파일 (여러 개면 마지막 것). 다른 운영체제는 실행 인수로 받으므로 항상 None.
pub fn take_opened_file() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return platform::take_opened_file();
    #[cfg(not(target_os = "macos"))]
    None
}

// ───────────────────────── Windows ─────────────────────────

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::io;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::System::Registry::{
        RegSetKeyValueW, HKEY, HKEY_CURRENT_USER, REG_NONE, REG_SZ,
    };
    use windows_sys::Win32::UI::Shell::{
        SHChangeNotify, ShellExecuteW, SHCNE_ASSOCCHANGED, SHCNF_IDLIST,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    use crate::app::IMAGE_EXTENSIONS;

    pub const DESCRIPTION: &str = "Registers ImgView for these file types (current user only) and opens \
        Windows Settings > Default apps. Windows lets only you pick the default app, so choose ImgView there.";

    /// HKCU\Software\RegisteredApplications에 적는 이름. 기본 앱 설정 화면을 바로 열 때도 쓴다.
    const APP_NAME: &str = "ImgView";

    pub fn associate() -> Result<String, String> {
        let exe = std::env::current_exe()
            .map_err(|e| format!("Cannot find the ImgView executable: {e}"))?;
        let exe_name = exe
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("imgview.exe")
            .to_owned();
        let exe = exe
            .to_str()
            .ok_or("The executable path is not valid Unicode")?
            .to_owned();
        register(HKEY_CURRENT_USER, "Software", &exe, &exe_name)
            .map_err(|e| format!("Could not write to the registry: {e}"))?;
        // 탐색기가 바뀐 연결 정보를 다시 읽게 한다.
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, null(), null()) };

        let next = if open_default_apps_page() {
            "In the Settings page that opened, click \"Set default\" (or choose ImgView for each type)."
        } else {
            "Open Windows Settings > Apps > Default apps > ImgView and click \"Set default\"."
        };
        Ok(format!(
            "Registered {} file types for\n{exe}\n{next}",
            IMAGE_EXTENSIONS.len()
        ))
    }

    /// `base` 아래에 ImgView를 등록한다. 실제로는 "Software", 테스트에서는 임시 키를 쓴다.
    ///
    /// - `Classes\ImgView.<ext>`: 확장자마다 하나씩 만든 ProgID (탐색기의 형식 이름이 "PNG Image"처럼 나오게)
    /// - `Classes\.<ext>\OpenWithProgids`: "연결 프로그램" 목록에 ImgView가 나오게
    /// - `Classes\Applications\imgview.exe`: 실행 파일 정보와 지원 형식
    /// - `ImgView\Capabilities` + `RegisteredApplications`: 설정 > 기본 앱 화면에 ImgView가 나오게
    ///
    /// 확장자 키의 기본값이나 UserChoice는 건드리지 않는다 (다른 앱의 설정을 덮어쓰지 않도록).
    pub(super) fn register(root: HKEY, base: &str, exe: &str, exe_name: &str) -> io::Result<()> {
        let classes = format!(r"{base}\Classes");
        let command = format!("\"{exe}\" \"%1\"");
        let icon = format!("\"{exe}\",0");
        let app_key = format!(r"{classes}\Applications\{exe_name}");
        let capabilities = format!(r"{base}\{APP_NAME}\Capabilities");

        set_str(root, &app_key, Some("FriendlyAppName"), APP_NAME)?;
        set_str(root, &format!(r"{app_key}\DefaultIcon"), None, &icon)?;
        set_str(root, &format!(r"{app_key}\shell\open\command"), None, &command)?;
        set_str(root, &capabilities, Some("ApplicationName"), APP_NAME)?;
        set_str(root, &capabilities, Some("ApplicationDescription"), "Simple image viewer")?;
        set_str(root, &capabilities, Some("ApplicationIcon"), &icon)?;

        for ext in IMAGE_EXTENSIONS {
            let prog_id = format!("ImgView.{ext}");
            let prog_key = format!(r"{classes}\{prog_id}");
            let dot_ext = format!(".{ext}");
            set_str(root, &prog_key, None, &format!("{} Image", ext.to_ascii_uppercase()))?;
            set_str(root, &format!(r"{prog_key}\DefaultIcon"), None, &icon)?;
            set_str(root, &format!(r"{prog_key}\shell\open\command"), None, &command)?;
            set_none(root, &format!(r"{classes}\{dot_ext}\OpenWithProgids"), &prog_id)?;
            set_str(root, &format!(r"{app_key}\SupportedTypes"), Some(&dot_ext), "")?;
            set_str(root, &format!(r"{capabilities}\FileAssociations"), Some(&dot_ext), &prog_id)?;
        }
        // Capabilities를 다 적은 뒤에 등록해야 Windows가 덜 만들어진 정보를 읽지 않는다.
        set_str(
            root,
            &format!(r"{base}\RegisteredApplications"),
            Some(APP_NAME),
            &capabilities,
        )
    }

    /// 설정 > 기본 앱 > ImgView 화면을 연다 (Windows 11 2023-04 업데이트 이후 지원, 그 전에는 기본 앱 첫 화면).
    fn open_default_apps_page() -> bool {
        let verb = wide("open");
        let uri = wide(&format!("ms-settings:defaultapps?registeredAppUser={APP_NAME}"));
        let result = unsafe {
            ShellExecuteW(null_mut(), verb.as_ptr(), uri.as_ptr(), null(), null(), SW_SHOWNORMAL)
        };
        // 32보다 크면 성공
        result as usize > 32
    }

    fn set_str(root: HKEY, key: &str, name: Option<&str>, value: &str) -> io::Result<()> {
        let data = wide(value);
        let size = (data.len() * std::mem::size_of::<u16>()) as u32;
        set_value(root, key, name, REG_SZ, data.as_ptr().cast(), size)
    }

    fn set_none(root: HKEY, key: &str, name: &str) -> io::Result<()> {
        set_value(root, key, Some(name), REG_NONE, null(), 0)
    }

    /// 값을 쓴다. 중간 키가 없으면 RegSetKeyValueW가 만든다.
    fn set_value(
        root: HKEY,
        key: &str,
        name: Option<&str>,
        kind: u32,
        data: *const c_void,
        size: u32,
    ) -> io::Result<()> {
        let key = wide(key);
        let name = name.map(wide);
        let name_ptr = name.as_ref().map_or(null(), |n| n.as_ptr());
        let status = unsafe { RegSetKeyValueW(root, key.as_ptr(), name_ptr, kind, data, size) };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }

    pub(super) fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

// ───────────────────────── Linux ─────────────────────────

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};

    use super::freedesktop::{self, DESKTOP_ID};

    pub const DESCRIPTION: &str = "Makes ImgView the default app for these file types \
        (~/.config/mimeapps.list) and adds it to the app menu (~/.local/share/applications).";

    /// install.sh가 아이콘을 설치하지 않았을 때 쓸 앱 아이콘
    const ICON_PNG: &[u8] = include_bytes!("../assets/icons/imgview_256.png");

    pub fn associate() -> Result<String, String> {
        let exe = std::env::current_exe()
            .map_err(|e| format!("Cannot find the ImgView executable: {e}"))?;
        let exe = exe.to_str().ok_or("The executable path is not valid UTF-8")?;
        let home = dirs::home_dir().ok_or("Cannot find the home folder")?;
        let data_home = xdg_dir("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
        let config_home = xdg_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));

        // 1) 앱 메뉴 항목: Exec을 지금 실행 중인 파일로 맞춘다.
        let apps_dir = data_home.join("applications");
        write_atomic(&apps_dir.join(DESKTOP_ID), freedesktop::desktop_entry(exe)?.as_bytes())?;
        let icon = data_home.join("icons/hicolor/256x256/apps/imgview.png");
        if !icon.exists() {
            write_atomic(&icon, ICON_PNG)?;
        }

        // 2) 기본 앱 지정. 데스크톱별 목록(gnome-mimeapps.list 등)이 있으면 그쪽이 우선이라 함께 고친다.
        let mimes = freedesktop::mime_types();
        let mut lists = vec![config_home.join("mimeapps.list")];
        if let Some(desktops) = std::env::var_os("XDG_CURRENT_DESKTOP") {
            for name in desktops.to_string_lossy().split(':').filter(|n| !n.is_empty()) {
                let path = config_home.join(format!("{}-mimeapps.list", name.to_lowercase()));
                if path.is_file() && !lists.contains(&path) {
                    lists.push(path);
                }
            }
        }
        for list in &lists {
            let old = match fs::read_to_string(list) {
                Ok(text) => text,
                Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
                Err(e) => return Err(format!("Cannot read {}: {e}", list.display())),
            };
            let new = freedesktop::set_default_apps(&old, DESKTOP_ID, &mimes);
            write_atomic(list, new.as_bytes())?;
        }

        // 3) 앱 메뉴 캐시 새로 고침 (도구가 없으면 건너뜀)
        let _ = std::process::Command::new("update-desktop-database")
            .arg("-q")
            .arg(&apps_dir)
            .status();

        Ok(format!(
            "ImgView is now the default app for {} image types.\nUpdated {}",
            mimes.len(),
            lists[0].display()
        ))
    }

    fn xdg_dir(name: &str) -> Option<PathBuf> {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    }

    /// 임시 파일에 쓴 뒤 이름을 바꿔서, 도중에 실패해도 원래 파일이 깨지지 않게 한다.
    /// 심볼릭 링크(dotfiles 관리 등)면 링크가 가리키는 파일을 고친다.
    fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
        let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let fail = |e: std::io::Error| format!("Cannot write {}: {e}", target.display());
        let dir = target.parent().ok_or_else(|| format!("Invalid path: {}", target.display()))?;
        fs::create_dir_all(dir).map_err(fail)?;
        let name = target.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
        let tmp = dir.join(format!(".{name}.imgview-tmp"));
        let result = fs::write(&tmp, bytes).and_then(|()| fs::rename(&tmp, &target));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.map_err(fail)
    }
}

/// Linux 데스크톱 규격(Desktop Entry, mimeapps.list) 처리. 시스템을 건드리지 않는 부분이라 모든 OS에서 테스트한다.
#[cfg(any(test, target_os = "linux"))]
mod freedesktop {
    // 다른 OS에서는 테스트에서만 쓴다.
    #![cfg_attr(not(target_os = "linux"), allow(dead_code))]

    use std::collections::HashSet;

    /// packaging/linux/install.sh가 만드는 것과 같은 이름이어야 앱 메뉴에 두 개가 생기지 않는다.
    pub const DESKTOP_ID: &str = "ImgView.desktop";
    /// 앱 메뉴 항목 원본. 연결할 MIME 형식 목록도 여기의 MimeType 줄을 쓴다.
    const DESKTOP_TEMPLATE: &str = include_str!("../packaging/linux/ImgView.desktop");

    pub fn mime_types() -> Vec<&'static str> {
        DESKTOP_TEMPLATE
            .lines()
            .find_map(|line| line.strip_prefix("MimeType="))
            .map(split_list)
            .unwrap_or_default()
    }

    /// Exec 줄만 지금 실행 중인 파일로 바꾼 데스크톱 항목
    pub fn desktop_entry(exe: &str) -> Result<String, String> {
        let exec = exec_arg(exe)?;
        let mut text = String::new();
        for line in DESKTOP_TEMPLATE.lines() {
            if line.starts_with("Exec=") {
                text.push_str(&format!("Exec={exec} %f"));
            } else {
                text.push_str(line);
            }
            text.push('\n');
        }
        Ok(text)
    }

    /// Desktop Entry 규격에 맞게 Exec의 실행 파일 경로를 적는다.
    /// 특수 문자가 있으면 큰따옴표로 감싸고 ", `, $, \ 앞에 \를 붙인다.
    /// 그 다음 %는 %%로 (필드 코드와 구별), \는 \\로 (문자열 값 규칙) 적는다.
    pub fn exec_arg(path: &str) -> Result<String, String> {
        if path.contains(['\n', '\r']) {
            return Err("The executable path contains a line break".to_owned());
        }
        const RESERVED: &[char] = &[
            ' ', '\t', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(', ')',
            '`',
        ];
        let quoted = if path.contains(RESERVED) {
            let mut s = String::from('"');
            for c in path.chars() {
                if matches!(c, '"' | '`' | '$' | '\\') {
                    s.push('\\');
                }
                s.push(c);
            }
            s.push('"');
            s
        } else {
            path.to_owned()
        };
        Ok(quoted.replace('%', "%%").replace('\\', "\\\\"))
    }

    /// mimeapps.list 내용에서 `mimes`의 기본 앱을 `desktop_id`로 바꾼다. 다른 줄은 그대로 둔다.
    /// - [Default Applications]: 기본 앱
    /// - [Added Associations]: "연결 프로그램" 목록 맨 앞에 추가
    /// - [Removed Associations]: 예전에 빼 둔 기록이 있으면 지운다
    pub fn set_default_apps(content: &str, desktop_id: &str, mimes: &[&str]) -> String {
        let mut doc = IniDoc::parse(content);
        doc.update("Default Applications", mimes, |_| Some(desktop_id.to_owned()));
        doc.update("Added Associations", mimes, |old| {
            let mut ids = vec![desktop_id];
            ids.extend(split_list(old.unwrap_or("")).into_iter().filter(|id| *id != desktop_id));
            Some(join_list(&ids))
        });
        doc.update("Removed Associations", mimes, |old| {
            let rest: Vec<&str> = split_list(old?)
                .into_iter()
                .filter(|id| *id != desktop_id)
                .collect();
            (!rest.is_empty()).then(|| join_list(&rest))
        });
        doc.to_text()
    }

    fn split_list(list: &str) -> Vec<&str> {
        list.split(';').map(str::trim).filter(|s| !s.is_empty()).collect()
    }

    fn join_list(ids: &[&str]) -> String {
        let mut text = ids.join(";");
        text.push(';');
        text
    }

    struct Section {
        /// 첫 구역(헤더 앞의 줄들)은 None
        name: Option<String>,
        header: Option<String>,
        body: Vec<String>,
    }

    /// 줄 단위로 고치는 아주 단순한 INI 문서 (주석, 빈 줄, 모르는 키는 그대로 보존)
    struct IniDoc {
        sections: Vec<Section>,
    }

    impl IniDoc {
        fn parse(content: &str) -> Self {
            let mut sections = vec![Section { name: None, header: None, body: Vec::new() }];
            for line in content.lines() {
                let t = line.trim();
                if t.len() >= 2 && t.starts_with('[') && t.ends_with(']') {
                    sections.push(Section {
                        name: Some(t[1..t.len() - 1].trim().to_owned()),
                        header: Some(line.to_owned()),
                        body: Vec::new(),
                    });
                } else if let Some(last) = sections.last_mut() {
                    last.body.push(line.to_owned());
                }
            }
            Self { sections }
        }

        /// `name` 구역의 `keys` 값을 `f(예전 값)`으로 바꾼다. None을 돌려주면 그 줄을 지운다.
        fn update(&mut self, name: &str, keys: &[&str], f: impl Fn(Option<&str>) -> Option<String>) {
            let index = match self.sections.iter().position(|s| s.name.as_deref() == Some(name)) {
                Some(i) => i,
                None => {
                    if f(None).is_none() {
                        return; // 새로 적을 것이 없으면 구역도 만들지 않는다
                    }
                    if let Some(last) = self.sections.last_mut() {
                        let ends_blank = last.body.last().is_none_or(|l| l.trim().is_empty());
                        if last.header.is_some() && !ends_blank {
                            last.body.push(String::new());
                        }
                    }
                    self.sections.push(Section {
                        name: Some(name.to_owned()),
                        header: Some(format!("[{name}]")),
                        body: Vec::new(),
                    });
                    self.sections.len() - 1
                }
            };

            let section = &mut self.sections[index];
            let mut seen = HashSet::new();
            let mut body = Vec::with_capacity(section.body.len() + keys.len());
            for line in std::mem::take(&mut section.body) {
                let entry = key_value(&line).map(|(k, v)| (k.to_owned(), v.to_owned()));
                match entry {
                    Some((key, value)) if keys.contains(&key.as_str()) => {
                        // 같은 키가 여러 번 있으면 첫 줄만 남긴다.
                        if seen.insert(key.clone()) {
                            if let Some(new) = f(Some(&value)) {
                                body.push(format!("{key}={new}"));
                            }
                        }
                    }
                    _ => body.push(line),
                }
            }
            // 없던 키는 구역 끝(뒤쪽 빈 줄 앞)에 붙인다.
            let trailing_blank = body.iter().rev().take_while(|l| l.trim().is_empty()).count();
            let at = body.len() - trailing_blank;
            let added: Vec<String> = keys
                .iter()
                .filter(|key| !seen.contains(**key))
                .filter_map(|key| f(None).map(|value| format!("{key}={value}")))
                .collect();
            body.splice(at..at, added);
            section.body = body;
        }

        fn to_text(&self) -> String {
            let mut text = String::new();
            for section in &self.sections {
                for line in section.header.iter().chain(&section.body) {
                    text.push_str(line);
                    text.push('\n');
                }
            }
            text
        }
    }

    fn key_value(line: &str) -> Option<(&str, &str)> {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            return None;
        }
        let (key, value) = t.split_once('=')?;
        Some((key.trim(), value.trim()))
    }
}

// ───────────────────────── macOS ─────────────────────────

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::{c_char, c_void, CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::ptr::null;
    use std::sync::{Mutex, OnceLock};

    use eframe::egui;
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    use crate::app::IMAGE_EXTENSIONS;

    pub const DESCRIPTION: &str = "Makes ImgView.app the default app for these file types in Finder. \
        Run the app bundle made by packaging/macos/build_app.sh (ideally from Applications).";

    type CFTypeRef = *const c_void;
    /// kCFStringEncodingUTF8
    const UTF8: u32 = 0x0800_0100;
    /// kLSRolesAll
    const ROLES_ALL: u32 = 0xFFFF_FFFF;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFBundleGetMainBundle() -> CFTypeRef;
        fn CFBundleGetIdentifier(bundle: CFTypeRef) -> CFTypeRef;
        fn CFBundleCopyBundleURL(bundle: CFTypeRef) -> CFTypeRef;
        fn CFStringCreateWithBytes(
            alloc: CFTypeRef,
            bytes: *const u8,
            len: isize,
            encoding: u32,
            is_external: u8,
        ) -> CFTypeRef;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        fn LSRegisterURL(url: CFTypeRef, update: u8) -> i32;
        fn LSSetDefaultRoleHandlerForContentType(
            content_type: CFTypeRef,
            role: u32,
            bundle_id: CFTypeRef,
        ) -> i32;
        fn UTTypeCreatePreferredIdentifierForTag(
            tag_class: CFTypeRef,
            tag: CFTypeRef,
            conforming_to: CFTypeRef,
        ) -> CFTypeRef;
    }

    /// 다 쓰면 CFRelease하는 Core Foundation 객체
    struct Owned(CFTypeRef);

    impl Owned {
        fn string(text: &str) -> Option<Self> {
            let s = unsafe {
                CFStringCreateWithBytes(null(), text.as_ptr(), text.len() as isize, UTF8, 0)
            };
            (!s.is_null()).then_some(Self(s))
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) };
            }
        }
    }

    pub fn associate() -> Result<String, String> {
        let exe = std::env::current_exe()
            .map_err(|e| format!("Cannot find the ImgView executable: {e}"))?;
        if !exe.to_string_lossy().contains(".app/Contents/MacOS/") {
            return Err("Run ImgView.app to associate files: build it with \
                `sh packaging/macos/build_app.sh` and move it to Applications. \
                A bare executable cannot be a Finder default app."
                .to_owned());
        }
        let mut failed = Vec::new();
        unsafe {
            let bundle = CFBundleGetMainBundle();
            let bundle_id = if bundle.is_null() { null() } else { CFBundleGetIdentifier(bundle) };
            if bundle_id.is_null() {
                return Err("ImgView.app has no bundle identifier (Info.plist).".to_owned());
            }
            // Launch Services가 Info.plist의 문서 형식(CFBundleDocumentTypes)을 읽게 한다.
            let url = Owned(CFBundleCopyBundleURL(bundle));
            if !url.0.is_null() {
                LSRegisterURL(url.0, 1);
            }
            let tag_class = Owned::string("public.filename-extension")
                .ok_or("Cannot create a CFString")?;
            for ext in IMAGE_EXTENSIONS {
                // 확장자 → UTI (jpg → public.jpeg 등, 시스템이 모르는 확장자는 dyn.* 형식)
                let uti = Owned::string(ext).map(|tag| {
                    Owned(UTTypeCreatePreferredIdentifierForTag(tag_class.0, tag.0, null()))
                });
                let status = match &uti {
                    Some(uti) if !uti.0.is_null() => {
                        LSSetDefaultRoleHandlerForContentType(uti.0, ROLES_ALL, bundle_id)
                    }
                    _ => -1,
                };
                if status != 0 {
                    failed.push(format!("{ext} ({status})"));
                }
            }
        }
        if failed.len() == IMAGE_EXTENSIONS.len() {
            return Err(format!("macOS refused every file type: {}", failed.join(", ")));
        }
        let mut message = "ImgView is now the default app for image files in Finder.".to_owned();
        if !failed.is_empty() {
            message.push_str(&format!("\nNot changed: {}", failed.join(", ")));
        }
        Ok(message)
    }

    /// Finder가 보낸 "문서 열기"로 받은 파일들
    static OPENED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    static REPAINT: OnceLock<egui::Context> = OnceLock::new();

    /// Finder에서 연 파일은 앱 delegate의 `application:openURLs:`로 온다.
    /// winit의 delegate 클래스는 이벤트 루프를 만들 때 생기고, 실행하면서 열 파일은
    /// applicationDidFinishLaunching(= eframe이 앱을 만드는 때)보다 먼저 온다.
    /// eframe에는 그 사이에 끼어들 방법이 없으므로, 시작 전에 NSObject에 이 메서드를 붙여서
    /// winit의 delegate(NSObject 하위 클래스)도 응답하게 만든다.
    pub fn install_open_handler() {
        type OpenUrls =
            unsafe extern "C" fn(*mut AnyObject, *const c_void, *mut AnyObject, *mut AnyObject);
        unsafe {
            let class = objc2::ffi::objc_getClass(c"NSObject".as_ptr());
            if class.is_null() {
                return;
            }
            let selector = objc2::ffi::sel_registerName(c"application:openURLs:".as_ptr());
            let imp: OpenUrls = open_urls;
            // 반환값 없음(v), self(@), _cmd(:), NSApplication(@), NSArray<NSURL>(@)
            objc2::ffi::class_addMethod(
                class as *mut _,
                selector,
                Some(std::mem::transmute::<OpenUrls, unsafe extern "C" fn()>(imp)),
                c"v@:@@".as_ptr(),
            );
        }
    }

    unsafe extern "C" fn open_urls(
        _this: *mut AnyObject,
        _cmd: *const c_void,
        _app: *mut AnyObject,
        urls: *mut AnyObject,
    ) {
        // Objective-C 쪽으로 패닉이 넘어가지 않게 막는다.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let paths = unsafe { file_paths(urls) };
            if let Ok(mut queue) = OPENED.lock() {
                queue.extend(paths);
            }
            if let Some(ctx) = REPAINT.get() {
                ctx.request_repaint();
            }
        }));
    }

    /// NSArray<NSURL *>에서 파일 경로만 꺼낸다.
    unsafe fn file_paths(urls: *mut AnyObject) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if urls.is_null() {
            return paths;
        }
        let count: usize = msg_send![urls, count];
        for i in 0..count {
            let url: *mut AnyObject = msg_send![urls, objectAtIndex: i];
            if url.is_null() {
                continue;
            }
            let is_file: bool = msg_send![url, isFileURL];
            if !is_file {
                continue;
            }
            let path: *mut AnyObject = msg_send![url, path];
            if path.is_null() {
                continue;
            }
            let utf8: *const c_char = msg_send![path, UTF8String];
            if !utf8.is_null() {
                let bytes = CStr::from_ptr(utf8).to_bytes();
                paths.push(PathBuf::from(OsStr::from_bytes(bytes)));
            }
        }
        paths
    }

    pub fn set_repaint_context(ctx: &egui::Context) {
        let _ = REPAINT.set(ctx.clone());
    }

    pub fn take_opened_file() -> Option<PathBuf> {
        let mut queue = OPENED.lock().ok()?;
        let last = queue.pop();
        queue.clear();
        last
    }
}

// ───────────────────────── 그 밖의 운영체제 ─────────────────────────

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod platform {
    pub const DESCRIPTION: &str = "File associations are not supported on this operating system.";

    pub fn associate() -> Result<String, String> {
        Err(DESCRIPTION.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::freedesktop::{desktop_entry, exec_arg, mime_types, set_default_apps};

    #[test]
    fn mime_types_come_from_the_desktop_file() {
        let mimes = mime_types();
        assert!(mimes.contains(&"image/png"));
        assert!(mimes.contains(&"image/jpeg"));
        assert!(mimes.contains(&"image/vnd.microsoft.icon"));
        assert!(mimes.iter().all(|m| m.starts_with("image/") && !m.contains(';')));
    }

    #[test]
    fn exec_paths_are_quoted_like_the_desktop_entry_spec() {
        assert_eq!(exec_arg("/home/me/.local/bin/imgview").unwrap(), "/home/me/.local/bin/imgview");
        assert_eq!(exec_arg("/home/me/my apps/imgview").unwrap(), "\"/home/me/my apps/imgview\"");
        // " → \" (따옴표 규칙) → \\" (문자열 값 규칙)
        assert_eq!(exec_arg("/opt/a\"b/imgview").unwrap(), "\"/opt/a\\\\\"b/imgview\"");
        assert_eq!(exec_arg("/opt/$HOME/imgview").unwrap(), "\"/opt/\\\\$HOME/imgview\"");
        assert_eq!(exec_arg("/opt/100%/imgview").unwrap(), "/opt/100%%/imgview");
        assert!(exec_arg("/opt/a\nb").is_err());
    }

    #[test]
    fn desktop_entry_points_at_the_running_executable() {
        let entry = desktop_entry("/home/me/my apps/imgview").unwrap();
        assert!(entry.contains("\nExec=\"/home/me/my apps/imgview\" %f\n"));
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("\nMimeType=image/png;"));
        assert_eq!(entry.matches("Exec=").count(), 1);
    }

    #[test]
    fn mimeapps_list_is_created_when_missing() {
        let text = set_default_apps("", "ImgView.desktop", &["image/png", "image/gif"]);
        assert_eq!(
            text,
            "[Default Applications]\nimage/png=ImgView.desktop\nimage/gif=ImgView.desktop\n\n\
             [Added Associations]\nimage/png=ImgView.desktop;\nimage/gif=ImgView.desktop;\n"
        );
    }

    #[test]
    fn mimeapps_list_keeps_other_lines_and_updates_ours() {
        let old = "# my settings\n\
                   [Default Applications]\n\
                   image/png=org.gnome.Loupe.desktop\n\
                   text/plain=org.gnome.TextEditor.desktop\n\
                   \n\
                   [Added Associations]\n\
                   image/png=org.gnome.Loupe.desktop;ImgView.desktop;\n\
                   \n\
                   [Removed Associations]\n\
                   image/gif=ImgView.desktop;gimp.desktop;\n\
                   image/jpeg=ImgView.desktop;\n";
        let new = set_default_apps(old, "ImgView.desktop", &["image/png", "image/gif", "image/jpeg"]);
        assert_eq!(
            new,
            "# my settings\n\
             [Default Applications]\n\
             image/png=ImgView.desktop\n\
             text/plain=org.gnome.TextEditor.desktop\n\
             image/gif=ImgView.desktop\n\
             image/jpeg=ImgView.desktop\n\
             \n\
             [Added Associations]\n\
             image/png=ImgView.desktop;org.gnome.Loupe.desktop;\n\
             image/gif=ImgView.desktop;\n\
             image/jpeg=ImgView.desktop;\n\
             \n\
             [Removed Associations]\n\
             image/gif=gimp.desktop;\n"
        );
        // 두 번 해도 결과가 같다.
        assert_eq!(set_default_apps(&new, "ImgView.desktop", &["image/png", "image/gif", "image/jpeg"]), new);
    }

    #[cfg(windows)]
    #[test]
    fn windows_registration_is_written_under_the_given_key() {
        use windows_sys::Win32::System::Registry::{
            RegDeleteTreeW, RegGetValueW, HKEY_CURRENT_USER, REG_NONE, RRF_RT_ANY,
        };

        use super::platform::{register, wide};

        // 실제 연결(HKCU\Software\Classes)은 건드리지 않도록 임시 키에 쓰고, 끝나면 지운다.
        struct TempKey(String);
        impl Drop for TempKey {
            fn drop(&mut self) {
                unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&self.0).as_ptr()) };
            }
        }
        let base = TempKey(format!(r"Software\ImgViewTest-{}", std::process::id()));
        let base = base.0.as_str();
        register(HKEY_CURRENT_USER, base, r"C:\Apps\ImgView\imgview.exe", "imgview.exe").unwrap();

        // (형식, 문자열 값)
        let read = |key: &str, name: Option<&str>| -> Option<(u32, String)> {
            let key = wide(key);
            let name = name.map(wide);
            let name_ptr = name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr());
            let mut kind = 0u32;
            let mut buf = [0u16; 512];
            let mut size = std::mem::size_of_val(&buf) as u32;
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    name_ptr,
                    RRF_RT_ANY,
                    &mut kind,
                    buf.as_mut_ptr().cast(),
                    &mut size,
                )
            };
            (status == 0).then(|| {
                let len = (size as usize / 2).saturating_sub(1);
                (kind, String::from_utf16_lossy(&buf[..len]))
            })
        };
        let text = |key: &str, name: Option<&str>| read(key, name).map(|(_, s)| s);

        assert_eq!(
            text(&format!(r"{base}\Classes\ImgView.png\shell\open\command"), None).as_deref(),
            Some(r#""C:\Apps\ImgView\imgview.exe" "%1""#)
        );
        assert_eq!(
            text(&format!(r"{base}\Classes\ImgView.jpg"), None).as_deref(),
            Some("JPG Image")
        );
        assert_eq!(
            read(&format!(r"{base}\Classes\.webp\OpenWithProgids"), Some("ImgView.webp")).map(|(k, _)| k),
            Some(REG_NONE)
        );
        assert_eq!(
            text(&format!(r"{base}\ImgView\Capabilities\FileAssociations"), Some(".tiff")).as_deref(),
            Some("ImgView.tiff")
        );
        assert_eq!(
            text(&format!(r"{base}\Classes\Applications\imgview.exe"), Some("FriendlyAppName")).as_deref(),
            Some("ImgView")
        );
        assert_eq!(
            text(&format!(r"{base}\RegisteredApplications"), Some("ImgView")),
            Some(format!(r"{base}\ImgView\Capabilities"))
        );
        // 확장자 키의 기본값(다른 앱이 정해 둔 연결)은 건드리지 않는다.
        assert_eq!(text(&format!(r"{base}\Classes\.png"), None), None);
    }
}
