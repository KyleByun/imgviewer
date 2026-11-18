//! Linux(X11): 앱에서 다른 프로그램(파일 관리자, 터미널)으로 끌어서 복사 — XDND의 "보내는 쪽".
//!
//! winit은 끌어서 받기만 지원하므로 보내는 쪽은 직접 구현한다.
//! - 앱 창과 따로 X 서버에 연결하고, 보이지 않는 창을 하나 만들어 끌기의 출처(source)로 쓴다.
//!   대상이 보내는 XdndStatus/XdndFinished와 데이터 요청은 이 창으로 오므로 winit의 연결과 섞이지 않는다.
//! - 마우스 버튼을 누르고 있는 동안은 winit이 포인터를 잡고(implicit grab) 있어서 다시 잡을 수 없다.
//!   그래서 포인터 위치와 버튼 상태를 짧은 간격으로 물어보며(QueryPointer) 끌기를 진행한다.
//! - Wayland 세션(XWayland)에서는 Mutter 같은 컴포지터가 Wayland 창 위에 대신 받아 주는 X 창을 띄우므로
//!   Nautilus나 터미널 같은 Wayland 앱에도 똑같이 놓을 수 있다.
//!
//! - 끄는 동안의 커서는 보내는 쪽이 정한다. 버튼을 누른 채 끄는 동안 X 서버는 화면 어디서든
//!   포인터를 잡은 창(앱 창)의 커서를 보여주므로, 앱 창의 커서를 "복사"/"놓을 수 없음" 모양으로 바꾼다.
//!
//! 넘기는 형식: `text/uri-list` (파일 관리자, 터미널), 일반 텍스트(경로).

use std::cell::Cell;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::errors::ReplyOrIdError;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _, CreateWindowAux,
    Cursor, EventMask, KeyButMask, PropMode, SelectionNotifyEvent, SelectionRequestEvent, Window,
    WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE};

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        XdndAware,
        XdndProxy,
        XdndSelection,
        XdndEnter,
        XdndPosition,
        XdndStatus,
        XdndLeave,
        XdndDrop,
        XdndFinished,
        XdndTypeList,
        XdndActionCopy,
        TARGETS,
        UTF8_STRING,
        _NET_WM_PID,
        TEXT_URI_LIST: b"text/uri-list",
        TEXT_PLAIN_UTF8: b"text/plain;charset=utf-8",
    }
}

/// 우리가 지원하는 XDND 버전 (2002년 이후 바뀌지 않음)
const XDND_VERSION: u32 = 5;
/// 포인터 위치를 물어보는 간격
const POLL_INTERVAL: Duration = Duration::from_millis(8);
/// 대상이 XdndStatus로 답하지 않을 때 다음 XdndPosition을 보내기까지 기다리는 시간
const STATUS_TIMEOUT: Duration = Duration::from_millis(500);
/// 포인터가 멈춰 있어도 이 간격으로 위치를 다시 보내서 대상의 "받을 수 있음" 답을 새로 받는다
const POSITION_REFRESH: Duration = Duration::from_millis(100);
/// 놓은 뒤 대상이 데이터를 가져가고 XdndFinished를 보낼 때까지 기다리는 최대 시간
const FINISH_TIMEOUT: Duration = Duration::from_secs(10);

/// 끌기가 진행 중인지. 버튼을 누르고 있는 동안 매 프레임 다시 시작하지 않게 한다.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// `IMGVIEW_XDND_DEBUG=1`로 실행하면 끌기 과정을 터미널(stderr)에 찍는다.
fn debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("IMGVIEW_XDND_DEBUG").is_some())
}

macro_rules! debug {
    ($($arg:tt)*) => {
        if debug_enabled() {
            eprintln!("[xdnd] {}", format_args!($($arg)*));
        }
    };
}

