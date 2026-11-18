//! 화면 구성: 왼쪽 폴더 트리 + 썸네일 목록(브라우저 모드) / 큰 이미지 보기(뷰어 모드).

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, SystemTime};

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CursorIcon, FontId, Key, Layout, Rect, Sense, Vec2,
};

use crate::fileops::{self, is_hidden, natural_cmp, CopyReport, DeleteReport};
use crate::loader::{self, FullImage, LoadResult, Loader};
use crate::placement::{self, SavedWindow};
use crate::tree::FolderTree;

/// 목록에 보여줄 이미지 확장자 (대소문자 무시)
const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "jpe", "jfif", "gif", "bmp", "dib", "ico", "tif", "tiff", "webp", "tga",
    "pbm", "pgm", "ppm", "pnm", "pam", "qoi", "hdr",
];

const THUMB_MIN: f32 = 80.0;
const THUMB_MAX: f32 = 256.0;
const TILE_PAD: f32 = 6.0;
const LABEL_HEIGHT: f32 = 22.0;
const TILE_SPACING: f32 = 8.0;
const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 32.0;
/// 원본 텍스처의 최대 크기 (GPU 메모리 보호)
const MAX_FULL_TEXTURE_SIDE: u32 = 8192;
/// 휠 한 칸(약 40포인트)에 약 1.2배 확대/축소
const WHEEL_ZOOM_SPEED: f32 = 1.0 / 200.0;
/// 타일을 이만큼 끌어야 탐색기로 끌기가 시작된다
const DRAG_OUT_DISTANCE: f32 = 6.0;
/// 안내 메시지를 보여주는 시간(초)
const INFO_SECONDS: f64 = 6.0;

/// 글꼴 크기 범위와 기본값 (egui 기본 Body 크기와 같다)
const FONT_MIN: f32 = 9.0;
const FONT_MAX: f32 = 24.0;
const FONT_DEFAULT: f32 = 12.5;

const BG_GRID: Color32 = Color32::from_rgb(30, 30, 33);
const BG_TILE: Color32 = Color32::from_rgb(45, 45, 49);
const BG_TILE_HOVER: Color32 = Color32::from_rgb(62, 62, 68);
const BG_TILE_SELECTED: Color32 = Color32::from_rgb(46, 84, 134);
const BG_PLACEHOLDER: Color32 = Color32::from_rgb(55, 55, 60);
const BG_VIEWER: Color32 = Color32::from_rgb(14, 14, 15);
const FOLDER_COLOR: Color32 = Color32::from_rgb(230, 184, 74);
const ERROR_COLOR: Color32 = Color32::from_rgb(235, 120, 100);
const INFO_COLOR: Color32 = Color32::from_rgb(130, 200, 140);

enum ThumbState {
    NotRequested,
    Loading,
    Ready(egui::TextureHandle),
    Failed,
}

struct ImageEntry {
    path: PathBuf,
    name: String,
    file_size: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    thumb: ThumbState,
    /// 원본 크기 (썸네일을 읽은 뒤에 알 수 있음)
    dims: Option<[u32; 2]>,
    /// 탐색기로 끌 때의 미리보기 PNG (썸네일을 읽은 뒤에 생김)
    preview: Vec<u8>,
}

struct FolderEntry {
    path: PathBuf,
    name: String,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
}

/// 설정 파일 안에서 이 앱 설정을 저장하는 키
const SETTINGS_KEY: &str = "imgview_settings";

/// 다음 실행 때 기억할 설정. 설정 파일(Windows: %APPDATA%\ImgView\data\app.ron)에 저장된다.
/// 창 크기·위치, 트리 너비와 펼침 상태는 eframe/egui가 같은 파일에 따로 저장한다.
#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
#[serde(default)] // 나중에 항목이 늘어나도 옛 설정 파일을 그대로 읽을 수 있게
struct Settings {
    last_folder: Option<PathBuf>,
    sort_key: SortKey,
    sort_descending: bool,
    thumb_size: f32,
    show_tree: bool,
    font_size: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            last_folder: None,
            sort_key: SortKey::Name,
            sort_descending: false,
            thumb_size: 150.0,
            show_tree: true,
            font_size: FONT_DEFAULT,
        }
    }
}

/// 정렬 기준 (탐색기의 Sort by 메뉴와 같은 항목)
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
enum SortKey {
    Name,
    DateModified,
    Type,
    Size,
    DateCreated,
}

impl SortKey {
    /// 메뉴에 보여줄 순서와 이름
    const MENU: [(SortKey, &'static str); 5] = [
        (SortKey::Name, "Name"),
        (SortKey::DateModified, "Date modified"),
        (SortKey::Type, "Type"),
        (SortKey::Size, "Size"),
        (SortKey::DateCreated, "Date created"),
    ];
}

/// 빈 공간 오른쪽 클릭 메뉴에서 고른 동작
enum GridAction {
    SortBy(SortKey),
    Descending(bool),
    Refresh,
}

/// 목록 순서가 바뀌어도 선택을 유지하기 위해 경로로 기억해 둔다.
struct SelectionSnapshot {
    selected: HashSet<PathBuf>,
    focus: Option<PathBuf>,
    view: Option<PathBuf>,
}

enum ScrollTarget {
    Top,
    Tile(usize),
}

#[derive(Clone, Copy)]
enum ZoomCmd {
    Fit,
    Actual,
    By(f32),
}

/// 지난 프레임에 그린 썸네일 격자 정보 (키보드 이동/스크롤 계산용)
#[derive(Default)]
struct GridMetrics {
    cols: usize,
    scroll_offset: f32,
    visible_rows: Range<usize>,
}

/// Del / Shift+Del 요청 (확인 창은 창 핸들이 있는 update에서 띄운다)
struct DeleteRequest {
    paths: Vec<PathBuf>,
    permanent: bool,
    after: AfterDelete,
}

/// 삭제가 끝난 뒤 이어서 할 일
struct AfterDelete {
    /// 목록: 지운 자리의 타일을 선택해서 키보드로 계속 작업할 수 있게
    select_tile: Option<usize>,
    /// 크게 보기: 다음(마지막이었으면 이전) 이미지를 보여준다
    view_next: Option<PathBuf>,
}

/// 앱 밖(탐색기)으로 끌어낼 파일들
struct DragOut {
    paths: Vec<PathBuf>,
    /// 커서 옆에 보여줄 미리보기 (Linux(X11)에서는 아직 쓰지 않음)
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    preview: Vec<u8>,
}

struct ViewState {
    index: usize,
    /// None이면 "화면에 맞춤". Some(z)이면 실제 픽셀 대비 배율 (1.0 = 100%).
    zoom: Option<f32>,
    /// 화면 중앙에서 이미지 중심까지의 이동량 (드래그로 이동)
    offset: Vec2,
    anim_start: f64,
    /// 이 이미지를 연 시각. 썸네일을 더블클릭했을 때 두 번째 클릭으로 바로 닫히지 않게 하는 데 쓴다.
    opened_at: f64,
    display_scale: f32,
}

impl ViewState {
    fn new(index: usize, now: f64) -> Self {
        Self {
            index,
            zoom: None,
            offset: Vec2::ZERO,
            anim_start: now,
            opened_at: now,
            display_scale: 1.0,
        }
    }

    fn zoom_by(&mut self, fit: f32, factor: f32, anchor: Vec2) {
        let current = self.zoom.unwrap_or(fit);
        self.set_zoom(fit, current * factor, anchor);
    }

    /// `anchor`(화면 중앙 기준 위치) 아래의 점이 제자리에 머물도록 배율을 바꾼다.
    fn set_zoom(&mut self, fit: f32, new_zoom: f32, anchor: Vec2) {
        let old = self.zoom.unwrap_or(fit).max(1e-6);
        let new = new_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.offset = anchor - (anchor - self.offset) * (new / old);
        self.zoom = Some(new);
    }
}

pub struct ImgViewApp {
    loader: Loader,
    gen: u64,
    now: f64,

    folder: Option<PathBuf>,
    path_input: String,
    folders: Vec<FolderEntry>,
    images: Vec<ImageEntry>,
    /// 오류 메시지 (빨간색, 다음 폴더를 열 때까지 표시)
    message: Option<String>,
    /// 안내 메시지와 표시 시작 시각 (잠시 후 사라짐)
    info: Option<(String, f64)>,

    tree: FolderTree,
    show_tree: bool,

    sort_key: SortKey,
    sort_descending: bool,

    thumb_size: f32,
    /// 목록/글자 크기 (설정 메뉴에서 조절)
    font_size: f32,
    /// 키보드 이동/Shift 선택의 기준 타일
    focus: Option<usize>,
    /// 선택된 타일들 (폴더들 다음에 이미지들이 이어지는 번호)
    selection: BTreeSet<usize>,
    pending_scroll: Option<ScrollTarget>,
    grid: GridMetrics,
    /// 폴더를 연 직후 잠깐 타일 클릭을 무시하는 시각 (초)
    ignore_tile_clicks_until: f64,

    view: Option<ViewState>,
    full_cache: HashMap<usize, FullImage>,
    full_pending: HashSet<usize>,
    full_errors: HashMap<usize, String>,
    max_texture_side: u32,

    copy_jobs: Vec<Receiver<CopyReport>>,
    delete_jobs: Vec<(Receiver<DeleteReport>, AfterDelete)>,
    pending_delete: Option<DeleteRequest>,
    drag_out: Option<DragOut>,
    /// 탐색기로 끌기가 끝난 뒤 "마우스 버튼 놓음"을 egui에 대신 알려야 하는지
    release_pointer: bool,

