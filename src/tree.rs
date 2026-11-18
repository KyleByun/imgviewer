//! 왼쪽 폴더 트리. 하위 폴더 목록은 펼칠 때 읽는다(지연 로딩).

use std::path::{Path, PathBuf};

use eframe::egui;

use crate::fileops::{is_hidden, natural_cmp};

pub struct FolderTree {
    roots: Vec<TreeNode>,
    /// 이 폴더가 보이도록 상위 폴더들을 펼치고 스크롤한다.
    reveal: Option<PathBuf>,
    reveal_frames: u32,
}

struct TreeNode {
    path: PathBuf,
    label: String,
    has_children: bool,
    /// None이면 아직 읽지 않음
    children: Option<Vec<TreeNode>>,
}

impl TreeNode {
    fn new(path: PathBuf, label: String) -> Self {
        let has_children = has_subfolder(&path);
        Self {
            path,
            label,
            has_children,
            children: None,
        }
    }

    fn children_mut(&mut self) -> &mut Vec<TreeNode> {
        let path = &self.path;
        self.children.get_or_insert_with(|| {
            list_subfolders(path)
                .into_iter()
                .map(|(child, name)| TreeNode::new(child, name))
                .collect()
        })
    }

    fn find_mut(&mut self, target: &Path) -> Option<&mut TreeNode> {
        if self.path == target {
            return Some(self);
        }
        if !target.starts_with(&self.path) {
            return None;
        }
        self.children
            .as_mut()?
            .iter_mut()
            .find_map(|child| child.find_mut(target))
    }
}

impl FolderTree {
    pub fn new() -> Self {
        Self {
            roots: default_roots(),
            reveal: None,
            reveal_frames: 0,
        }
    }

    pub fn reveal(&mut self, path: &Path) {
        self.reveal = Some(path.to_path_buf());
        self.reveal_frames = 0;
    }

    pub fn cancel_reveal(&mut self) {
        self.reveal = None;
    }

    /// 폴더 내용이 바뀌었을 때(복사, 새로 고침) 해당 폴더의 하위 목록을 다시 읽게 한다.
    pub fn refresh(&mut self, path: &Path) {
        for root in &mut self.roots {
            if let Some(node) = root.find_mut(path) {
                node.children = None;
                node.has_children = has_subfolder(&node.path);
            }
        }
    }

    /// 트리를 그린다. 사용자가 폴더를 클릭하면 그 경로를 돌려준다.
    pub fn show(&mut self, ui: &mut egui::Ui, current: Option<&Path>) -> Option<PathBuf> {
        let reveal = self.reveal.clone();
        // 여러 시작 폴더(바탕 화면, C: 드라이브…)에 걸리면 가장 깊은 곳 하나에서만 펼친다.
        let reveal_root = reveal.as_deref().and_then(|target| {
            self.roots
                .iter()
                .enumerate()
                .filter(|(_, root)| target.starts_with(&root.path))
                .max_by_key(|(_, root)| root.path.components().count())
                .map(|(i, _)| i)
        });

        let mut clicked = None;
        let mut revealed = false;
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (i, root) in self.roots.iter_mut().enumerate() {
                    let target = if reveal_root == Some(i) {
                        reveal.as_deref()
                    } else {
                        None
                    };
                    show_node(ui, root, current, target, &mut clicked, &mut revealed);
                }
            });

        if self.reveal.is_some() {
            self.reveal_frames += 1;
            // 펼침 애니메이션 때문에 몇 프레임에 걸쳐 진행된다.
            if revealed || reveal_root.is_none() || self.reveal_frames > 120 {
                self.reveal = None;
            } else {
                ui.ctx().request_repaint();
            }
        }
        clicked
    }
}

fn show_node(
    ui: &mut egui::Ui,
    node: &mut TreeNode,
    current: Option<&Path>,
    reveal: Option<&Path>,
    clicked: &mut Option<PathBuf>,
    revealed: &mut bool,
) {
    let selected = current == Some(node.path.as_path());
    let is_target = reveal == Some(node.path.as_path());

    let response = if node.has_children {
        let id = ui.make_persistent_id(&node.path);
        let mut state =
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
        if reveal.is_some_and(|target| !is_target && target.starts_with(&node.path)) {
            state.set_open(true);
        }
        let label = node.label.clone();
        let (_, header, _) = state
            .show_header(ui, |ui| ui.selectable_label(selected, label))
            .body(|ui| {
                for child in node.children_mut().iter_mut() {
                    show_node(ui, child, current, reveal, clicked, revealed);
                }
            });
        header.inner
    } else {
        // 펼침 화살표 자리만큼 들여써서 이름 위치를 맞춘다.
        ui.horizontal(|ui| {
            ui.add_space(ui.spacing().indent);
            ui.selectable_label(selected, node.label.as_str())
        })
        .inner
    };

    if response.clicked() {
        *clicked = Some(node.path.clone());
    }
    if is_target {
        response.scroll_to_me(Some(egui::Align::Center));
        *revealed = true;
    }
}

fn default_roots() -> Vec<TreeNode> {
    let mut roots: Vec<TreeNode> = Vec::new();
    let favorites = [
        (dirs::desktop_dir(), "바탕 화면"),
        (dirs::picture_dir(), "사진"),
        (dirs::download_dir(), "다운로드"),
        (dirs::document_dir(), "문서"),
        (dirs::home_dir(), "홈"),
    ];
    for (path, label) in favorites {
        if let Some(path) = path {
            if path.is_dir() && !roots.iter().any(|r| r.path == path) {
                roots.push(TreeNode::new(path, label.to_owned()));
            }
        }
    }

    #[cfg(windows)]
    for letter in b'C'..=b'Z' {
        let drive = PathBuf::from(format!("{}:\\", letter as char));
        if drive.is_dir() {
            roots.push(TreeNode::new(drive, format!("{}: 드라이브", letter as char)));
        }
    }
    #[cfg(not(windows))]
    roots.push(TreeNode::new(PathBuf::from("/"), "/".to_owned()));

    roots
}

/// 숨김/시스템 폴더를 뺀 하위 폴더 목록 (이름순)
fn list_subfolders(path: &Path) -> Vec<(PathBuf, String)> {
    let Ok(read_dir) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut folders: Vec<(PathBuf, String)> = read_dir
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata().ok()?;
            // 분해형(NFD) 이름이 자모로 쪼개져 보이지 않도록 합쳐서 보여준다.
            (meta.is_dir() && !is_hidden(&meta, &name))
                .then(|| (entry.path(), crate::app::nfc(&name)))
        })
        .collect();
    folders.sort_by(|a, b| natural_cmp(&a.1, &b.1));
    folders
}

fn has_subfolder(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|read_dir| {
        read_dir.flatten().any(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            entry
                .metadata()
                .is_ok_and(|meta| meta.is_dir() && !is_hidden(&meta, &name))
        })
    })
}