/// `paths`를 끌어서 복사하기 시작한다. 실제 끌기는 백그라운드 스레드에서 버튼을 놓을 때까지 진행된다.
/// `app_window`는 앱 창의 X11 창 번호로, 끄는 동안 이 창의 커서를 바꾼다 (없으면 커서는 그대로).
/// 이미 끌고 있는 중이면 아무것도 하지 않는다.
pub fn start_drag(paths: Vec<PathBuf>, app_window: Option<u32>) -> Result<(), String> {
    if paths.is_empty() || ACTIVE.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let session = match DragSession::new(&paths, app_window) {
        Ok(session) => session,
        Err(e) => {
            ACTIVE.store(false, Ordering::SeqCst);
            return Err(e);
        }
    };
    let spawned = thread::Builder::new()
        .name("imgview-xdnd".to_owned())
        .spawn(move || {
            if let Err(e) = session.run() {
                eprintln!("끌어서 복사 중 X11 오류: {e}");
            }
            ACTIVE.store(false, Ordering::SeqCst);
        });
    if let Err(e) = spawned {
        ACTIVE.store(false, Ordering::SeqCst);
        return Err(e.to_string());
    }
    Ok(())
}

/// 끌어서 놓을 대상 창
#[derive(Clone, Copy, PartialEq, Eq)]
struct Target {
    /// 포인터 아래에 있는, XdndAware가 붙은 창 (메시지의 window 칸에 넣는 값)
    window: Window,
    /// 메시지를 실제로 보낼 창 (XdndProxy가 있으면 그 창, 아니면 `window`)
    send_to: Window,
    version: u32,
}

/// 대상이 보낸 답을 모아 두는 상태
#[derive(Default)]
struct Replies {
    /// 마지막 XdndPosition에 대한 답을 기다리는 중이면 보낸 시각
    status_pending: Option<Instant>,
    accepted: bool,
    /// 대상이 고른 동작 (복사여야 한다). 디버그 로그에서 바뀔 때만 찍으려고 기억한다.
    action: u32,
    finished: bool,
}

/// 끄는 동안 앱 창에 씌우는 커서들 (현재 커서 테마에서 불러옴)
struct Cursors {
    copy: Cursor,
    no_drop: Cursor,
    /// 끌기가 끝나면 되돌릴 기본 커서 (winit이 쓰는 것과 같은 "default")
    default: Cursor,
}

struct DragSession {
    conn: RustConnection,
    atoms: Atoms,
    root: Window,
    /// 끌기의 출처로 쓰는 보이지 않는 창
    source: Window,
    time: u32,
    types: Vec<u32>,
    uri_list: Vec<u8>,
    plain_text: Vec<u8>,
    /// 커서를 바꿀 앱 창
    app_window: Option<Window>,
    cursors: Option<Cursors>,
    /// 앱 창의 커서를 바꿔 놓았는지 (끝날 때 되돌려야 하는지)
    cursor_changed: Cell<bool>,
}