    fullscreen: bool,
    title: String,
    /// 지난번 저장된 창 위치. 첫 프레임에서 화면 밖이었는지 확인한 뒤 비운다.
    startup_window: Option<SavedWindow>,
}

impl ImgViewApp {
    pub fn new(cc: &eframe::CreationContext<'_>, start_path: Option<PathBuf>) -> Self {
        let settings: Settings = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, SETTINGS_KEY))
            .unwrap_or_default();
        let font_size = clamp_font_size(settings.font_size);
        setup_style(&cc.egui_ctx, font_size);
        let thumb_size = if settings.thumb_size.is_finite() {
            settings.thumb_size.clamp(THUMB_MIN, THUMB_MAX)
        } else {
            Settings::default().thumb_size
        };

        let mut app = Self {
            loader: Loader::new(&cc.egui_ctx),
            gen: 0,
            now: 0.0,
            folder: None,
            path_input: String::new(),
            folders: Vec::new(),
            images: Vec::new(),
            message: None,
            info: None,
            tree: FolderTree::new(),
            show_tree: settings.show_tree,
            sort_key: settings.sort_key,
            sort_descending: settings.sort_descending,
            thumb_size,
            font_size,
            focus: None,
            selection: BTreeSet::new(),
            pending_scroll: None,
            grid: GridMetrics::default(),
            ignore_tile_clicks_until: 0.0,
            view: None,
            full_cache: HashMap::new(),
            full_pending: HashSet::new(),
            full_errors: HashMap::new(),
            max_texture_side: 4096,
            copy_jobs: Vec::new(),
            delete_jobs: Vec::new(),
            pending_delete: None,
            drag_out: None,
            release_pointer: false,
            fullscreen: false,
            title: String::new(),
            startup_window: cc.storage.and_then(|storage| {
                eframe::get_value(storage, placement::WINDOW_STORAGE_KEY)
            }),
        };

