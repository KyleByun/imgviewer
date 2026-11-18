//! 파일 시스템 보조 기능: 숨김 판별, 이름 정렬, 끌어놓은 파일 복사.

use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use eframe::egui;

/// 복사 작업 결과
pub struct CopyReport {
    pub dest: PathBuf,
    pub copied: usize,
    /// 이미 대상 폴더에 있는 파일(같은 폴더에서 끌어온 경우)이라 건너뛴 수
    pub skipped: usize,
    pub errors: Vec<String>,
}

/// 백그라운드 스레드에서 `sources`를 `dest` 폴더 안으로 복사한다. 끝나면 결과를 보내고 화면을 다시 그리게 한다.
pub fn copy_into_background(
    ctx: egui::Context,
    sources: Vec<PathBuf>,
    dest: PathBuf,
) -> Receiver<CopyReport> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let report = copy_into(&sources, &dest);
        let _ = tx.send(report);
        ctx.request_repaint();
    });
    rx
}

/// 파일과 폴더(하위 내용 포함)를 `dest` 안으로 복사한다.
/// 같은 이름이 있으면 덮어쓰지 않고 "이름 (2).jpg"처럼 새 이름을 붙인다.
pub fn copy_into(sources: &[PathBuf], dest: &Path) -> CopyReport {
    let mut report = CopyReport {
        dest: dest.to_path_buf(),
        copied: 0,
        skipped: 0,
        errors: Vec::new(),
    };
    for src in sources {
        let Some(name) = src.file_name() else {
            report
                .errors
                .push(format!("{}: 복사할 수 없는 경로", src.display()));
            continue;
        };
        if src.parent().is_some_and(|parent| same_path(parent, dest)) {
            report.skipped += 1;
            continue;
        }
        if src.is_dir() && dest.starts_with(src) {
            report.errors.push(format!(
                "{}: 폴더를 자기 자신 안으로 복사할 수 없습니다",
                name.to_string_lossy()
            ));
            continue;
        }
        let target = unique_path(&dest.join(name));
        let result = if src.is_dir() {
            copy_dir_all(src, &target)
        } else {
            fs::copy(src, &target).map(|_| ())
        };
        match result {
            Ok(()) => report.copied += 1,
            Err(e) => report
                .errors
                .push(format!("{}: {e}", name.to_string_lossy())),
        }
    }
    report
}

/// 삭제 작업 결과
pub struct DeleteReport {
    pub deleted: usize,
    pub permanent: bool,
    pub errors: Vec<String>,
}

/// 백그라운드 스레드에서 삭제한다. `permanent`이 false면 휴지통으로 보낸다.
pub fn delete_background(
    ctx: egui::Context,
    paths: Vec<PathBuf>,
    permanent: bool,
) -> Receiver<DeleteReport> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let report = delete_paths(&paths, permanent);
        let _ = tx.send(report);
        ctx.request_repaint();
    });
    rx
}

pub fn delete_paths(paths: &[PathBuf], permanent: bool) -> DeleteReport {
    let mut report = DeleteReport {
        deleted: 0,
        permanent,
        errors: Vec::new(),
    };
    if permanent {
        for path in paths {
            let result = if path.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
            match result {
                Ok(()) => report.deleted += 1,
                Err(e) => report.errors.push(format!("{}: {e}", display_name(path))),
            }
        }
    } else {
        // 휴지통에 넣을 수 없는 항목(너무 큰 파일, 네트워크 드라이브 등)은
        // 영구 삭제하지 않고 그대로 둔 채 오류로 알려 준다.
        let result = std::panic::catch_unwind(|| trash::delete_all(paths));
        report.deleted = paths.iter().filter(|p| !p.exists()).count();
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => report.errors.push(e.to_string()),
            Err(_) => report
                .errors
                .push("휴지통 기능을 사용할 수 없습니다".to_owned()),
        }
    }
    report
}

fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// `path`가 이미 있으면 "이름 (2).확장자", "이름 (3).확장자" … 중 비어 있는 이름을 돌려준다.
pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    (2u32..)
        .map(|n| {
            let name = match &ext {
                Some(ext) => format!("{stem} ({n}).{ext}"),
                None => format!("{stem} ({n})"),
            };
            path.with_file_name(name)
        })
        .find(|candidate| !candidate.exists())
        .expect("사용할 수 있는 파일 이름을 찾지 못했습니다")
}

fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Windows는 경로의 대소문자를 구분하지 않는다.
fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

#[cfg(windows)]
pub fn is_hidden(meta: &std::fs::Metadata, _name: &str) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    meta.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
}

#[cfg(not(windows))]
pub fn is_hidden(_meta: &std::fs::Metadata, name: &str) -> bool {
    name.starts_with('.')
}