impl DragSession {
    fn new(paths: &[PathBuf], app_window: Option<Window>) -> Result<Self, String> {
        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|e| format!("X11 화면에 연결할 수 없습니다 ({e})"))?;
        let root = conn.setup().roots[screen_num].root;
        let (atoms, source, time, types) = prepare_source(&conn, root)
            .map_err(|e| format!("끌기를 준비하지 못했습니다 ({e})"))?;
        let cursors = if app_window.is_some() {
            load_cursors(&conn, screen_num)
        } else {
            None
        };
        if cursors.is_none() {
            debug!("커서를 바꾸지 않음 (앱 창 번호나 커서 테마를 얻지 못함)");
        }
        Ok(Self {
            conn,
            atoms,
            root,
            source,
            time,
            types,
            uri_list: uri_list(paths),
            plain_text: plain_text(paths),
            app_window,
            cursors,
            cursor_changed: Cell::new(false),
        })
    }

    fn run(self) -> Result<(), ReplyOrIdError> {
        let result = self.drag();
        // 오류로 끝났어도 앱 창의 커서는 원래대로 돌려놓는다.
        let _ = self.restore_cursor();
        result
    }

    fn drag(&self) -> Result<(), ReplyOrIdError> {
        // 끌기가 끝난 직후 한 프레임 늦게 불린 경우: 버튼이 이미 떨어졌으면 아무것도 하지 않는다.
        if !self.button_down()? {
            debug!("버튼이 이미 떨어져 있어서 끌기를 시작하지 않음");
            return Ok(());
        }
        debug!("끌기 시작: source=0x{:x} time={}", self.source, self.time);
        self.conn
            .set_selection_owner(self.source, self.atoms.XdndSelection, self.time)?;
        self.conn.flush()?;

        let mut replies = Replies::default();
        let mut target: Option<Target> = None;
        // 마지막으로 대상 창을 찾은 위치와 시각
        let mut last_find: Option<((i16, i16), Instant)> = None;
        let mut sent_pos: Option<((i16, i16), Instant)> = None;
        // 앱 창에 마지막으로 씌운 커서와 시각
        let mut shown_cursor: Option<(Cursor, Instant)> = None;

        loop {
            self.pump_events(target, &mut replies)?;
            let pointer = self.conn.query_pointer(self.root)?.reply()?;
            if !pointer.mask.contains(KeyButMask::BUTTON1) {
                break;
            }
            let pos = (pointer.root_x, pointer.root_y);

            // 포인터가 움직였을 때, 그리고 멈춰 있어도 가끔 다시 찾는다.
            // (Wayland 창 위에서는 Mutter가 대신 받아 주는 X 창을 조금 늦게 띄우기 때문)
            let find_due = match last_find {
                Some((p, at)) => p != pos || at.elapsed() >= POSITION_REFRESH,
                None => true,
            };
            if find_due {
                let found = self.find_target()?;
                if found != target {
                    if let Some(old) = target {
                        self.send(old, self.atoms.XdndLeave, [self.source, 0, 0, 0, 0])?;
                    }
                    replies = Replies::default();
                    sent_pos = None;
                    match found {
                        Some(t) => debug!(
                            "대상 바뀜 ({}, {}): window=0x{:x} send_to=0x{:x} version={}",
                            pos.0, pos.1, t.window, t.send_to, t.version
                        ),
                        None => debug!("대상 없음 ({}, {})", pos.0, pos.1),
                    }
                    if let Some(new) = found {
                        let more_than_three = u32::from(self.types.len() > 3);
                        let mut data = [self.source, (new.version << 24) | more_than_three, 0, 0, 0];
                        for (slot, ty) in data[2..].iter_mut().zip(&self.types) {
                            *slot = *ty;
                        }
                        self.send(new, self.atoms.XdndEnter, data)?;
                    }
                    target = found;
                }
                last_find = Some((pos, Instant::now()));
            }

            if let Some(t) = target {
                let waiting = replies
                    .status_pending
                    .is_some_and(|sent| sent.elapsed() < STATUS_TIMEOUT);
                let due = match sent_pos {
                    Some((p, at)) => p != pos || at.elapsed() >= POSITION_REFRESH,
                    None => true,
                };
                if !waiting && due {
                    let packed = ((pos.0 as u16 as u32) << 16) | pos.1 as u16 as u32;
                    self.send(
                        t,
                        self.atoms.XdndPosition,
                        [self.source, 0, packed, self.time, self.atoms.XdndActionCopy],
                    )?;
                    replies.status_pending = Some(Instant::now());
                    sent_pos = Some((pos, Instant::now()));
                }
            }

            self.update_cursor(target.is_some() && replies.accepted, &mut shown_cursor)?;
            self.conn.flush()?;
            thread::sleep(POLL_INTERVAL);
        }

        // 버튼을 놓았다. 대상이 데이터를 가져가는 동안 기다리기 전에 커서부터 되돌린다.
        self.restore_cursor()?;
        let Some(t) = target else {
            debug!("대상 없는 곳에서 놓음 → 취소");
            return Ok(());
        };
        // 마지막 위치에 대한 답이 아직 안 왔으면 잠깐 기다린다.
        let deadline = Instant::now() + STATUS_TIMEOUT;
        while replies.status_pending.is_some() && Instant::now() < deadline {
            self.pump_events(target, &mut replies)?;
            thread::sleep(POLL_INTERVAL);
        }
        if !replies.accepted {
            debug!("대상이 받지 않겠다고 해서 XdndLeave 보냄");
            self.send(t, self.atoms.XdndLeave, [self.source, 0, 0, 0, 0])?;
            self.conn.flush()?;
            return Ok(());
        }
        debug!("XdndDrop 보냄 → window=0x{:x}", t.window);
        self.send(t, self.atoms.XdndDrop, [self.source, 0, self.time, 0, 0])?;
        self.conn.flush()?;

        // 대상이 데이터를 가져가는 동안 요청에 답한다.
        let deadline = Instant::now() + FINISH_TIMEOUT;
        while !replies.finished && Instant::now() < deadline {
            self.pump_events(target, &mut replies)?;
            thread::sleep(POLL_INTERVAL);
        }
        if !replies.finished {
            debug!("XdndFinished를 {}초 동안 받지 못함", FINISH_TIMEOUT.as_secs());
        }
        Ok(())
    }

    /// 놓을 수 있으면 "복사", 아니면 "놓을 수 없음" 커서를 앱 창에 씌운다.
    /// 포인터가 앱 창에 다시 들어오면 egui가 커서를 기본으로 덮어쓰므로, 같은 커서라도 가끔 다시 씌운다.
    fn update_cursor(
        &self,
        can_drop: bool,
        shown: &mut Option<(Cursor, Instant)>,
    ) -> Result<(), ReplyOrIdError> {
        let (Some(window), Some(cursors)) = (self.app_window, &self.cursors) else {
            return Ok(());
        };
        let want = if can_drop {
            cursors.copy
        } else {
            cursors.no_drop
        };
        let due = match *shown {
            Some((cursor, at)) => cursor != want || at.elapsed() >= POSITION_REFRESH,
            None => true,
        };
        if due {
            self.conn.change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new().cursor(want),
            )?;
            self.cursor_changed.set(true);
            *shown = Some((want, Instant::now()));
        }
        Ok(())
    }

    /// 바꿔 놓은 앱 창의 커서를 기본 커서로 되돌린다.
    fn restore_cursor(&self) -> Result<(), ReplyOrIdError> {
        if !self.cursor_changed.replace(false) {
            return Ok(());
        }
        if let (Some(window), Some(cursors)) = (self.app_window, &self.cursors) {
            self.conn.change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new().cursor(cursors.default),
            )?;
            self.conn.flush()?;
        }
        Ok(())
    }

    fn button_down(&self) -> Result<bool, ReplyOrIdError> {
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        Ok(pointer.mask.contains(KeyButMask::BUTTON1))
    }

    /// 쌓인 이벤트를 모두 처리한다: 대상의 답(XdndStatus/XdndFinished)과 데이터 요청.
    fn pump_events(
        &self,
        target: Option<Target>,
        replies: &mut Replies,
    ) -> Result<(), ReplyOrIdError> {
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                Event::ClientMessage(msg) => {
                    let data = msg.data.as_data32();
                    let from_target = target.is_some_and(|t| data[0] == t.window);
                    if !from_target {
                        continue;
                    }
                    if msg.type_ == self.atoms.XdndStatus {
                        replies.status_pending = None;
                        let accepted = data[1] & 1 == 1;
                        if accepted != replies.accepted || data[4] != replies.action {
                            debug!(
                                "XdndStatus: accept={accepted} action={}",
                                self.atom_name(data[4])
                            );
                        }
                        replies.accepted = accepted;
                        replies.action = data[4];
                    } else if msg.type_ == self.atoms.XdndFinished {
                        debug!(
                            "XdndFinished: success={} action={}",
                            data[1] & 1 == 1,
                            self.atom_name(data[2])
                        );
                        replies.finished = true;
                    }
                }
                Event::SelectionRequest(request) => {
                    debug!(
                        "데이터 요청: target={} requestor=0x{:x}",
                        self.atom_name(request.target),
                        request.requestor
                    );
                    self.answer(request)?;
                }
                _ => {}
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    /// 대상이 놓인 데이터를 달라고 하면 요청한 형식으로 넘겨준다.
    fn answer(&self, request: SelectionRequestEvent) -> Result<(), ReplyOrIdError> {
        let property = if request.property == NONE {
            request.target // 오래된 클라이언트는 속성을 비워 보낸다
        } else {
            request.property
        };
        let a = &self.atoms;
        let data = if request.selection != a.XdndSelection {
            None
        } else if request.target == a.TEXT_URI_LIST {
            Some(&self.uri_list)
        } else if request.target == a.TEXT_PLAIN_UTF8 || request.target == a.UTF8_STRING {
            Some(&self.plain_text)
        } else {
            None
        };
        let answered = if request.selection == a.XdndSelection && request.target == a.TARGETS {
            let mut targets = self.types.clone();
            targets.push(a.TARGETS);
            self.conn.change_property32(
                PropMode::REPLACE,
                request.requestor,
                property,
                AtomEnum::ATOM,
                &targets,
            )?;
            true
        } else if let Some(bytes) = data {
            self.conn.change_property8(
                PropMode::REPLACE,
                request.requestor,
                property,
                request.target,
                bytes,
            )?;
            true
        } else {
            false
        };
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property: if answered { property } else { NONE },
        };
        self.conn
            .send_event(false, request.requestor, EventMask::NO_EVENT, notify)?;
        Ok(())
    }

    /// 포인터 아래에서 XdndAware가 붙은 창을 찾는다. 이 앱 자신의 창이면 대상이 없는 것으로 본다.
    fn find_target(&self) -> Result<Option<Target>, ReplyOrIdError> {
        let mut window = self.root;
        loop {
            let child = self.conn.query_pointer(window)?.reply()?.child;
            if child == NONE {
                return Ok(None);
            }
            // XdndProxy가 있으면 메시지는 그 창으로 보내고, XdndAware도 그 창에서 확인한다.
            let proxy = self
                .window_property(child, self.atoms.XdndProxy, AtomEnum::WINDOW.into())?
                .filter(|&p| {
                    self.window_property(p, self.atoms.XdndProxy, AtomEnum::WINDOW.into())
                        .ok()
                        .flatten()
                        == Some(p)
                });
            let send_to = proxy.unwrap_or(child);
            if let Some(version) =
                self.window_property(send_to, self.atoms.XdndAware, AtomEnum::ATOM.into())?
            {
                if version < 3 || self.is_own_window(child)? {
                    return Ok(None);
                }
                return Ok(Some(Target {
                    window: child,
                    send_to,
                    version: version.min(XDND_VERSION),
                }));
            }
            window = child;
        }
    }

    fn is_own_window(&self, window: Window) -> Result<bool, ReplyOrIdError> {
        let pid = self.window_property(window, self.atoms._NET_WM_PID, AtomEnum::CARDINAL.into())?;
        Ok(pid == Some(std::process::id()))
    }

    /// 32비트 값 하나짜리 속성을 읽는다. 창이 그새 사라졌으면 None.
    fn window_property(
        &self,
        window: Window,
        property: u32,
        ty: u32,
    ) -> Result<Option<u32>, ReplyOrIdError> {
        let reply = match self
            .conn
            .get_property(false, window, property, ty, 0, 1)?
            .reply()
        {
            Ok(reply) => reply,
            Err(x11rb::errors::ReplyError::X11Error(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(reply.value32().and_then(|mut values| values.next()))
    }

    /// 디버그 로그용 아톰 이름 (디버그를 켰을 때만 서버에 물어본다)
    fn atom_name(&self, atom: u32) -> String {
        if !debug_enabled() || atom == NONE {
            return atom.to_string();
        }
        self.conn
            .get_atom_name(atom)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map_or_else(|| atom.to_string(), |r| String::from_utf8_lossy(&r.name).into_owned())
    }

    fn send(&self, target: Target, ty: u32, data: [u32; 5]) -> Result<(), ReplyOrIdError> {
        let message = ClientMessageEvent::new(32, target.window, ty, data);
        self.conn
            .send_event(false, target.send_to, EventMask::NO_EVENT, message)?;
        Ok(())
    }
}

/// 출처로 쓸 보이지 않는 창을 만들고, 넘길 형식 목록(XdndTypeList)을 붙인다.
fn prepare_source(
    conn: &RustConnection,
    root: Window,
) -> Result<(Atoms, Window, u32, Vec<u32>), ReplyOrIdError> {
    let atoms = Atoms::new(conn)?.reply()?;
    let source = conn.generate_id()?;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        source,
        root,
        -10,
        -10,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        COPY_FROM_PARENT,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    let types = vec![atoms.TEXT_URI_LIST, atoms.TEXT_PLAIN_UTF8, atoms.UTF8_STRING];
    conn.change_property32(
        PropMode::REPLACE,
        source,
        atoms.XdndTypeList,
        AtomEnum::ATOM,
        &types,
    )?;
    let time = server_time(conn, source)?;
    Ok((atoms, source, time, types))
}

/// 현재 커서 테마(Xcursor.theme, Xcursor.size)에서 끌기용 커서를 불러온다.
/// 테마마다 이름이 달라서 GTK가 쓰는 이름부터 차례로 찾는다. 하나라도 없으면 커서는 바꾸지 않는다.
fn load_cursors(conn: &RustConnection, screen_num: usize) -> Option<Cursors> {
    let database = x11rb::resource_manager::new_from_default(conn).ok()?;
    let handle = x11rb::cursor::Handle::new(conn, screen_num, &database)
        .ok()?
        .reply()
        .ok()?;
    let load = |names: &[&str]| {
        names
            .iter()
            .map(|name| handle.load_cursor(conn, name).unwrap_or(NONE))
            .find(|&cursor| cursor != NONE)
    };
    Some(Cursors {
        copy: load(&["dnd-copy", "copy"])?,
        no_drop: load(&["dnd-no-drop", "no-drop", "dnd-none", "not-allowed"])?,
        default: load(&["default", "left_ptr"])?,
    })
}

/// X 서버의 현재 시각. 선택 영역 소유권과 XDND 메시지에 쓴다.
/// 출처 창의 속성을 건드려서 PropertyNotify 이벤트의 시각을 받는다.
fn server_time(conn: &RustConnection, window: Window) -> Result<u32, ReplyOrIdError> {
    conn.change_property8(PropMode::APPEND, window, AtomEnum::WM_NAME, AtomEnum::STRING, &[])?;
    conn.flush()?;
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline {
        match conn.poll_for_event()? {
            Some(Event::PropertyNotify(e)) if e.window == window => return Ok(e.time),
            Some(_) => {}
            None => thread::sleep(Duration::from_millis(2)),
        }
    }
    Ok(CURRENT_TIME)
}

/// `text/uri-list`: 파일마다 `file://` URI 한 줄 (CRLF로 끝냄)
fn uri_list(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = String::new();
    for path in paths {
        out.push_str(&file_uri(path));
        out.push_str("\r\n");
    }
    out.into_bytes()
}

/// 일반 텍스트: 한 줄에 경로 하나
fn plain_text(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        out.extend_from_slice(path.as_os_str().as_bytes());
    }
    out
}

/// 절대 경로 → `file:///...` (공백, 한글 등은 %XX로 바꾼다)
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_are_percent_encoded() {
        assert_eq!(
            file_uri(Path::new("/home/me/사진/a b.png")),
            "file:///home/me/%EC%82%AC%EC%A7%84/a%20b.png"
        );
        assert_eq!(file_uri(Path::new("/tmp/100%.jpg")), "file:///tmp/100%25.jpg");
    }

    /// X 화면이 필요하므로 기본으로는 돌지 않는다: `cargo test -- --ignored`
    #[test]
    #[ignore = "X11 화면이 필요함"]
    fn loads_drag_cursors_from_the_cursor_theme() {
        let (conn, screen_num) = x11rb::connect(None).expect("X11 화면에 연결");
        let cursors = load_cursors(&conn, screen_num).expect("끌기용 커서를 불러와야 함");
        assert!(cursors.copy != cursors.no_drop && cursors.copy != cursors.default);
    }

    #[test]
    fn uri_list_has_one_crlf_line_per_file() {
        let paths = [PathBuf::from("/a/1.png"), PathBuf::from("/b/2.jpg")];
        assert_eq!(uri_list(&paths), b"file:///a/1.png\r\nfile:///b/2.jpg\r\n");
        assert_eq!(plain_text(&paths), b"/a/1.png\n/b/2.jpg");
    }
}