        // 실행할 때 경로를 주면 그것을, 아니면 마지막으로 보던 폴더를 연다.
        // 마지막 폴더가 지워졌으면 남아 있는 가장 가까운 상위 폴더를 연다.
        let start = start_path.or_else(|| {
            settings
                .last_folder
                .as_deref()
                .and_then(|p| p.ancestors().find(|a| a.is_dir()))
                .map(Path::to_path_buf)
        });
        if let Some(path) = start {
            app.open_path(&path);
        }
        app
    }

    fn settings(&self) -> Settings {
        Settings {
            last_folder: self.folder.clone(),
            sort_key: self.sort_key,
            sort_descending: self.sort_descending,
            thumb_size: self.thumb_size,
            show_tree: self.show_tree,
            font_size: self.font_size,
        }
    }

    // ───────────────────────── 폴더 / 파일 열기 ─────────────────────────

    /// 폴더면 목록을 열고, 이미지 파일이면 그 폴더를 연 뒤 해당 이미지를 크게 보여준다.
    fn open_path(&mut self, path: &Path) {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        if path.is_dir() {
            self.open_folder(path);
        } else if path.is_file() {
            if let Some(parent) = path.parent() {
                if self.open_folder(parent.to_path_buf()) {
                    if let Some(i) = self.images.iter().position(|e| e.path == path) {
                        self.open_viewer(i);
                    }
                }
            }
        } else {
            self.message = Some(format!("경로를 찾을 수 없습니다: {}", display_path(&path)));
        }
    }

    fn open_folder(&mut self, path: PathBuf) -> bool {
        let path = std::path::absolute(&path).unwrap_or(path);
        let (folders, images) = match read_folder(&path) {
            Ok(lists) => lists,
            Err(e) => {
                self.message = Some(e);
                return false;
            }
        };

        self.gen = self.loader.reset();
        self.folders = folders;
        self.images = images;
        self.sort_entries();
        self.path_input = display_path(&path);
        self.tree.reveal(&path);
        self.folder = Some(path);
        self.message = None;
        self.clear_selection();
        self.pending_scroll = Some(ScrollTarget::Top);
        self.grid.scroll_offset = 0.0;
        self.ignore_tile_clicks_until = self.now + 0.4;
        self.view = None;
        self.full_cache.clear();
        self.full_pending.clear();
        self.full_errors.clear();
        true
    }

    /// 현재 폴더를 다시 읽는다. 스크롤 위치, 선택, 이미 만든 썸네일은 유지한다.
    fn reload_folder(&mut self) {
        let Some(path) = self.folder.clone() else {
            return;
        };
        let (folders, mut images) = match read_folder(&path) {
            Ok(lists) => lists,
            Err(e) => {
                self.message = Some(e);
                return;
            }
        };

        let snapshot = self.snapshot_selection();
        let mut old: HashMap<PathBuf, ImageEntry> =
            self.images.drain(..).map(|e| (e.path.clone(), e)).collect();
        for entry in &mut images {
            if let Some(prev) = old.remove(&entry.path) {
                if prev.file_size == entry.file_size && matches!(prev.thumb, ThumbState::Ready(_)) {
                    entry.thumb = prev.thumb;
                    entry.dims = prev.dims;
                    entry.preview = prev.preview;
                }
            }
        }

        self.gen = self.loader.reset();
        self.folders = folders;
        self.images = images;
        self.sort_entries();
        self.restore_selection(snapshot);
        self.tree.refresh(&path);
    }

    // ───────────────────────── 정렬 ─────────────────────────

    fn sort_entries(&mut self) {
        let (key, descending) = (self.sort_key, self.sort_descending);
        self.folders
            .sort_by(|a, b| directed(compare_folders(a, b, key), descending));
        self.images
            .sort_by(|a, b| directed(compare_images(a, b, key), descending));
    }

    /// 정렬 기준이 바뀌었을 때: 다시 정렬하고 선택은 경로 기준으로 유지한다.
    fn resort(&mut self) {
        let snapshot = self.snapshot_selection();
        self.sort_entries();
        // 대기 중인 썸네일 작업은 옛 순번을 쓰므로 버리고 다시 요청한다 (이미 만든 썸네일은 그대로).
        self.gen = self.loader.reset();
        for entry in &mut self.images {
            if matches!(entry.thumb, ThumbState::Loading) {
                entry.thumb = ThumbState::NotRequested;
            }
        }
        self.restore_selection(snapshot);
        if let Some(tile) = self.focus {
            self.pending_scroll = Some(ScrollTarget::Tile(tile));
        }
    }

    fn snapshot_selection(&self) -> SelectionSnapshot {
        SelectionSnapshot {
            selected: self
                .selection
                .iter()
                .filter_map(|&t| self.tile_path(t).map(Path::to_path_buf))
                .collect(),
            focus: self
                .focus
                .and_then(|t| self.tile_path(t).map(Path::to_path_buf)),
            view: self
                .view
                .as_ref()
                .and_then(|v| self.images.get(v.index))
                .map(|e| e.path.clone()),
        }
    }

    fn restore_selection(&mut self, snapshot: SelectionSnapshot) {
        self.selection = (0..self.tile_count())
            .filter(|&t| {
                self.tile_path(t)
                    .is_some_and(|p| snapshot.selected.contains(p))
            })
            .collect();
        self.focus = snapshot.focus.and_then(|fp| {
            (0..self.tile_count()).find(|&t| self.tile_path(t) == Some(fp.as_path()))
        });
        if let Some(vp) = snapshot.view {
            match self.images.iter().position(|e| e.path == vp) {
                Some(i) => {
                    if let Some(view) = &mut self.view {
                        view.index = i;
                    }
                }
                None => self.view = None,
            }
        }
        // 원본 이미지 캐시는 순번 기준이라 비운다.
        self.full_cache.clear();
        self.full_pending.clear();
        self.full_errors.clear();
    }

    /// 빈 공간 오른쪽 클릭 메뉴 (탐색기와 같은 영문 메뉴)
    fn grid_context_menu(&mut self, ui: &mut egui::Ui, action: &mut Option<GridAction>) {
        ui.menu_button("Sort by", |ui| {
            for (key, label) in SortKey::MENU {
                if ui.radio(self.sort_key == key, label).clicked() {
                    *action = Some(GridAction::SortBy(key));
                    ui.close_menu();
                }
            }
            ui.separator();
            if ui.radio(!self.sort_descending, "Ascending").clicked() {
                *action = Some(GridAction::Descending(false));
                ui.close_menu();
            }
            if ui.radio(self.sort_descending, "Descending").clicked() {
                *action = Some(GridAction::Descending(true));
                ui.close_menu();
            }
        });
        ui.menu_button("Settings", |ui| self.font_size_menu(ui));
        ui.separator();
        if ui.button("Refresh").clicked() {
            *action = Some(GridAction::Refresh);
            ui.close_menu();
        }
    }

    /// Settings 메뉴 안의 글꼴 크기 항목: 현재 값(기본값)을 보여주고 슬라이더로 조절한다.
    fn font_size_menu(&mut self, ui: &mut egui::Ui) {
        if ui
            .button(format!(
                "Font size: {:.1}  (default {FONT_DEFAULT:.1})",
                self.font_size
            ))
            .clicked()
        {
            self.set_font_size(ui.ctx(), FONT_DEFAULT);
            ui.close_menu();
        }
        let response = ui.add(
            egui::Slider::new(&mut self.font_size, FONT_MIN..=FONT_MAX)
                .step_by(0.5)
                .fixed_decimals(1)
                .show_value(true),
        );
        if response.changed() {
            self.set_font_size(ui.ctx(), self.font_size);
        }
        ui.separator();
        ui.label("Tile names and all UI text");
    }

    /// 글꼴 크기를 적용하고 설정에 저장될 값도 함께 맞춘다.
    fn set_font_size(&mut self, ctx: &egui::Context, font_size: f32) {
        self.font_size = clamp_font_size(font_size);
        apply_font_size(ctx, self.font_size);
    }

    fn pick_folder(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_title("이미지 폴더 선택");
        if let Some(dir) = &self.folder {
            dialog = dialog.set_directory(dir);
        }
        if let Some(dir) = dialog.pick_folder() {
            self.open_folder(dir);
        }
    }

    fn go_parent(&mut self) {
        let Some(current) = self.folder.clone() else {
            return;
        };
        let Some(parent) = current.parent().map(Path::to_path_buf) else {
            return;
        };
        if self.open_folder(parent) {
            // 방금 있던 폴더를 선택해 둔다.
            if let Some(t) = self.folders.iter().position(|f| f.path == current) {
                self.select_only(t);
                self.pending_scroll = Some(ScrollTarget::Tile(t));
            }
        }
    }

    // ───────────────────────── 타일 / 선택 ─────────────────────────

    fn tile_count(&self) -> usize {
        self.folders.len() + self.images.len()
    }

    fn tile_path(&self, tile: usize) -> Option<&Path> {
        let folder_count = self.folders.len();
        if tile < folder_count {
            Some(&self.folders[tile].path)
        } else {
            self.images
                .get(tile - folder_count)
                .map(|e| e.path.as_path())
        }
    }

    fn select_only(&mut self, tile: usize) {
        self.selection.clear();
        self.selection.insert(tile);
        self.focus = Some(tile);
    }

    fn clear_selection(&mut self) {
        self.selection.clear();
        self.focus = None;
    }

    /// Ctrl+클릭: 선택에 넣고 빼기, Shift+클릭: 범위 선택, 그냥 클릭: 선택만.
    /// 여는 것은 더블클릭 (이미지는 크게 보기, 폴더는 이동). 왼쪽 트리는 한 번 클릭으로 연다.
    fn on_tile_clicked(&mut self, tile: usize, modifiers: egui::Modifiers) {
        if modifiers.command {
            if !self.selection.remove(&tile) {
                self.selection.insert(tile);
            }
            self.focus = Some(tile);
        } else if modifiers.shift {
            let anchor = self.focus.unwrap_or(tile);
            self.selection = (anchor.min(tile)..=anchor.max(tile)).collect();
        } else {
            self.select_only(tile);
        }
    }

    fn activate_tile(&mut self, tile: usize) {
        let folder_count = self.folders.len();
        if tile < folder_count {
            let path = self.folders[tile].path.clone();
            self.open_folder(path);
        } else {
            self.open_viewer(tile - folder_count);
        }
    }

    /// 타일을 끌기 시작하면 선택된 파일들을 탐색기로 내보낸다.
    /// 끈 타일이 선택에 없으면: Ctrl을 누른 상태면 선택에 더하고, 아니면 그 타일 하나만 끈다.
    fn begin_drag_out(&mut self, tile: usize, modifiers: egui::Modifiers) {
        if !self.selection.contains(&tile) {
            if modifiers.command {
                self.selection.insert(tile);
                self.focus = Some(tile);
            } else {
                self.select_only(tile);
            }
        }
        let paths: Vec<PathBuf> = self
            .selection
            .iter()
            .filter_map(|&t| self.tile_path(t).map(Path::to_path_buf))
            .collect();
        if paths.is_empty() {
            return;
        }
        let folder_count = self.folders.len();
        let preview = tile
            .checked_sub(folder_count)
            .and_then(|i| self.images.get(i))
            .map(|e| e.preview.clone())
            .filter(|png| !png.is_empty())
            .unwrap_or_else(|| loader::solid_png(64, 48, FOLDER_COLOR.to_array()));
        self.drag_out = Some(DragOut { paths, preview });
    }

    #[cfg(any(windows, target_os = "macos"))]
    fn run_drag_out(&mut self, frame: &eframe::Frame, drag: DragOut) {
        let options = drag::Options {
            mode: drag::DragMode::Copy,
            ..Default::default()
        };
        // Windows에서는 끌기가 끝날 때까지 여기서 기다린다.
        let result = drag::start_drag(
            frame,
            drag::DragItem::Files(drag.paths),
            drag::Image::Raw(drag.preview),
            |_result, _cursor| {},
            options,
        );
        if let Err(e) = result {
            self.message = Some(format!("끌어서 복사를 시작할 수 없습니다: {e}"));
        }
        // 끄는 동안 마우스 버튼을 놓은 사실이 앱에 전달되지 않으므로 다음 프레임에 대신 알려준다.
        self.release_pointer = true;
    }

    /// Linux(X11): 끌기는 백그라운드 스레드에서 버튼을 놓을 때까지 진행되고, 그동안 화면은 계속 그려진다.
    /// 버튼을 누르고 있는 동안 매 프레임 불리지만 이미 끌고 있으면 `xdnd`가 무시한다.
    #[cfg(target_os = "linux")]
    fn run_drag_out(&mut self, frame: &eframe::Frame, drag: DragOut) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        // 끄는 동안 커서를 바꿀 앱 창의 X11 창 번호
        let app_window = frame
            .window_handle()
            .ok()
            .and_then(|handle| match handle.as_raw() {
                RawWindowHandle::Xlib(xlib) => u32::try_from(xlib.window).ok(),
                RawWindowHandle::Xcb(xcb) => Some(xcb.window.get()),
                _ => None,
            });
        if let Err(e) = crate::xdnd::start_drag(drag.paths, app_window) {
            self.message = Some(format!("끌어서 복사를 시작할 수 없습니다: {e}"));
        }
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    fn run_drag_out(&mut self, _frame: &eframe::Frame, _drag: DragOut) {
        self.message =
            Some("이 운영체제에서는 앱 밖으로 끌어서 복사하기를 지원하지 않습니다".to_owned());
    }

    // ───────────────────────── 복사 (탐색기 → 앱) ─────────────────────────

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        match self.folder.clone() {
            // 폴더가 열려 있으면 그 폴더로 복사
            Some(dest) => self
                .copy_jobs
                .push(fileops::copy_into_background(ctx.clone(), dropped, dest)),
            // 아무 폴더도 열려 있지 않으면 놓은 폴더/이미지를 연다
            None => self.open_path(&dropped[0]),
        }
    }

    fn poll_copy_jobs(&mut self) {
        let mut finished = Vec::new();
        self.copy_jobs.retain(|rx| match rx.try_recv() {
            Ok(report) => {
                finished.push(report);
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => false,
        });
        for report in finished {
            self.finish_copy(report);
        }
    }

    fn finish_copy(&mut self, report: CopyReport) {
        if report.copied == 0 && report.errors.is_empty() {
            return; // 같은 폴더에 다시 놓은 경우: 아무 일도 없음
        }
        let dest_name = report
            .dest
            .file_name()
            .map_or_else(|| display_path(&report.dest), |n| nfc(&n.to_string_lossy()));
        let summary = format!("{}개 항목을 '{dest_name}' 폴더로 복사했습니다", report.copied);
        if report.errors.is_empty() {
            self.info = Some((summary, self.now));
        } else {
            let shown: Vec<&str> = report.errors.iter().take(3).map(String::as_str).collect();
            let more = report.errors.len().saturating_sub(shown.len());
            let mut text = format!("{summary}. 실패 {}개: {}", report.errors.len(), shown.join(" / "));
            if more > 0 {
                text.push_str(&format!(" 외 {more}개"));
            }
            self.message = Some(text);
        }
        if self.folder.as_deref() == Some(report.dest.as_path()) {
            self.reload_folder();
        } else {
            self.tree.refresh(&report.dest);
        }
    }

    // ───────────────────────── 삭제 (Del: 휴지통, Shift+Del: 영구 삭제) ─────────────────────────

    /// 목록에서 선택한 항목들을 지운다.
    fn request_delete_selection(&mut self, permanent: bool) {
        if !self.delete_jobs.is_empty() {
            return; // 앞의 삭제가 아직 끝나지 않음
        }
        let paths: Vec<PathBuf> = self
            .selection
            .iter()
            .filter_map(|&t| self.tile_path(t).map(Path::to_path_buf))
            .collect();
        if paths.is_empty() {
            return;
        }
        self.pending_delete = Some(DeleteRequest {
            paths,
            permanent,
            after: AfterDelete {
                select_tile: self.selection.first().copied(),
                view_next: None,
            },
        });
    }

    /// 크게 보기에서 지금 보고 있는 이미지를 지운다.
    fn request_delete_current_image(&mut self, permanent: bool) {
        if !self.delete_jobs.is_empty() {
            return;
        }
        let Some(index) = self.view.as_ref().map(|v| v.index) else {
            return;
        };
        let Some(entry) = self.images.get(index) else {
            return;
        };
        let next = self
            .images
            .get(index + 1)
            .or_else(|| index.checked_sub(1).and_then(|i| self.images.get(i)))
            .map(|e| e.path.clone());
        self.pending_delete = Some(DeleteRequest {
            paths: vec![entry.path.clone()],
            permanent,
            after: AfterDelete {
                select_tile: None,
                view_next: next,
            },
        });
    }

    fn run_delete(&mut self, ctx: &egui::Context, frame: &eframe::Frame, request: DeleteRequest) {
        if request.permanent && !confirm_permanent_delete(frame, &request.paths) {
            return;
        }
        let rx = fileops::delete_background(ctx.clone(), request.paths, request.permanent);
        self.delete_jobs.push((rx, request.after));
    }

    fn poll_delete_jobs(&mut self) {
        let mut finished = Vec::new();
        let mut i = 0;
        while i < self.delete_jobs.len() {
            match self.delete_jobs[i].0.try_recv() {
                Ok(report) => {
                    let (_, after) = self.delete_jobs.remove(i);
                    finished.push((report, after));
                }
                Err(TryRecvError::Empty) => i += 1,
                Err(TryRecvError::Disconnected) => {
                    self.delete_jobs.remove(i);
                }
            }
        }
        for (report, after) in finished {
            self.finish_delete(report, after);
        }
    }

    fn finish_delete(&mut self, report: DeleteReport, after: AfterDelete) {
        let how = if report.permanent {
            "영구 삭제했습니다"
        } else {
            "휴지통으로 이동했습니다"
        };
        let summary = format!("{}개 항목을 {how}", report.deleted);
        if report.errors.is_empty() {
            self.info = Some((summary, self.now));
        } else {
            let shown: Vec<&str> = report.errors.iter().take(3).map(String::as_str).collect();
            let mut text = format!("{summary}. 실패: {}", shown.join(" / "));
            if !report.permanent {
                text.push_str(" (휴지통에 넣을 수 없는 항목은 지우지 않았습니다. Shift+Del로 영구 삭제할 수 있습니다)");
            }
            self.message = Some(text);
        }

        let was_viewing = self.view.is_some();
        self.reload_folder();
        if was_viewing {
            // 보고 있던 이미지가 지워졌으면 다음 이미지로
            if self.view.is_none() {
                if let Some(next) = after.view_next {
                    if let Some(i) = self.images.iter().position(|e| e.path == next) {
                        self.open_viewer(i);
                    }
                }
            }
        } else if self.selection.is_empty() {
            if let Some(tile) = after.select_tile {
                let count = self.tile_count();
                if count > 0 {
                    let tile = tile.min(count - 1);
                    self.select_only(tile);
                    self.pending_scroll = Some(ScrollTarget::Tile(tile));
                }
            }
        }
    }

    fn paint_drop_overlay(&self, ctx: &egui::Context) {
        if ctx.input(|i| i.raw.hovered_files.is_empty()) {
            return;
        }
        let text = match &self.folder {
            Some(folder) => format!(
                "여기에 놓으면 현재 폴더로 복사합니다\n{}",
                display_path(folder)
            ),
            None => "놓으면 해당 폴더나 이미지를 엽니다".to_owned(),
        };
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("drop_overlay"),
        ));
        let rect = ctx.screen_rect();
        painter.rect_filled(rect, 0.0, Color32::from_black_alpha(170));
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            text,
            FontId::proportional(20.0),
            Color32::WHITE,
        );
    }

    // ───────────────────────── 로딩 결과 처리 ─────────────────────────

    fn process_results(&mut self) {
        while let Some(result) = self.loader.try_recv() {
            match result {
                LoadResult::Thumb { gen, index, result } => {
                    if gen != self.gen {
                        continue;
                    }
                    let Some(entry) = self.images.get_mut(index) else {
                        continue;
                    };
                    match result {
                        Ok(thumb) => {
                            entry.dims = Some(thumb.orig_size);
                            entry.preview = thumb.preview_png;
                            entry.thumb = ThumbState::Ready(thumb.texture);
                        }
                        Err(_) => entry.thumb = ThumbState::Failed,
                    }
                }
                LoadResult::Full { gen, index, result } => {
                    // 취소된 요청(이미 다른 이미지로 넘어감)이면 버린다.
                    if gen != self.gen || !self.full_pending.remove(&index) {
                        continue;
                    }
                    match result {
                        Ok(full) => {
                            if let Some(entry) = self.images.get_mut(index) {
                                entry.dims = Some(full.orig_size);
                            }
                            if let Some(view) = &mut self.view {
                                if view.index == index {
                                    view.anim_start = self.now;
                                }
                            }
                            self.full_cache.insert(index, full);
                        }
                        Err(e) => {
                            self.full_errors.insert(index, e);
                        }
                    }
                }
            }
        }
    }

    fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        if self.fullscreen != on {
            self.fullscreen = on;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = if let Some(view) = &self.view {
            let name = self.images.get(view.index).map_or("", |e| e.name.as_str());
            format!("{name} - ImgView")
        } else if let Some(folder) = &self.folder {
            format!("{} - ImgView", display_path(folder))
        } else {
            "ImgView".to_owned()
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    // ───────────────────────── 브라우저(트리 + 썸네일 목록) ─────────────────────────

    fn ui_browser(&mut self, ctx: &egui::Context) {
        self.browser_keys(ctx);

        egui::TopBottomPanel::top("browser_toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let (label, tip) = if self.show_tree {
                    ("<<", "폴더 트리 숨기기")
                } else {
                    (">>", "폴더 트리 보이기")
                };
                if ui.button(label).on_hover_text(tip).clicked() {
                    self.show_tree = !self.show_tree;
                }
                ui.separator();
                if ui.button("📂 폴더 열기").on_hover_text("Ctrl+O").clicked() {
                    self.pick_folder();
                }
                let has_parent = self.folder.as_deref().and_then(Path::parent).is_some();
                if ui
                    .add_enabled(has_parent, egui::Button::new("⬆ 상위 폴더"))
                    .on_hover_text("Backspace")
                    .clicked()
                {
                    self.go_parent();
                }
                ui.separator();
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add(
                        egui::Slider::new(&mut self.thumb_size, THUMB_MIN..=THUMB_MAX)
                            .show_value(false),
                    );
                    ui.label("썸네일 크기");
                    ui.separator();
                    let width = ui.available_width();
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.path_input)
                            .desired_width(width)
                            .hint_text("폴더 경로를 입력하고 Enter"),
                    );
                    if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        let path = PathBuf::from(self.path_input.trim());
                        self.open_path(&path);
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("browser_status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let folder_count = self.folders.len();
                ui.label(format!(
                    "폴더 {}개 · 이미지 {}개",
                    folder_count,
                    self.images.len()
                ));
                if !self.copy_jobs.is_empty() {
                    ui.separator();
                    ui.spinner();
                    ui.label("복사 중…");
                }
                if !self.delete_jobs.is_empty() {
                    ui.separator();
                    ui.spinner();
                    ui.label("삭제 중…");
                }
                if self.selection.len() > 1 {
                    let (count, bytes) = self
                        .selection
                        .iter()
                        .filter_map(|&t| t.checked_sub(folder_count).and_then(|i| self.images.get(i)))
                        .fold((0usize, 0u64), |(c, b), e| (c + 1, b + e.file_size));
                    ui.separator();
                    ui.label(format!("{}개 선택됨", self.selection.len()));
                    if count > 0 {
                        ui.label(format!("(이미지 {count}개, {})", format_size(bytes)));
                    }
                } else if let Some(t) = self.focus.filter(|t| self.selection.contains(t)) {
                    if t < folder_count {
                        ui.separator();
                        ui.label(format!("📁 {}", self.folders[t].name));
                    } else if let Some(entry) = self.images.get(t - folder_count) {
                        ui.separator();
                        ui.label(entry.name.as_str());
                        if let Some([w, h]) = entry.dims {
                            ui.separator();
                            ui.label(format!("{w} × {h}"));
                        }
                        ui.separator();
                        ui.label(format_size(entry.file_size));
                    }
                }
                if let Some((text, _)) = &self.info {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.colored_label(INFO_COLOR, text.as_str());
                    });
                }
            });
        });

        if self.show_tree {
            egui::SidePanel::left("folder_tree")
                .resizable(true)
                .default_width(250.0)
                .width_range(160.0..=600.0)
                .show(ctx, |ui| {
                    if let Some(path) = self.tree.show(ui, self.folder.as_deref()) {
                        // 트리에서 고른 폴더는 이미 보이므로 다시 스크롤하지 않는다.
                        if self.open_folder(path) {
                            self.tree.cancel_reveal();
                        }
                    }
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).fill(BG_GRID))
            .show(ctx, |ui| {
                if let Some(message) = &self.message {
                    ui.colored_label(ERROR_COLOR, message.as_str());
                }
                if self.folder.is_none() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.35);
                        ui.heading("이미지 폴더를 선택하세요");
                        ui.add_space(6.0);
                        ui.label("왼쪽 트리에서 폴더를 고르거나, '폴더 열기'를 누르거나, 폴더나 이미지를 이 창에 끌어다 놓으세요.");
                        ui.add_space(10.0);
                        if ui.button("📂 폴더 열기").clicked() {
                            self.pick_folder();
                        }
                    });
                    return;
                }
                if self.folders.is_empty() && self.images.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label("이 폴더에는 이미지가 없습니다. 탐색기에서 파일을 끌어다 놓으면 복사됩니다.");
                    });
                    return;
                }
                self.thumbnail_grid(ui);
            });
    }

    fn browser_keys(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return; // 경로 입력 중
        }
        let (open_dialog, select_all, refresh, escape, delete) = ctx.input(|i| {
            (
                i.modifiers.command && i.key_pressed(Key::O),
                i.modifiers.command && i.key_pressed(Key::A),
                i.key_pressed(Key::F5),
                i.key_pressed(Key::Escape),
                // Some(true) = Shift+Del (영구 삭제)
                i.key_pressed(Key::Delete).then_some(i.modifiers.shift),
            )
        });
        let (left, right, up, down, page_up, page_down, home, end, enter, back) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::PageUp),
                i.key_pressed(Key::PageDown),
                i.key_pressed(Key::Home),
                i.key_pressed(Key::End),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Backspace),
            )
        });

        if open_dialog {
            self.pick_folder();
            return;
        }
        if let Some(permanent) = delete {
            self.request_delete_selection(permanent);
            return;
        }
        if refresh {
            self.reload_folder();
            return;
        }
        if back {
            self.go_parent();
            return;
        }
        if escape {
            self.clear_selection();
            return;
        }

        let total = self.tile_count();
        if total == 0 {
            return;
        }
        if select_all {
            self.selection = (0..total).collect();
            return;
        }
        let last = total - 1;
        let cols = self.grid.cols.max(1);
        let page = self.grid.visible_rows.len().saturating_sub(1).max(1) * cols;
        let current = self.focus;
        let base = current.unwrap_or(0);

        let mut next = current;
        if current.is_none() && (left || right || up || down || page_up || page_down) {
            next = Some(0);
        } else {
            if left {
                next = Some(base.saturating_sub(1));
            }
            if right {
                next = Some((base + 1).min(last));
            }
            if up {
                next = Some(base.saturating_sub(cols));
            }
            if down {
                next = Some((base + cols).min(last));
            }
            if page_up {
                next = Some(base.saturating_sub(page));
            }
            if page_down {
                next = Some((base + page).min(last));
            }
        }
        if home {
            next = Some(0);
        }
        if end {
            next = Some(last);
        }

        if next != current {
            if let Some(t) = next {
                self.select_only(t);
                self.pending_scroll = Some(ScrollTarget::Tile(t));
            }
        }
        if enter {
            if let Some(t) = self.focus {
                self.activate_tile(t);
            }
        }
    }

    fn thumbnail_grid(&mut self, ui: &mut egui::Ui) {
        let folder_count = self.folders.len();
        let total = self.tile_count();
        let tile_w = self.thumb_size + 2.0 * TILE_PAD;
        // 이름 줄 높이도 글꼴 크기를 따라간다 (기본 12.5에서 예전 값 22).
        let label_h = (self.font_size * 1.76).max(LABEL_HEIGHT);
        let tile_h = self.thumb_size + TILE_PAD + label_h;
        // 오른쪽 스크롤바 자리를 조금 남긴다.
        let usable_w = (ui.available_width() - 12.0).max(tile_w);
        let cols = (((usable_w + TILE_SPACING) / (tile_w + TILE_SPACING)).floor() as usize).max(1);
        let rows = total.div_ceil(cols);
        let row_pitch = tile_h + TILE_SPACING;
        let view_h = ui.available_height();
        let (modifiers, press_origin, pointer_pos) = ui.input(|i| {
            (
                i.modifiers,
                i.pointer.press_origin(),
                i.pointer.interact_pos(),
            )
        });
        let moved_enough = match (press_origin, pointer_pos) {
            (Some(a), Some(b)) => a.distance(b) >= DRAG_OUT_DISTANCE,
            _ => false,
        };

        ui.spacing_mut().item_spacing = vec2(TILE_SPACING, TILE_SPACING);
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(target) = self.pending_scroll.take() {
            let y = match target {
                ScrollTarget::Top => 0.0,
                ScrollTarget::Tile(t) => {
                    // 선택한 타일이 화면 밖이면 최소한만 스크롤
                    let top = (t / cols) as f32 * row_pitch;
                    let bottom = top + tile_h;
                    let current = self.grid.scroll_offset;
                    if top < current {
                        top
                    } else if bottom > current + view_h {
                        bottom - view_h
                    } else {
                        current
                    }
                }
            };
            area = area.vertical_scroll_offset(y.max(0.0));
        }

        let mut clicked = None;
        let mut double_clicked = None;
        let mut drag_tile = None;
        let mut background_clicked = false;
        let mut menu_action: Option<GridAction> = None;
        let output = area.show_rows(ui, tile_h, rows, |ui, row_range| {
            // 빈 공간. 타일보다 먼저 등록해야 타일이 위에 놓여 타일 클릭을 먼저 받는다.
            let background = ui.interact(
                ui.clip_rect(),
                egui::Id::new("grid_background"),
                Sense::click(),
            );
            if background.clicked() {
                background_clicked = true;
            }
            background.context_menu(|ui| self.grid_context_menu(ui, &mut menu_action));

            for row in row_range.clone() {
                let row_width = ui.available_width();
                let (row_rect, _) = ui.allocate_exact_size(vec2(row_width, tile_h), Sense::hover());
                for col in 0..cols {
                    let tile = row * cols + col;
                    if tile >= total {
                        break;
                    }
                    let min = pos2(
                        row_rect.left() + col as f32 * (tile_w + TILE_SPACING),
                        row_rect.top(),
                    );
                    let rect = Rect::from_min_size(min, vec2(tile_w, tile_h));
                    let response =
                        ui.interact(rect, egui::Id::new(("tile", tile)), Sense::click_and_drag());
                    self.paint_tile(ui, rect, tile, response.hovered());
                    if response.double_clicked() {
                        double_clicked = Some(tile);
                    } else if response.clicked() {
                        clicked = Some(tile);
                    }
                    if response.dragged_by(egui::PointerButton::Primary) && moved_enough {
                        drag_tile = Some(tile);
                    }
                }
            }
            row_range
        });

        self.grid = GridMetrics {
            cols,
            scroll_offset: output.state.offset.y,
            visible_rows: output.inner,
        };

        // 화면에서 멀어진 썸네일 작업은 취소해서, 지금 보이는 것부터 읽히게 한다.
        let keep_from = self.grid.visible_rows.start.saturating_sub(2) * cols;
        let keep_to = (self.grid.visible_rows.end + 2) * cols;
        let dropped = self
            .loader
            .retain_thumbs(|i| (keep_from..keep_to).contains(&(i + folder_count)));
        for i in dropped {
            if let Some(entry) = self.images.get_mut(i) {
                if matches!(entry.thumb, ThumbState::Loading) {
                    entry.thumb = ThumbState::NotRequested;
                }
            }
        }

        if let Some(tile) = drag_tile {
            self.begin_drag_out(tile, modifiers);
        } else if self.now < self.ignore_tile_clicks_until {
            // 폴더를 연 직후: 폴더를 더블클릭한 두 번째 클릭이 새 폴더의 타일을 누르지 않게 무시
        } else if let Some(tile) = double_clicked {
            // 더블클릭: 이미지는 크게 보기, 폴더는 그 폴더로 이동
            self.select_only(tile);
            self.activate_tile(tile);
        } else if let Some(tile) = clicked {
            self.on_tile_clicked(tile, modifiers);
        } else if background_clicked && !modifiers.command && !modifiers.shift {
            self.clear_selection(); // 탐색기처럼 빈 곳을 클릭하면 선택 해제
        }

        match menu_action {
            Some(GridAction::SortBy(key)) => {
                self.sort_key = key;
                self.resort();
            }
            Some(GridAction::Descending(descending)) => {
                self.sort_descending = descending;
                self.resort();
            }
            Some(GridAction::Refresh) => self.reload_folder(),
            None => {}
        }
    }

    fn paint_tile(&mut self, ui: &egui::Ui, rect: Rect, tile: usize, hovered: bool) {
        let painter = ui.painter_at(rect);
        let selected = self.selection.contains(&tile);
        let background = if selected {
            BG_TILE_SELECTED
        } else if hovered {
            BG_TILE_HOVER
        } else {
            BG_TILE
        };
        painter.rect_filled(rect, 4.0, background);

        let image_area = Rect::from_min_size(
            rect.min + vec2(TILE_PAD, TILE_PAD),
            vec2(rect.width() - 2.0 * TILE_PAD, self.thumb_size),
        );
        let label_rect = Rect::from_min_max(
            pos2(rect.left() + 4.0, image_area.bottom()),
            pos2(rect.right() - 4.0, rect.bottom()),
        );
        let label_color = if selected {
            Color32::WHITE
        } else {
            Color32::from_gray(205)
        };

        let folder_count = self.folders.len();
        let font_size = self.font_size;
        if tile < folder_count {
            paint_folder_icon(&painter, image_area);
            paint_label(
                ui,
                &painter,
                label_rect,
                &self.folders[tile].name,
                label_color,
                font_size,
            );
            return;
        }

        let index = tile - folder_count;
        let gen = self.gen;
        let entry = &mut self.images[index];
        if matches!(entry.thumb, ThumbState::NotRequested) {
            // 화면에 보이는 타일만 썸네일을 요청한다 (지연 로딩).
            self.loader.request_thumb(gen, index, entry.path.clone());
            entry.thumb = ThumbState::Loading;
        }
        match &entry.thumb {
            ThumbState::Ready(texture) => {
                let r = fit_rect(image_area, texture.size_vec2(), 1.0);
                painter.image(texture.id(), r, uv_full(), Color32::WHITE);
            }
            ThumbState::Failed => {
                painter.text(
                    image_area.center(),
                    Align2::CENTER_CENTER,
                    "⚠",
                    FontId::proportional(28.0),
                    ERROR_COLOR,
                );
            }
            ThumbState::NotRequested | ThumbState::Loading => {
                let r = Rect::from_center_size(image_area.center(), image_area.size() * 0.6);
                painter.rect_filled(r, 3.0, BG_PLACEHOLDER);
            }
        }
        paint_label(ui, &painter, label_rect, &entry.name, label_color, font_size);
    }

    // ───────────────────────── 뷰어(크게 보기) ─────────────────────────

    fn open_viewer(&mut self, index: usize) {
        if index >= self.images.len() {
            return;
        }
        self.view = Some(ViewState::new(index, self.now));
        self.select_only(self.folders.len() + index);
    }

    fn close_viewer(&mut self) {
        if let Some(view) = self.view.take() {
            let tile = self.folders.len() + view.index;
            self.select_only(tile);
            self.pending_scroll = Some(ScrollTarget::Tile(tile));
        }
        self.trim_full_cache();
    }

    fn go_to_image(&mut self, index: usize) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        if view.index == index || index >= self.images.len() {
            return;
        }
        *view = ViewState::new(index, self.now);
        self.select_only(self.folders.len() + index);
        self.trim_full_cache();
    }

    /// 현재 이미지와 바로 앞/뒤 이미지만 메모리에 남긴다.
    fn trim_full_cache(&mut self) {
        match self.view.as_ref().map(|v| v.index) {
            Some(current) => {
                let keep = move |i: usize| i.abs_diff(current) <= 1;
                self.full_cache.retain(|i, _| keep(*i));
                self.full_pending.retain(|i| keep(*i));
                self.loader.retain_full(keep);
            }
            None => {
                self.full_cache.clear();
                self.full_pending.clear();
                self.loader.retain_full(|_| false);
            }
        }
    }

    fn ensure_full(&mut self, index: usize, urgent: bool) {
        if index >= self.images.len()
            || self.full_cache.contains_key(&index)
            || self.full_pending.contains(&index)
            || self.full_errors.contains_key(&index)
        {
            return;
        }
        self.loader.request_full(
            self.gen,
            index,
            self.images[index].path.clone(),
            self.max_texture_side,
            urgent,
        );
        self.full_pending.insert(index);
    }

    fn ui_viewer(&mut self, ctx: &egui::Context) {
        let Some(index) = self.view.as_ref().map(|v| v.index) else {
            return;
        };
        let total = self.images.len();
        if index >= total {
            self.view = None;
            return;
        }
        let last = total - 1;
        let fullscreen = self.fullscreen;

        let mut target: Option<usize> = None;
        let mut zoom_cmd: Option<ZoomCmd> = None;
        let mut close = false;
        let mut exit_fullscreen = false;
        let mut toggle_fullscreen = false;
        let mut delete: Option<bool> = None;

        if !ctx.wants_keyboard_input() {
            ctx.input(|i| {
                if i.key_pressed(Key::Delete) {
                    delete = Some(i.modifiers.shift);
                }
                if i.key_pressed(Key::Escape) {
                    if fullscreen {
                        exit_fullscreen = true;
                    } else {
                        close = true;
                    }
                }
                if i.key_pressed(Key::Backspace) {
                    close = true;
                }
                if i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::PageUp) {
                    target = Some(index.saturating_sub(1));
                }
                if i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::PageDown) {
                    target = Some((index + 1).min(last));
                }
                if i.key_pressed(Key::Home) {
                    target = Some(0);
                }
                if i.key_pressed(Key::End) {
                    target = Some(last);
                }
                if i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals) {
                    zoom_cmd = Some(ZoomCmd::By(1.25));
                }
                if i.key_pressed(Key::Minus) {
                    zoom_cmd = Some(ZoomCmd::By(0.8));
                }
                if i.key_pressed(Key::Num0) {
                    zoom_cmd = Some(ZoomCmd::Fit);
                }
                if i.key_pressed(Key::Num1) {
                    zoom_cmd = Some(ZoomCmd::Actual);
                }
            });
        }

        // 지금 이미지를 먼저, 다 읽었으면 앞뒤 이미지를 미리 읽어 둔다.
        self.ensure_full(index, true);
        if self.full_cache.contains_key(&index) {
            if index < last {
                self.ensure_full(index + 1, false);
            }
            if index > 0 {
                self.ensure_full(index - 1, false);
            }
        }

        if !fullscreen {
            egui::TopBottomPanel::top("viewer_toolbar").show(ctx, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("◀ 목록으로").on_hover_text("Esc / 더블클릭").clicked() {
                        close = true;
                    }
                    ui.separator();
                    if ui
                        .add_enabled(index > 0, egui::Button::new("◀ 이전"))
                        .on_hover_text("← / PageUp")
                        .clicked()
                    {
                        target = Some(index - 1);
                    }
                    if ui
                        .add_enabled(index < last, egui::Button::new("다음 ▶"))
                        .on_hover_text("→ / PageDown")
                        .clicked()
                    {
                        target = Some(index + 1);
                    }
                    ui.separator();
                    if ui.button("화면 맞춤").on_hover_text("0").clicked() {
                        zoom_cmd = Some(ZoomCmd::Fit);
                    }
                    if ui.button("100%").on_hover_text("1").clicked() {
                        zoom_cmd = Some(ZoomCmd::Actual);
                    }
                    if ui.button("축소").on_hover_text("휠 아래로 / -").clicked() {
                        zoom_cmd = Some(ZoomCmd::By(0.8));
                    }
                    if ui.button("확대").on_hover_text("휠 위로 / +").clicked() {
                        zoom_cmd = Some(ZoomCmd::By(1.25));
                    }
                    ui.separator();
                    if ui.button("전체 화면").on_hover_text("F11").clicked() {
                        toggle_fullscreen = true;
                    }
                });
                ui.add_space(4.0);
            });

            egui::TopBottomPanel::bottom("viewer_status").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let entry = &self.images[index];
                    ui.label(egui::RichText::new(entry.name.as_str()).strong());
                    if let Some([w, h]) = entry.dims {
                        ui.separator();
                        ui.label(format!("{w} × {h}"));
                    }
                    ui.separator();
                    ui.label(format_size(entry.file_size));
                    if let Some(full) = self.full_cache.get(&index) {
                        if let Some(view) = &self.view {
                            ui.separator();
                            ui.label(format!("{:.0}%", view.display_scale * 100.0));
                        }
                        if full.frames.len() > 1 {
                            ui.separator();
                            ui.label(format!("애니메이션 {}프레임", full.frames.len()));
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(format!("{} / {}", index + 1, total));
                        if let Some(message) = &self.message {
                            ui.separator();
                            ui.colored_label(ERROR_COLOR, message.as_str());
                        } else if let Some((text, _)) = &self.info {
                            ui.separator();
                            ui.colored_label(INFO_COLOR, text.as_str());
                        }
                    });
                });
            });
        }

        let mut double_clicked = false;
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(BG_VIEWER))
            .show(ctx, |ui| {
                double_clicked = self.view_area(ui, zoom_cmd);
            });

        if double_clicked {
            close = true;
        }
        if let Some(permanent) = delete {
            self.request_delete_current_image(permanent);
        }
        if toggle_fullscreen {
            self.set_fullscreen(ctx, !fullscreen);
        }
        if exit_fullscreen {
            self.set_fullscreen(ctx, false);
        }
        if close {
            self.close_viewer();
            return;
        }
        if let Some(t) = target {
            self.go_to_image(t);
        }
    }

    /// 이미지를 그리고 마우스 입력을 처리한다.
    /// 휠 = 확대/축소(커서 위치 기준), 왼쪽 버튼 끌기 = 보이는 영역 이동.
    /// 목록으로 돌아가야 하면(더블클릭) true.
    fn view_area(&mut self, ui: &mut egui::Ui, zoom_cmd: Option<ZoomCmd>) -> bool {
        let size = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let ppp = ui.ctx().pixels_per_point();
        let (scroll_y, zoom_delta, ctrl, pointer) = ui.input(|i| {
            (
                i.raw_scroll_delta.y,
                i.zoom_delta(),
                i.modifiers.ctrl || i.modifiers.command,
                i.pointer.hover_pos(),
            )
        });
        let now = self.now;
        let Some(view) = self.view.as_mut() else {
            return false;
        };
        let index = view.index;

        if let Some(full) = self.full_cache.get(&index) {
            let orig = vec2(
                full.orig_size[0].max(1) as f32,
                full.orig_size[1].max(1) as f32,
            );
            // "화면 맞춤" 배율: 창보다 큰 이미지만 줄이고, 작은 이미지는 원래 크기로.
            let fit = (rect.width() * ppp / orig.x)
                .min(rect.height() * ppp / orig.y)
                .min(1.0);

            match zoom_cmd {
                Some(ZoomCmd::Fit) => {
                    view.zoom = None;
                    view.offset = Vec2::ZERO;
                }
                Some(ZoomCmd::Actual) => view.set_zoom(fit, 1.0, Vec2::ZERO),
                Some(ZoomCmd::By(factor)) => view.zoom_by(fit, factor, Vec2::ZERO),
                None => {}
            }

            if response.hovered() {
                // Ctrl+휠은 egui가 이미 zoom_delta로 바꿔 주므로 휠 값을 또 더하지 않는다.
                let factor = if ctrl {
                    zoom_delta
                } else {
                    zoom_delta * (scroll_y * WHEEL_ZOOM_SPEED).exp()
                };
                if (factor - 1.0).abs() > 1e-4 {
                    let anchor = pointer.map_or(Vec2::ZERO, |p| p - rect.center());
                    view.zoom_by(fit, factor, anchor);
                }
            }
            if response.dragged_by(egui::PointerButton::Primary) {
                view.offset += response.drag_delta();
            }

            let scale = view.zoom.unwrap_or(fit);
            let shown = orig * (scale / ppp);
            view.offset = clamp_offset(view.offset, shown, rect.size());
            view.display_scale = scale;
            let image_rect = Rect::from_center_size(rect.center() + view.offset, shown);

            let (frame, wait) = full.frame_at(now - view.anim_start);
            painter.image(
                full.frames[frame].texture.id(),
                image_rect,
                uv_full(),
                Color32::WHITE,
            );
            if wait.is_finite() {
                ui.ctx()
                    .request_repaint_after(Duration::from_secs_f64(wait.max(0.01)));
            }

            let pannable = shown.x > rect.width() + 0.5 || shown.y > rect.height() + 0.5;
            if pannable {
                if response.dragged() {
                    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                } else if response.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::Grab);
                }
            }
        } else if let Some(error) = self.full_errors.get(&index) {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                format!("이미지를 열 수 없습니다\n{error}"),
                FontId::proportional(16.0),
                ERROR_COLOR,
            );
        } else {
            // 원본을 읽는 동안 썸네일을 흐릿하게 키워서 보여준다.
            if let Some(entry) = self.images.get(index) {
                if let ThumbState::Ready(texture) = &entry.thumb {
                    let orig = entry
                        .dims
                        .map_or(texture.size_vec2(), |[w, h]| vec2(w as f32, h as f32));
                    let fit = (rect.width() * ppp / orig.x.max(1.0))
                        .min(rect.height() * ppp / orig.y.max(1.0))
                        .min(1.0);
                    let shown = orig * (fit / ppp);
                    painter.image(
                        texture.id(),
                        Rect::from_center_size(rect.center(), shown),
                        uv_full(),
                        Color32::from_gray(170),
                    );
                }
            }
            painter.text(
                rect.center_bottom() - vec2(0.0, 28.0),
                Align2::CENTER_CENTER,
                "불러오는 중…",
                FontId::proportional(14.0),
                Color32::from_gray(210),
            );
        }

        // 연 직후의 더블클릭은 썸네일을 더블클릭한 두 번째 클릭이므로 무시
        response.double_clicked() && now - view.opened_at > 0.5
    }
}