/// 사람이 기대하는 순서로 정렬: "2.jpg" < "10.jpg", 대소문자 무시.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut a = a.chars().peekable();
    let mut b = b.chars().peekable();
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let na = take_digits(&mut a);
                let nb = take_digits(&mut b);
                let ta = na.trim_start_matches('0');
                let tb = nb.trim_start_matches('0');
                let ord = ta
                    .len()
                    .cmp(&tb.len())
                    .then_with(|| ta.cmp(tb))
                    .then_with(|| na.len().cmp(&nb.len()));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(ca), Some(cb)) => {
                let ord = ca.to_lowercase().cmp(cb.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

fn take_digits(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut digits = String::new();
    while let Some(&c) = it.peek() {
        if !c.is_ascii_digit() {
            break;
        }
        digits.push(c);
        it.next();
    }
    digits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("imgview-fileops-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn natural_sort_orders_numbers_like_people_do() {
        let mut names = vec!["10.jpg", "2.jpg", "1.jpg", "b.png", "A.png", "img2.png", "img10.png"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            ["1.jpg", "2.jpg", "10.jpg", "A.png", "b.png", "img2.png", "img10.png"]
        );
    }

    #[test]
    fn copies_files_and_folders_without_overwriting() {
        let root = test_dir("copy");
        let src = root.join("src");
        let dest = root.join("dest");
        fs::create_dir_all(src.join("앨범").join("sub")).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(src.join("a.jpg"), b"new").unwrap();
        fs::write(src.join("앨범").join("sub").join("b.png"), b"bbb").unwrap();
        fs::write(dest.join("a.jpg"), b"old").unwrap();

        let report = copy_into(&[src.join("a.jpg"), src.join("앨범")], &dest);
        assert_eq!(report.copied, 2);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        // 기존 파일은 그대로, 새 파일은 다른 이름으로
        assert_eq!(fs::read(dest.join("a.jpg")).unwrap(), b"old");
        assert_eq!(fs::read(dest.join("a (2).jpg")).unwrap(), b"new");
        assert_eq!(
            fs::read(dest.join("앨범").join("sub").join("b.png")).unwrap(),
            b"bbb"
        );

        // 같은 파일을 한 번 더 놓으면 (3)
        copy_into(&[src.join("a.jpg")], &dest);
        assert!(dest.join("a (3).jpg").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dropping_into_the_same_folder_is_skipped() {
        let root = test_dir("same");
        fs::write(root.join("x.png"), b"x").unwrap();
        let report = copy_into(&[root.join("x.png")], &root);
        assert_eq!((report.copied, report.skipped), (0, 1));
        assert!(!root.join("x (2).png").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn permanent_delete_removes_files_and_folders() {
        let root = test_dir("delete");
        fs::write(root.join("a.jpg"), b"a").unwrap();
        fs::create_dir_all(root.join("앨범").join("sub")).unwrap();
        fs::write(root.join("앨범").join("sub").join("b.png"), b"b").unwrap();
        fs::write(root.join("keep.png"), b"k").unwrap();

        let report = delete_paths(
            &[root.join("a.jpg"), root.join("앨범"), root.join("없는파일.png")],
            true,
        );
        assert_eq!(report.deleted, 2);
        assert_eq!(report.errors.len(), 1, "없는 파일은 오류로 보고");
        assert!(!root.join("a.jpg").exists());
        assert!(!root.join("앨범").exists());
        assert!(root.join("keep.png").exists(), "선택하지 않은 파일은 그대로");
        let _ = fs::remove_dir_all(root);
    }

    /// 실제 휴지통을 쓰므로 기본으로는 돌지 않는다: `cargo test -- --ignored`
    /// 끝나면 테스트 파일을 휴지통에서도 지운다.
    #[test]
    #[ignore = "실제 휴지통을 사용함"]
    fn recycle_moves_file_to_trash() {
        let root = test_dir("recycle");
        let name = format!("imgview-trash-test-{}.png", std::process::id());
        let file = root.join(&name);
        fs::write(&file, b"x").unwrap();

        let report = delete_paths(&[file.clone()], false);
        assert_eq!(report.deleted, 1, "{:?}", report.errors);
        assert!(!file.exists());

        #[cfg(any(windows, target_os = "linux"))]
        {
            let items: Vec<_> = trash::os_limited::list()
                .unwrap()
                .into_iter()
                .filter(|item| item.name.to_string_lossy() == name)
                .collect();
            assert_eq!(items.len(), 1, "휴지통에 들어가 있어야 함");
            trash::os_limited::purge_all(items).unwrap();
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn folder_cannot_be_copied_into_itself() {
        let root = test_dir("self");
        let folder = root.join("folder");
        let inner = folder.join("inner");
        fs::create_dir_all(&inner).unwrap();
        let report = copy_into(&[folder.clone()], &inner);
        assert_eq!(report.copied, 0);
        assert_eq!(report.errors.len(), 1);
        let _ = fs::remove_dir_all(root);
    }
}