impl eframe::App for ImgViewApp {
    /// 프로그램을 끌 때와 30초마다 eframe이 불러서 설정 파일에 저장한다.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings());
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // 지난번 창 위치를 정확히 복원하고, 화면 영역 밖이면(모니터를 뺀 경우 등) 가운데로
        if let Some(saved) = self.startup_window.take() {
            placement::restore_window_position(frame, &saved);
        }
        self.now = ctx.input(|i| i.time);
        // 전체 화면으로 끝냈다면 창이 전체 화면으로 복원되므로 실제 상태에 맞춘다.
        if let Some(fullscreen) = ctx.input(|i| i.viewport().fullscreen) {
            self.fullscreen = fullscreen;
        }
        self.max_texture_side =
            (ctx.input(|i| i.max_texture_side) as u32).clamp(512, MAX_FULL_TEXTURE_SIDE);

        self.process_results();
        self.poll_copy_jobs();
        self.poll_delete_jobs();
        self.handle_dropped_files(ctx);
        if ctx.input(|i| i.key_pressed(Key::F11)) {
            self.set_fullscreen(ctx, !self.fullscreen);
        }
        if let Some((_, shown_at)) = &self.info {
            let age = self.now - shown_at;
            if age > INFO_SECONDS {
                self.info = None;
            } else {
                ctx.request_repaint_after(Duration::from_secs_f64(INFO_SECONDS - age + 0.05));
            }
        }

        if self.view.is_some() {
            self.ui_viewer(ctx);
        } else {
            self.ui_browser(ctx);
        }
        self.paint_drop_overlay(ctx);
        self.update_title(ctx);

        if let Some(drag) = self.drag_out.take() {
            self.run_drag_out(frame, drag);
        }
        if let Some(request) = self.pending_delete.take() {
            self.run_delete(ctx, frame, request);
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if std::mem::take(&mut self.release_pointer) {
            let pos = ctx.input(|i| i.pointer.latest_pos()).unwrap_or_default();
            raw_input.events.push(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: raw_input.modifiers,
            });
        }
    }
}

// ───────────────────────── 보조 함수 ─────────────────────────

/// 폴더를 읽어 하위 폴더와 이미지 파일 목록을 이름순으로 돌려준다.
fn read_folder(path: &Path) -> Result<(Vec<FolderEntry>, Vec<ImageEntry>), String> {
    let read_dir = std::fs::read_dir(path)
        .map_err(|e| format!("폴더를 열 수 없습니다: {} ({e})", display_path(path)))?;
    let mut folders = Vec::new();
    let mut images = Vec::new();
    for entry in read_dir.flatten() {
        let entry_path = entry.path();
        let raw_name = entry.file_name().to_string_lossy().into_owned();
        let Ok(meta) = std::fs::metadata(&entry_path) else {
            continue;
        };
        if is_hidden(&meta, &raw_name) {
            continue;
        }
        // macOS/Windows에서 만든 이름(NFD)은 화면에서 자모가 분리되므로 NFC로 합쳐서 보여준다.
        let name = nfc(&raw_name);
        if meta.is_dir() {
            folders.push(FolderEntry {
                path: entry_path,
                name,
                modified: meta.modified().ok(),
                created: meta.created().ok(),
            });
        } else if meta.is_file() && is_supported_image(&entry_path) {
            images.push(ImageEntry {
                path: entry_path,
                name,
                file_size: meta.len(),
                modified: meta.modified().ok(),
                created: meta.created().ok(),
                thumb: ThumbState::NotRequested,
                dims: None,
                preview: Vec::new(),
            });
        }
    }
    // 정렬은 호출한 쪽에서 현재 정렬 기준으로 한다.
    Ok((folders, images))
}

/// Shift+Del 확인 창 (탐색기처럼 되돌릴 수 없다는 경고). "예"를 누르면 true.
fn confirm_permanent_delete(frame: &eframe::Frame, paths: &[PathBuf]) -> bool {
    let folders = paths.iter().filter(|p| p.is_dir()).count();
    let files = paths.len() - folders;
    let name = |p: &PathBuf| {
        p.file_name()
            .map_or_else(|| display_path(p), |n| nfc(&n.to_string_lossy()))
    };
    let what = match (files, folders) {
        (1, 0) => format!("'{}' 파일을", name(&paths[0])),
        (0, 1) => format!("'{}' 폴더와 그 안의 모든 내용을", name(&paths[0])),
        _ => {
            let mut parts = Vec::new();
            if files > 0 {
                parts.push(format!("파일 {files}개"));
            }
            if folders > 0 {
                parts.push(format!("폴더 {folders}개(안의 내용 포함)"));
            }
            format!("{}를", parts.join(", "))
        }
    };
    let answer = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title("영구 삭제")
        .set_description(format!(
            "{what} 영구적으로 삭제하시겠습니까?\n\n휴지통으로 이동하지 않으며 되돌릴 수 없습니다."
        ))
        .set_buttons(rfd::MessageButtons::YesNo)
        .set_parent(frame)
        .show();
    answer == rfd::MessageDialogResult::Yes
}

fn directed(ord: Ordering, descending: bool) -> Ordering {
    if descending {
        ord.reverse()
    } else {
        ord
    }
}

/// 같은 값이면 이름순으로 정한다 (탐색기와 같음).
fn compare_folders(a: &FolderEntry, b: &FolderEntry, key: SortKey) -> Ordering {
    let primary = match key {
        SortKey::DateModified => a.modified.cmp(&b.modified),
        SortKey::DateCreated => a.created.cmp(&b.created),
        // 폴더는 종류와 크기가 모두 같으므로 이름순
        SortKey::Name | SortKey::Type | SortKey::Size => Ordering::Equal,
    };
    primary.then_with(|| natural_cmp(&a.name, &b.name))
}

fn compare_images(a: &ImageEntry, b: &ImageEntry, key: SortKey) -> Ordering {
    let primary = match key {
        SortKey::Name => Ordering::Equal,
        SortKey::DateModified => a.modified.cmp(&b.modified),
        SortKey::DateCreated => a.created.cmp(&b.created),
        SortKey::Type => file_type(&a.path).cmp(&file_type(&b.path)),
        SortKey::Size => a.file_size.cmp(&b.file_size),
    };
    primary.then_with(|| natural_cmp(&a.name, &b.name))
}

/// 종류 = 확장자 (소문자)
fn file_type(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn setup_style(ctx: &egui::Context, font_size: f32) {
    setup_fonts(ctx);
    ctx.set_theme(egui::Theme::Dark);
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(38, 38, 42);
    ctx.set_visuals(visuals);
    apply_font_size(ctx, font_size);
}

fn clamp_font_size(font_size: f32) -> f32 {
    if font_size.is_finite() {
        font_size.clamp(FONT_MIN, FONT_MAX)
    } else {
        FONT_DEFAULT
    }
}

/// 본문 글꼴 크기를 기준으로 나머지 글자 크기를 같은 비율로 맞춘다.
fn apply_font_size(ctx: &egui::Context, font_size: f32) {
    let size = clamp_font_size(font_size);
    let proportional = |s: f32| FontId::new(s, egui::FontFamily::Proportional);
    ctx.style_mut(|style| {
        style.text_styles = [
            (egui::TextStyle::Small, proportional(size * 0.72)),
            (egui::TextStyle::Body, proportional(size)),
            (egui::TextStyle::Button, proportional(size)),
            (egui::TextStyle::Heading, proportional(size * 1.44)),
            (
                egui::TextStyle::Monospace,
                FontId::new(size * 0.96, egui::FontFamily::Monospace),
            ),
        ]
        .into();
    });
}

/// egui 기본 글꼴에는 한글이 없으므로 시스템 한글 글꼴을 보조 글꼴로 추가한다.
/// 글꼴을 못 찾으면 한글이 네모(토푸)로 보이므로, 마지막에는 fontconfig에게 물어본다.
fn setup_fonts(ctx: &egui::Context) {
    let Some(bytes) = find_korean_font() else {
        eprintln!("한글 글꼴을 찾지 못했습니다. 한글이 네모로 보일 수 있습니다.");
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("korean".to_owned(), egui::FontData::from_owned(bytes).into());
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("korean".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// 한글을 그릴 수 있는 글꼴 파일을 찾는다 (플랫폼별 알려진 경로 → fontconfig 순).
fn find_korean_font() -> Option<Vec<u8>> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(windir) = std::env::var_os("WINDIR") {
        let fonts = PathBuf::from(windir).join("Fonts");
        candidates.push(fonts.join("malgun.ttf"));
        candidates.push(fonts.join("NanumGothic.ttf"));
        candidates.push(fonts.join("gulim.ttc"));
    }
    candidates.push(PathBuf::from(r"C:\Windows\Fonts\malgun.ttf"));
    candidates.push(PathBuf::from("/System/Library/Fonts/AppleSDGothicNeo.ttc"));
    candidates.push(PathBuf::from("/usr/share/fonts/truetype/nanum/NanumGothic.ttf"));
    candidates.push(PathBuf::from(
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    ));
    candidates.push(PathBuf::from(
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    ));

    candidates
        .iter()
        .find_map(|p| std::fs::read(p).ok())
        .or_else(|| {
            // 배포판마다 경로가 달라서 fontconfig에 마지막으로 물어본다.
            let out = std::process::Command::new("fc-match")
                .args(["-f", "%{file}", ":lang=ko"])
                .output()
                .ok()?;
            let path = String::from_utf8(out.stdout).ok()?;
            std::fs::read(path.trim()).ok()
        })
}

/// 화면에 그릴 문자열을 NFC로 합친다.
/// macOS/Windows에서 만든 이름은 자모가 분리된 NFD라서 그대로 그리면 "ㅅㅏㅈㅣᆫ"처럼 깨져 보인다.
pub(crate) fn nfc(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    if text.is_ascii() {
        return text.to_owned();
    }
    text.nfc().collect()
}

/// 경로를 화면에 보여줄 문자열로 만든다 (NFC).
fn display_path(path: &Path) -> String {
    nfc(&path.display().to_string())
}

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn uv_full() -> Rect {
    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))
}

/// `size` 크기의 그림을 `area` 가운데에 비율을 유지하며 맞춘다 (`max_scale` 이상으로 키우지 않음).
fn fit_rect(area: Rect, size: Vec2, max_scale: f32) -> Rect {
    if size.x <= 0.0 || size.y <= 0.0 {
        return area;
    }
    let scale = (area.width() / size.x)
        .min(area.height() / size.y)
        .min(max_scale);
    Rect::from_center_size(area.center(), size * scale)
}

/// 이미지가 화면보다 작으면 가운데 고정, 크면 가장자리가 화면 안쪽으로 들어오지 않게 제한.
fn clamp_offset(offset: Vec2, image: Vec2, area: Vec2) -> Vec2 {
    let max = ((image - area) * 0.5).max(Vec2::ZERO);
    vec2(offset.x.clamp(-max.x, max.x), offset.y.clamp(-max.y, max.y))
}

fn paint_folder_icon(painter: &egui::Painter, area: Rect) {
    let s = area.width().min(area.height()) * 0.55;
    let body = Rect::from_center_size(area.center() + vec2(0.0, s * 0.06), vec2(s, s * 0.72));
    let tab = Rect::from_min_size(body.min - vec2(0.0, s * 0.12), vec2(s * 0.42, s * 0.2));
    painter.rect_filled(tab, 3.0, FOLDER_COLOR.gamma_multiply(0.8));
    painter.rect_filled(body, 4.0, FOLDER_COLOR);
}

/// 한 줄로, 넘치면 "…"로 자른 파일 이름을 가운데 정렬해서 그린다.
fn paint_label(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: Rect,
    text: &str,
    color: Color32,
    font_size: f32,
) {
    let mut job = egui::text::LayoutJob::simple_singleline(
        text.to_owned(),
        FontId::proportional(font_size),
        color,
    );
    job.wrap = egui::text::TextWrapping {
        max_width: rect.width(),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
        ..Default::default()
    };
    let galley = ui.fonts(|f| f.layout_job(job));
    let pos = pos2(
        rect.center().x - galley.size().x / 2.0,
        rect.center().y - galley.size().y / 2.0,
    );
    painter.galley(pos, galley, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(name: &str, size: u64, modified: u64, created: u64) -> ImageEntry {
        ImageEntry {
            path: PathBuf::from(name),
            name: name.to_owned(),
            file_size: size,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(modified)),
            created: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(created)),
            thumb: ThumbState::NotRequested,
            dims: None,
            preview: Vec::new(),
        }
    }

    fn sorted_names(key: SortKey, descending: bool) -> Vec<String> {
        let mut images = vec![
            image("10.jpg", 300, 20, 3),
            image("2.png", 100, 30, 1),
            image("b.gif", 200, 10, 2),
            image("a.jpg", 200, 40, 4),
        ];
        images.sort_by(|a, b| directed(compare_images(a, b, key), descending));
        images.into_iter().map(|e| e.name).collect()
    }

    /// 실제 설정 파일 대신 쓰는 메모리 저장소
    #[derive(Default)]
    struct MemoryStorage(HashMap<String, String>);

    impl eframe::Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }
        fn flush(&mut self) {}
    }

    #[test]
    fn settings_round_trip_through_storage() {
        let mut storage = MemoryStorage::default();
        let settings = Settings {
            last_folder: Some(PathBuf::from(r"D:\사진\여행 2026")),
            sort_key: SortKey::DateModified,
            sort_descending: true,
            thumb_size: 200.0,
            show_tree: false,
            font_size: 16.0,
        };
        eframe::set_value(&mut storage, SETTINGS_KEY, &settings);
        let loaded: Settings = eframe::get_value(&storage, SETTINGS_KEY).unwrap();
        assert_eq!(loaded, settings);
    }

    #[test]
    fn missing_settings_fields_use_defaults() {
        let mut storage = MemoryStorage::default();
        eframe::Storage::set_string(&mut storage, SETTINGS_KEY, "(sort_key:Size)".to_owned());
        let loaded: Settings = eframe::get_value(&storage, SETTINGS_KEY).unwrap();
        assert_eq!(loaded.sort_key, SortKey::Size);
        assert_eq!(loaded.last_folder, None);
        assert_eq!(loaded.thumb_size, 150.0);
        assert!(loaded.show_tree);
    }

    #[test]
    fn sort_by_each_key_like_explorer() {
        assert_eq!(sorted_names(SortKey::Name, false), ["2.png", "10.jpg", "a.jpg", "b.gif"]);
        assert_eq!(sorted_names(SortKey::Name, true), ["b.gif", "a.jpg", "10.jpg", "2.png"]);
        assert_eq!(
            sorted_names(SortKey::DateModified, false),
            ["b.gif", "10.jpg", "2.png", "a.jpg"]
        );
        // 최신 파일이 먼저
        assert_eq!(
            sorted_names(SortKey::DateModified, true),
            ["a.jpg", "2.png", "10.jpg", "b.gif"]
        );
        assert_eq!(
            sorted_names(SortKey::DateCreated, false),
            ["2.png", "b.gif", "10.jpg", "a.jpg"]
        );
        // 같은 크기(200)는 이름순
        assert_eq!(sorted_names(SortKey::Size, false), ["2.png", "a.jpg", "b.gif", "10.jpg"]);
        assert_eq!(sorted_names(SortKey::Size, true), ["10.jpg", "b.gif", "a.jpg", "2.png"]);
        // 종류(확장자)별로 묶고 그 안에서는 이름순
        assert_eq!(sorted_names(SortKey::Type, false), ["b.gif", "10.jpg", "a.jpg", "2.png"]);
    }

    #[test]
    fn extensions_are_case_insensitive() {
        assert!(is_supported_image(Path::new("photo.JPG")));
        assert!(is_supported_image(Path::new("anim.Gif")));
        assert!(is_supported_image(Path::new("scan.tiff")));
        assert!(!is_supported_image(Path::new("notes.txt")));
        assert!(!is_supported_image(Path::new("no_extension")));
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn zoom_keeps_point_under_cursor_fixed() {
        let mut view = ViewState::new(0, 0.0);
        // 배율 1.0에서 커서(중앙 기준 +100,+50) 아래 점을 고정한 채 2배 확대
        view.set_zoom(1.0, 1.0, Vec2::ZERO);
        let anchor = vec2(100.0, 50.0);
        let image_point_before = (anchor - view.offset) / 1.0;
        view.zoom_by(1.0, 2.0, anchor);
        let image_point_after = (anchor - view.offset) / 2.0;
        assert!((image_point_before - image_point_after).length() < 1e-4);
    }

    #[test]
    fn small_images_stay_centered() {
        let offset = clamp_offset(vec2(300.0, -40.0), vec2(200.0, 100.0), vec2(800.0, 600.0));
        assert_eq!(offset, Vec2::ZERO);
        let offset = clamp_offset(vec2(300.0, 0.0), vec2(1000.0, 100.0), vec2(800.0, 600.0));
        assert_eq!(offset, vec2(100.0, 0.0));
    }

    #[test]
    fn decomposed_korean_names_are_shown_composed() {
        use unicode_normalization::UnicodeNormalization as _;
        let composed = "사진모음";
        let decomposed: String = composed.nfd().collect();
        // macOS/Windows에서 만든 이름은 자모가 분리되어 있다.
        assert_ne!(decomposed, composed);
        assert_eq!(nfc(&decomposed), composed);
        assert_eq!(display_path(Path::new(&decomposed)), composed);
        // ASCII는 그대로 둔다 (경로 비교가 흔들리지 않게).
        assert_eq!(nfc("photo.png"), "photo.png");
    }

    #[test]
    fn font_size_is_clamped_to_the_slider_range() {
        assert_eq!(clamp_font_size(FONT_DEFAULT), FONT_DEFAULT);
        assert_eq!(clamp_font_size(1.0), FONT_MIN);
        assert_eq!(clamp_font_size(500.0), FONT_MAX);
        assert_eq!(clamp_font_size(f32::NAN), FONT_DEFAULT);
    }
}
