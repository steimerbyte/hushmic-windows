//! Windows desktop shell: notification-area icon, global hotkeys, autostart,
//! single instance and toasts — the counterparts of `tray.rs`,
//! `shortcuts.rs`, `autostart.rs`, `lock.rs` and `notify.rs`, which are
//! Linux-only.
//!
//! One dedicated thread owns everything Windows ties to a *calling thread*
//! rather than to a process: a hidden window, the notification-area icon and
//! the global hotkeys. `RegisterHotKey` delivers `WM_HOTKEY` to the
//! registering thread's message queue, so it cannot be registered from the
//! eframe/winit thread — winit's event loop owns that queue and never hands a
//! message to anyone else. A thread with its own `GetMessageW` pump is the
//! only clean owner, and it also keeps the tray popup off the render thread:
//! `TrackPopupMenuEx` is a nested modal loop that would freeze the window for
//! as long as the menu is open.
//!
//! The window is a never-shown top-level window rather than a message-only
//! (`HWND_MESSAGE`) one on purpose: message-only windows receive no broadcast
//! messages, and without `TaskbarCreated` the icon stays gone after an
//! Explorer restart.
//!
//! Everything that arrives on the `WndProc` is pushed into one `mpsc` channel
//! in arrival order and drained by [`Shell::poll`], so the UI thread never
//! touches a Windows handle and never blocks on one.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA,
    ERROR_SUCCESS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL,
    MOD_NOREPEAT, VIRTUAL_KEY, VK_B, VK_M, VK_Q, VK_T,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics,
    GetWindowLongPtrW, KillTimer, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, TrackPopupMenuEx,
    TranslateMessage, UnregisterClassW, CREATESTRUCTW, GWLP_USERDATA, HICON, HMENU, ICONINFO,
    MF_CHECKED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MSG, SM_CXSMICON, SM_CYSMICON,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_HOTKEY,
    WM_LBUTTONUP, WM_NCCREATE, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW,
    WS_POPUP,
};

use crate::tr;

/// The hidden window's class. Registered per process at [`Shell::start`].
const CLASS_NAME: &str = "HushMicShellWindow";
/// `Local\` = per logon session, so two signed-in users each get their own
/// tray, exactly like the per-user `flock` on Linux.
const MUTEX_NAME: &str = r"Local\hushmic-single-instance";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "HushMic";
const TRAY_ID: u32 = 1;
/// Tray callback (legacy protocol: `lParam` is the mouse message).
const WM_TRAY: u32 = WM_APP + 1;
/// Cross-thread toast request; `lParam` owns a boxed [`Notice`].
const WM_NOTIFY: u32 = WM_APP + 2;
const TIMER_RELEASE: usize = 1;

/// The tray/panel state, mirroring the five icons `tray.rs` ships.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayState {
    On,
    Off,
    Bypass,
    Mute,
    Error,
}

impl TrayState {
    pub const ALL: [TrayState; 5] = [
        TrayState::On,
        TrayState::Off,
        TrayState::Bypass,
        TrayState::Mute,
        TrayState::Error,
    ];

    /// Icon slot index, fixed by [`TrayState::ALL`].
    fn index(self) -> usize {
        match self {
            TrayState::On => 0,
            TrayState::Off => 1,
            TrayState::Bypass => 2,
            TrayState::Mute => 3,
            TrayState::Error => 4,
        }
    }
}

/// One entry of the tray menu, in display order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuItem {
    Enable,
    Bypass,
    Mute,
    SetDefault,
    MicTest,
    Settings,
    About,
    Quit,
}

impl MenuItem {
    pub const ALL: [MenuItem; 8] = [
        MenuItem::Enable,
        MenuItem::Bypass,
        MenuItem::Mute,
        MenuItem::SetDefault,
        MenuItem::MicTest,
        MenuItem::Settings,
        MenuItem::About,
        MenuItem::Quit,
    ];

    fn index(self) -> usize {
        match self {
            MenuItem::Enable => 0,
            MenuItem::Bypass => 1,
            MenuItem::Mute => 2,
            MenuItem::SetDefault => 3,
            MenuItem::MicTest => 4,
            MenuItem::Settings => 5,
            MenuItem::About => 6,
            MenuItem::Quit => 7,
        }
    }

    /// Command id inside the popup; 0 means "menu cancelled".
    fn command(self) -> usize {
        self.index() + 1
    }

    fn from_command(command: usize) -> Option<MenuItem> {
        MenuItem::ALL.get(command.checked_sub(1)?).copied()
    }

    /// Section breaks, matching the grouping of the Linux menu.
    fn starts_section(self) -> bool {
        matches!(self, MenuItem::SetDefault | MenuItem::About)
    }

    fn label(self) -> String {
        match self {
            MenuItem::Enable => tr!("tray-mode-suppress"),
            MenuItem::Bypass => tr!("tray-mode-bypass"),
            MenuItem::Mute => tr!("tray-mode-mute"),
            MenuItem::SetDefault => tr!("tray-set-default"),
            MenuItem::MicTest => tr!("tray-test-mic"),
            // No catalog key yet — the settings window itself is English-only.
            MenuItem::Settings => "Settings…".to_string(),
            MenuItem::About => tr!("tray-about"),
            MenuItem::Quit => tr!("tray-quit"),
        }
    }
}

/// What the UI thread drains. Tray clicks and hotkeys share one queue so the
/// consumer applies them in the order the user produced them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShellEvent {
    /// Left-click on the icon: the global on/off toggle, not a menu choice.
    Toggle(MenuItem),
    /// A popup-menu command.
    Tray(MenuItem),
    /// A global hotkey; `pressed` is always true for the tap actions and
    /// becomes false for push-to-talk / push-to-mute when the key comes up.
    Hotkey { key: Hotkey, pressed: bool },
}

/// The bindable actions, the same four `shortcuts.rs` exposes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hotkey {
    ToggleMute,
    ToggleBypass,
    PushToTalk,
    PushToMute,
}

impl Hotkey {
    pub const ALL: [Hotkey; 4] = [
        Hotkey::ToggleMute,
        Hotkey::ToggleBypass,
        Hotkey::PushToTalk,
        Hotkey::PushToMute,
    ];

    fn index(self) -> usize {
        match self {
            Hotkey::ToggleMute => 0,
            Hotkey::ToggleBypass => 1,
            Hotkey::PushToTalk => 2,
            Hotkey::PushToMute => 3,
        }
    }

    fn id(self) -> i32 {
        self.index() as i32 + 1
    }

    fn from_id(id: i32) -> Option<Hotkey> {
        Hotkey::ALL
            .get(usize::try_from(id).ok()?.checked_sub(1)?)
            .copied()
    }

    fn bit(self) -> u32 {
        1 << self.index()
    }

    fn vk(self) -> u32 {
        let key: VIRTUAL_KEY = match self {
            Hotkey::ToggleMute => VK_M,
            Hotkey::ToggleBypass => VK_B,
            Hotkey::PushToTalk => VK_T,
            Hotkey::PushToMute => VK_Q,
        };
        key.0 as u32
    }

    /// Ctrl+Alt is the one combination the compositor owns on Linux and does
    /// not collide with Windows' own shortcuts; `MOD_NOREPEAT` keeps a held
    /// key from flooding the queue with auto-repeats.
    fn mods(self) -> HOT_KEY_MODIFIERS {
        MOD_CONTROL | MOD_ALT | MOD_NOREPEAT
    }

    fn chord(self) -> &'static str {
        match self {
            Hotkey::ToggleMute => "Ctrl+Alt+M",
            Hotkey::ToggleBypass => "Ctrl+Alt+B",
            Hotkey::PushToTalk => "Ctrl+Alt+T",
            Hotkey::PushToMute => "Ctrl+Alt+Q",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Hotkey::ToggleMute => "toggle mute",
            Hotkey::ToggleBypass => "toggle bypass",
            Hotkey::PushToTalk => "push to talk",
            Hotkey::PushToMute => "push to mute",
        }
    }

    /// Held keys need a synthetic release; taps are fire-and-forget.
    fn holds(self) -> bool {
        matches!(self, Hotkey::PushToTalk | Hotkey::PushToMute)
    }
}

/// The one live shell's window, for [`notify`], which has no `&self`.
static SHELL_HWND: AtomicIsize = AtomicIsize::new(0);

/// One tray/panel thread owns the hidden window, the notification-area icon
/// and the global hotkeys. Dropping it removes everything.
pub struct Shell {
    shared: Arc<Shared>,
    events: Mutex<Receiver<ShellEvent>>,
    thread: Option<JoinHandle<()>>,
    hotkey_warnings: Vec<String>,
}

impl Shell {
    /// Start the tray thread. `icon_png` is the same 8-bit RGBA PNG the
    /// window icon comes from; the per-state variants are derived from it.
    pub fn start(icon_png: &[u8]) -> Result<Shell, String> {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            tx,
            hwnd: AtomicIsize::new(0),
            taskbar_created: AtomicU32::new(0),
            icons: std::array::from_fn(|_| AtomicIsize::new(0)),
            registered: AtomicU32::new(0),
            held: AtomicU32::new(0),
            panel: Mutex::new(Panel::default()),
        });
        let (init_tx, init_rx) = mpsc::channel();
        let png = icon_png.to_vec();
        let thread_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("hushmic-shell".into())
            .spawn(move || run(thread_shared, png, init_tx))
            .map_err(|e| format!("the tray thread could not be started: {e}"))?;
        let hotkey_warnings = match init_rx.recv() {
            Ok(Ok(warnings)) => warnings,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                let _ = thread.join();
                return Err("the tray thread ended before the tray was ready".into());
            }
        };
        for warning in &hotkey_warnings {
            eprintln!("[hushmic] {warning}");
        }
        let hwnd = shared.hwnd.load(Ordering::Acquire);
        if hwnd == 0 {
            let _ = thread.join();
            return Err("the tray window closed during startup".into());
        }
        SHELL_HWND.store(hwnd, Ordering::Release);
        Ok(Shell {
            shared,
            events: Mutex::new(rx),
            thread: Some(thread),
            hotkey_warnings,
        })
    }

    /// Non-blocking drain of one event; call it in a loop from the UI thread.
    pub fn poll(&self) -> Option<ShellEvent> {
        let events = lock(&self.events);
        events.try_recv().ok()
    }

    /// Replace the icon and its tooltip in one call, the way the Linux tray
    /// publishes title + icon together.
    pub fn set_state(&self, state: TrayState, tooltip: &str) {
        {
            let mut panel = lock(&self.shared.panel);
            panel.state = state;
            panel.tip.clear();
            panel.tip.push_str(tooltip);
        }
        if let Some(hwnd) = self.hwnd() {
            if let Err(e) = unsafe { update_icon(hwnd, &self.shared) } {
                eprintln!("[hushmic] {e}");
            }
        }
    }

    /// Checkmark for a menu entry; rebuilding the menu on open means a stale
    /// checked state is never shown.
    pub fn set_menu_checked(&self, item: MenuItem, checked: bool) {
        lock(&self.shared.panel).checked[item.index()] = checked;
    }

    /// Non-fatal `RegisterHotKey` failures ("already taken by another app").
    pub fn hotkey_warnings(&self) -> &[String] {
        &self.hotkey_warnings
    }

    fn hwnd(&self) -> Option<HWND> {
        let hwnd = HWND(self.shared.hwnd.load(Ordering::Acquire) as *mut c_void);
        (!hwnd.is_invalid()).then_some(hwnd)
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        SHELL_HWND.store(0, Ordering::Release);
        if let Some(hwnd) = self.hwnd() {
            // WM_CLOSE -> DestroyWindow -> PostQuitMessage ends the pump.
            if let Err(e) = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) } {
                eprintln!("[hushmic] the tray window could not be closed: {e}");
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// State shared between the UI thread, the window thread and [`notify`].
struct Shared {
    tx: Sender<ShellEvent>,
    /// 0 while the window thread is starting or already gone.
    hwnd: AtomicIsize,
    /// `RegisterWindowMessageW("TaskbarCreated")`, sent to every top-level
    /// window when the shell restarts.
    taskbar_created: AtomicU32,
    icons: [AtomicIsize; 5],
    registered: AtomicU32,
    held: AtomicU32,
    panel: Mutex<Panel>,
}

/// What the icon currently shows; kept so a `TaskbarCreated` re-add can
/// restore the state the user last saw instead of a default.
struct Panel {
    state: TrayState,
    tip: String,
    checked: [bool; 8],
}

impl Default for Panel {
    fn default() -> Self {
        Panel {
            state: TrayState::On,
            tip: tr!("tray-title"),
            checked: [false; 8],
        }
    }
}

/// A toast posted to the window thread; the box is freed there.
struct Notice {
    title: String,
    body: String,
}

fn run(shared: Arc<Shared>, png: Vec<u8>, init: Sender<Result<Vec<String>, String>>) {
    let (hwnd, instance, class) = match unsafe { open_window(&shared) } {
        Ok(window) => window,
        Err(e) => {
            let _ = init.send(Err(e));
            return;
        }
    };
    shared.hwnd.store(hwnd.0 as isize, Ordering::Release);
    match unsafe { install(&shared, hwnd, &png) } {
        Ok(warnings) => {
            let _ = init.send(Ok(warnings));
        }
        Err(e) => {
            unsafe { teardown(&shared, hwnd, instance, &class) };
            let _ = init.send(Err(e));
            return;
        }
    }
    unsafe { pump_messages() };
    unsafe { teardown(&shared, hwnd, instance, &class) };
}

/// Register the class and create the hidden window. The `Arc` pointer rides
/// along in `CREATESTRUCT` so the `WndProc` can find the shared state before
/// any other message arrives.
unsafe fn open_window(shared: &Arc<Shared>) -> Result<(HWND, HINSTANCE, Vec<u16>), String> {
    let module = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW failed: {e}"))?;
    let instance = HINSTANCE(module.0);
    let class = wide(CLASS_NAME);
    let window_class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance,
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    if RegisterClassExW(&window_class) == 0 {
        return Err(win_error("RegisterClassExW"));
    }
    let window = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        PCWSTR(class.as_ptr()),
        PCWSTR(class.as_ptr()),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(instance),
        Some(Arc::as_ptr(shared) as *const c_void),
    );
    match window {
        Ok(hwnd) => Ok((hwnd, instance, class)),
        Err(e) => {
            let _ = UnregisterClassW(PCWSTR(class.as_ptr()), Some(instance));
            Err(format!("CreateWindowExW({CLASS_NAME}) failed: {e}"))
        }
    }
}

/// Icon + hotkeys + the broadcast message id. Hotkey failures come back as
/// warnings: one taken chord must not cost the user the tray.
unsafe fn install(shared: &Shared, hwnd: HWND, png: &[u8]) -> Result<Vec<String>, String> {
    let broadcast = wide("TaskbarCreated");
    let created = RegisterWindowMessageW(PCWSTR(broadcast.as_ptr()));
    if created == 0 {
        return Err(win_error("RegisterWindowMessageW(TaskbarCreated)"));
    }
    shared.taskbar_created.store(created, Ordering::Release);

    for (slot, icon) in shared.icons.iter().zip(build_icons(png)?) {
        slot.store(icon.0 as isize, Ordering::Release);
    }
    let nid = current_nid(hwnd, shared);
    Shell_NotifyIconW(NIM_ADD, &nid)
        .ok()
        .map_err(|e| format!("the notification-area icon could not be added: {e}"))?;

    let mut warnings = Vec::new();
    for key in Hotkey::ALL {
        match RegisterHotKey(Some(hwnd), key.id(), key.mods(), key.vk()) {
            Ok(()) => {
                shared.registered.fetch_or(key.bit(), Ordering::Relaxed);
            }
            Err(e) => warnings.push(format!(
                "the {} hotkey {} is unavailable: {e}",
                key.name(),
                key.chord()
            )),
        }
    }
    Ok(warnings)
}

unsafe fn pump_messages() {
    let mut message = MSG::default();
    // 0 is WM_QUIT and -1 is an error; both end the pump.
    while GetMessageW(&mut message, None, 0, 0).0 > 0 {
        let _ = TranslateMessage(&message);
        DispatchMessageW(&message);
    }
}

/// Remove every process-global artefact. The tray icon itself is removed in
/// `WM_DESTROY`, while the window is still valid for `Shell_NotifyIconW`.
unsafe fn teardown(shared: &Shared, hwnd: HWND, instance: HINSTANCE, class: &[u16]) {
    let registered = shared.registered.swap(0, Ordering::Relaxed);
    for key in Hotkey::ALL {
        if registered & key.bit() != 0 {
            // HOT_KEY_MODIFIERS registration is per thread; `None` releases
            // this thread's entry even though the window is gone.
            let _ = UnregisterHotKey(None, key.id());
        }
    }
    for slot in &shared.icons {
        let icon = HICON(slot.swap(0, Ordering::AcqRel) as *mut c_void);
        if !icon.is_invalid() {
            let _ = DestroyIcon(icon);
        }
    }
    if !hwnd.is_invalid() {
        let _ = DestroyWindow(hwnd);
    }
    let _ = UnregisterClassW(PCWSTR(class.as_ptr()), Some(instance));
    shared.hwnd.store(0, Ordering::Release);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        // First message: nothing is stored on the window yet, so take the
        // state pointer from CREATESTRUCT and hand creation on.
        let create = &*(l.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        return LRESULT(1);
    }
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Shared;
    if state.is_null() {
        return DefWindowProcW(hwnd, msg, w, l);
    }
    let shared = &*state;
    match msg {
        WM_TRAY => {
            // Legacy (pre-v4) callback: lParam is the mouse message.
            match (l.0 as u32) & 0xFFFF {
                WM_LBUTTONUP => send(shared, ShellEvent::Toggle(MenuItem::Enable)),
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    if let Some(item) = show_menu(hwnd, shared) {
                        send(shared, ShellEvent::Tray(item));
                    }
                }
                _ => {}
            }
        }
        WM_NOTIFY => {
            // Ownership of the box moved with the message.
            let notice = Box::from_raw(l.0 as *mut Notice);
            balloon(hwnd, &notice.title, &notice.body);
        }
        WM_HOTKEY => hotkey_pressed(shared, hwnd, w.0 as i32),
        WM_TIMER if w.0 == TIMER_RELEASE => poll_release(shared, hwnd),
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            return LRESULT(0);
        }
        WM_DESTROY => {
            let nid = current_nid(hwnd, shared);
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => {
            let created = shared.taskbar_created.load(Ordering::Acquire);
            if created != 0 && msg == created {
                // Explorer restarted and reaped every icon; ours will not come
                // back by itself.
                let nid = current_nid(hwnd, shared);
                let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            }
        }
    }
    DefWindowProcW(hwnd, msg, w, l)
}

fn send(shared: &Shared, event: ShellEvent) {
    // A send failure only means the UI dropped the shell; the thread is on
    // its way out and there is nobody left to tell.
    let _ = shared.tx.send(event);
}

unsafe fn hotkey_pressed(shared: &Shared, hwnd: HWND, id: i32) {
    let Some(key) = Hotkey::from_id(id) else {
        return;
    };
    send(shared, ShellEvent::Hotkey { key, pressed: true });
    if !key.holds() {
        return;
    }
    shared.held.fetch_or(key.bit(), Ordering::Relaxed);
    // Windows reports the press only, so the release is found by sampling the
    // key. A 20 ms timer costs nothing and no user notices the latency.
    if SetTimer(Some(hwnd), TIMER_RELEASE, 20, None) == 0 {
        // No timer means the release would never arrive and push-to-talk
        // would stick on; report it right away instead.
        shared.held.fetch_and(!key.bit(), Ordering::Relaxed);
        send(
            shared,
            ShellEvent::Hotkey {
                key,
                pressed: false,
            },
        );
        eprintln!("[hushmic] {}", win_error("SetTimer"));
    }
}

unsafe fn poll_release(shared: &Shared, hwnd: HWND) {
    let mut held = shared.held.load(Ordering::Relaxed);
    for key in Hotkey::ALL {
        if held & key.bit() != 0 && (GetAsyncKeyState(key.vk() as i32) as u16) & 0x8000 == 0 {
            send(
                shared,
                ShellEvent::Hotkey {
                    key,
                    pressed: false,
                },
            );
            held &= !key.bit();
        }
    }
    shared.held.store(held, Ordering::Relaxed);
    if held == 0 {
        let _ = KillTimer(Some(hwnd), TIMER_RELEASE);
    }
}

/// Build and run the popup menu. `TrackPopupMenuEx` blocks this thread in its
/// own modal loop until the user picks something, which is exactly why the
/// window lives on its own thread.
unsafe fn show_menu(hwnd: HWND, shared: &Shared) -> Option<MenuItem> {
    let menu = match CreatePopupMenu() {
        Ok(menu) => menu,
        Err(e) => {
            eprintln!("[hushmic] the tray menu could not be created: {e}");
            return None;
        }
    };
    let checked = lock(&shared.panel).checked;
    for item in MenuItem::ALL {
        let appended = if item.starts_section() {
            let separator = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            if separator.is_ok() {
                append_item(menu, item, checked[item.index()])
            } else {
                separator
            }
        } else {
            append_item(menu, item, checked[item.index()])
        };
        if let Err(e) = appended {
            eprintln!("[hushmic] the tray menu could not be built: {e}");
            let _ = DestroyMenu(menu);
            return None;
        }
    }
    let mut cursor = POINT::default();
    if let Err(e) = GetCursorPos(&mut cursor) {
        eprintln!("[hushmic] the popup could not be positioned: {e}");
        let _ = DestroyMenu(menu);
        return None;
    }
    // Without the foreground handover the menu can refuse to close when the
    // user clicks elsewhere; the trailing WM_NULL is the documented follow-up.
    let _ = SetForegroundWindow(hwnd);
    let chosen = TrackPopupMenuEx(
        menu,
        TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0,
        cursor.x,
        cursor.y,
        hwnd,
        None,
    );
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    MenuItem::from_command(chosen.0 as usize)
}

unsafe fn append_item(menu: HMENU, item: MenuItem, checked: bool) -> windows::core::Result<()> {
    let check = if checked { MF_CHECKED } else { MF_UNCHECKED };
    let label = wide(&item.label());
    AppendMenuW(
        menu,
        MF_STRING | check,
        item.command(),
        PCWSTR(label.as_ptr()),
    )
}

/// One `HICON` per tray state, rendered from the single PNG the caller has.
unsafe fn build_icons(png: &[u8]) -> Result<Vec<HICON>, String> {
    let (source_width, source_height, rgba) = decode_png(png)?;
    // The shell's icon slot is the same at every DPI; a later DPI change
    // leaves Windows to scale this bitmap.
    let width = GetSystemMetrics(SM_CXSMICON).max(16) as u32;
    let height = GetSystemMetrics(SM_CYSMICON).max(16) as u32;
    let base = resample(&rgba, source_width, source_height, width, height);
    let mut icons = Vec::with_capacity(TrayState::ALL.len());
    for state in TrayState::ALL {
        let mut pixels = base.clone();
        tint(&mut pixels, width, height, state);
        match icon_from_rgba(&pixels, width, height) {
            Ok(icon) => icons.push(icon),
            Err(e) => {
                for built in icons {
                    let _ = DestroyIcon(built);
                }
                return Err(format!("the tray icon could not be built: {e}"));
            }
        }
    }
    Ok(icons)
}

/// 32bpp top-down DIB as the icon image plus a 1bpp mask for its alpha, then
/// `CreateIconIndirect`. `CreateIconFromResourceEx` would take the PNG
/// directly, but only as a PNG *icon* image and with no error feedback at
/// all; building the DIB keeps the failure modes ours.
unsafe fn icon_from_rgba(rgba: &[u8], width: u32, height: u32) -> Result<HICON, String> {
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        // Negative height = top-down, matching the PNG's row order.
        biHeight: -(height as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let info = BITMAPINFO {
        bmiHeader: header,
        ..Default::default()
    };
    let mut bits: *mut c_void = null_mut();
    let color = match CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
        Ok(bitmap) => bitmap,
        Err(e) => return Err(format!("CreateDIBSection failed: {e}")),
    };
    if bits.is_null() {
        let _ = DeleteObject(HGDIOBJ::from(color));
        return Err("CreateDIBSection returned no pixel memory".into());
    }
    // GDI wants BGRA; alpha is reduced to "opaque or masked" so the icon is
    // drawn identically by every shell version instead of relying on
    // premultiplied alpha handling.
    let pixels = std::slice::from_raw_parts_mut(bits as *mut u8, rgba.len());
    for (source, target) in rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(pixels.as_chunks_mut::<4>().0)
    {
        target[0] = source[2];
        target[1] = source[1];
        target[2] = source[0];
        target[3] = if source[3] >= 128 { 255 } else { 0 };
    }
    let stride = (width as usize).div_ceil(32) * 4;
    let mut mask = vec![0u8; stride * height as usize];
    for y in 0..height as usize {
        for x in 0..width as usize {
            if rgba[(y * width as usize + x) * 4 + 3] < 128 {
                mask[y * stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    let mask_bitmap = CreateBitmap(
        width as i32,
        height as i32,
        1,
        1,
        Some(mask.as_ptr() as *const c_void),
    );
    if mask_bitmap.is_invalid() {
        let _ = DeleteObject(HGDIOBJ::from(color));
        // GDI does not document a last-error value for CreateBitmap, so a
        // guessed code would be stale; be explicit instead.
        return Err("CreateBitmap returned no mask bitmap".into());
    }
    let icon_info = ICONINFO {
        fIcon: BOOL(1),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask_bitmap,
        hbmColor: color,
    };
    let icon = CreateIconIndirect(&icon_info);
    let _ = DeleteObject(HGDIOBJ::from(mask_bitmap));
    let _ = DeleteObject(HGDIOBJ::from(color));
    icon.map_err(|e| format!("CreateIconIndirect failed: {e}"))
}

/// Decode the embedded 8-bit RGBA PNG, the same way `branding.rs` does for
/// the window icon.
fn decode_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("the tray icon PNG could not be read: {e}"))?;
    let mut rgba = vec![
        0;
        reader
            .output_buffer_size()
            .ok_or("the tray icon PNG is too large to decode")?
    ];
    let info = reader
        .next_frame(&mut rgba)
        .map_err(|e| format!("the tray icon PNG could not be decoded: {e}"))?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "the tray icon must be 8-bit RGBA (got {:?}/{:?})",
            info.color_type, info.bit_depth
        ));
    }
    rgba.truncate(info.buffer_size());
    Ok((info.width, info.height, rgba))
}

/// Box-filter downscale in premultiplied space. Averaging straight RGBA
/// bleeds the transparent background colour into every edge pixel, which
/// shows as a dark fringe once the icon sits on a light or red taskbar.
fn resample(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let mut out = vec![0u8; (width * height * 4) as usize];
    for y in 0..height as u64 {
        let y0 = y * source_height as u64 / height as u64;
        let y1 = ((y + 1) * source_height as u64 / height as u64)
            .max(y0 + 1)
            .min(source_height as u64);
        for x in 0..width as u64 {
            let x0 = x * source_width as u64 / width as u64;
            let x1 = ((x + 1) * source_width as u64 / width as u64)
                .max(x0 + 1)
                .min(source_width as u64);
            let (mut red, mut green, mut blue, mut alpha, mut count) =
                (0u64, 0u64, 0u64, 0u64, 0u64);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = ((sy * source_width as u64 + sx) * 4) as usize;
                    let a = source[i + 3] as u64;
                    red += source[i] as u64 * a;
                    green += source[i + 1] as u64 * a;
                    blue += source[i + 2] as u64 * a;
                    alpha += a;
                    count += 1;
                }
            }
            let o = ((y * width as u64 + x) * 4) as usize;
            out[o] = red.checked_div(alpha).unwrap_or(0) as u8;
            out[o + 1] = green.checked_div(alpha).unwrap_or(0) as u8;
            out[o + 2] = blue.checked_div(alpha).unwrap_or(0) as u8;
            out[o + 3] = (alpha / count) as u8;
        }
    }
    out
}

/// Recolour one state variant. Linux ships five icon files; here the state is
/// the same glyph tinted, so a single PNG still gives five distinguishable
/// icons.
fn tint(rgba: &mut [u8], width: u32, height: u32, state: TrayState) {
    let (red, green, blue) = match state {
        TrayState::On => return,
        // Grey = filter bypassed; dimmed grey = chain off.
        TrayState::Bypass => (104, 104, 104),
        TrayState::Off => (58, 58, 58),
        TrayState::Error => (222, 138, 20),
        TrayState::Mute => (208, 46, 46),
    };
    for pixel in rgba.as_chunks_mut::<4>().0 {
        let luma = (pixel[0] as u32 * 299 + pixel[1] as u32 * 587 + pixel[2] as u32 * 114) / 1000;
        pixel[0] = mix(red, luma);
        pixel[1] = mix(green, luma);
        pixel[2] = mix(blue, luma);
    }
    if state != TrayState::Mute {
        return;
    }
    // Mute is a privacy state and has to be unmistakable at 16 px, where a
    // colour alone reads as "some other icon".
    let thickness = (width.min(height) / 8).max(1) as i32;
    for y in 0..height as i32 {
        for x in 0..width as i32 {
            if (x - y).abs() < thickness {
                let i = ((y as u32 * width + x as u32) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
}

/// Three parts state colour to one part glyph shading, so the glyph keeps its
/// shape instead of turning into a flat blob.
fn mix(target: u8, luma: u32) -> u8 {
    ((target as u32 * 3 + luma) / 4) as u8
}

unsafe fn current_nid(hwnd: HWND, shared: &Shared) -> NOTIFYICONDATAW {
    let panel = lock(&shared.panel);
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_ICON | NIF_TIP | NIF_MESSAGE,
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    };
    nid.hIcon = HICON(shared.icons[panel.state.index()].load(Ordering::Acquire) as *mut c_void);
    fill(&mut nid.szTip, &panel.tip);
    nid
}

unsafe fn update_icon(hwnd: HWND, shared: &Shared) -> Result<(), String> {
    let nid = current_nid(hwnd, shared);
    Shell_NotifyIconW(NIM_MODIFY, &nid)
        .ok()
        .map_err(|e| format!("the tray icon could not be updated: {e}"))
}

/// Windows 10+ turns `NIF_INFO` into a toast; older shells show a balloon.
/// Never blocks the caller: the work happens on the window thread.
unsafe fn balloon(hwnd: HWND, title: &str, body: &str) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_INFO,
        dwInfoFlags: NIIF_INFO,
        ..Default::default()
    };
    fill(&mut nid.szInfoTitle, title);
    fill(&mut nid.szInfo, body);
    if let Err(e) = Shell_NotifyIconW(NIM_MODIFY, &nid).ok() {
        eprintln!("[hushmic] the notification could not be shown: {e}");
    }
}

/// Toast/banner notification, replacing `notify.rs`'s DBus queue.
pub fn notify(title: &str, body: &str) -> Result<(), String> {
    let hwnd = HWND(SHELL_HWND.load(Ordering::Acquire) as *mut c_void);
    if hwnd.is_invalid() {
        return Err("there is no tray icon to show the notification on".into());
    }
    // Posted, not executed inline: the window thread may be inside its own
    // popup loop, and a notification must never delay the caller.
    let notice = Box::into_raw(Box::new(Notice {
        title: title.to_string(),
        body: body.to_string(),
    }));
    match unsafe { PostMessageW(Some(hwnd), WM_NOTIFY, WPARAM(0), LPARAM(notice as isize)) } {
        Ok(()) => Ok(()),
        Err(e) => {
            drop(unsafe { Box::from_raw(notice) });
            Err(format!("the notification could not be queued: {e}"))
        }
    }
}

/// Single-instance guard, the counterpart of `lock.rs`'s `flock`: a named
/// mutex whose *existence* is the lock, so the kernel releases it when the
/// last handle closes — a crash included, and no stale lock to clean up.
pub struct InstanceLock {
    handle: HANDLE,
    /// False when another instance already owns the mutex.
    pub acquired: bool,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }
}

pub fn single_instance() -> Result<InstanceLock, String> {
    let (handle, taken) = open_mutex()?;
    if taken {
        let _ = unsafe { CloseHandle(handle) };
        return Ok(InstanceLock {
            handle: HANDLE(null_mut()),
            acquired: false,
        });
    }
    Ok(InstanceLock {
        handle,
        acquired: true,
    })
}

/// Read-only probe for the doctor: true while another instance holds the
/// mutex. It never keeps the handle, so a doctor run cannot take the guard.
pub fn instance_running() -> Result<bool, String> {
    let (handle, taken) = open_mutex()?;
    let _ = unsafe { CloseHandle(handle) };
    Ok(taken)
}

/// `(handle, already_existed)`. Never takes ownership, so no abandonment
/// semantics are involved.
fn open_mutex() -> Result<(HANDLE, bool), String> {
    let name = wide(MUTEX_NAME);
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
        .map_err(|e| format!("CreateMutexW({MUTEX_NAME}) failed: {e}"))?;
    // Must be read before anything else can overwrite the thread's last error.
    let taken = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    Ok((handle, taken))
}

/// Quoted, because `CreateProcess` splits an unquoted path at its first
/// space; `--tray` is the same argument the Linux `Exec=` line uses.
fn run_command() -> Result<String, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("the executable path is unknown: {e}"))?;
    Ok(format!("\"{}\" --tray", exe.display()))
}

pub fn autostart_enabled() -> Result<bool, String> {
    let expected = run_command()?;
    let key = wide(RUN_KEY);
    let value = wide(RUN_VALUE);
    let mut buffer = [0u16; 1024];
    let mut bytes = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr() as *mut c_void),
            Some(&mut bytes),
        )
    };
    if status == ERROR_SUCCESS {
        let units = (bytes as usize / 2).min(buffer.len());
        let stored = String::from_utf16_lossy(&buffer[..units]);
        // Compare content, not existence: a relocated install leaves the old
        // path behind and must read as "off", like the Linux reader does.
        return Ok(stored.trim_end_matches('\0') == expected);
    }
    // No value, or one longer than any real command line.
    if status == ERROR_FILE_NOT_FOUND || status == ERROR_MORE_DATA {
        return Ok(false);
    }
    Err(format!(
        "reading HKCU\\{RUN_KEY}\\{RUN_VALUE} failed: Windows error {}",
        status.0
    ))
}

pub fn set_autostart(enabled: bool) -> Result<(), String> {
    let key = wide(RUN_KEY);
    let value = wide(RUN_VALUE);
    if !enabled {
        let mut handle = HKEY::default();
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                None,
                KEY_SET_VALUE,
                &mut handle,
            )
        };
        // A fresh profile has no Run key at all: nothing to remove.
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        if status != ERROR_SUCCESS {
            return Err(format!(
                "opening HKCU\\{RUN_KEY} failed: Windows error {}",
                status.0
            ));
        }
        let status = unsafe { RegDeleteValueW(handle, PCWSTR(value.as_ptr())) };
        let _ = unsafe { RegCloseKey(handle) };
        if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        return Err(format!(
            "removing HKCU\\{RUN_KEY}\\{RUN_VALUE} failed: Windows error {}",
            status.0
        ));
    }
    let command = run_command()?;
    let mut handle = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut handle,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "opening HKCU\\{RUN_KEY} for writing failed: Windows error {}",
            status.0
        ));
    }
    let data = wide(&command);
    let bytes = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2) };
    let status =
        unsafe { RegSetValueExW(handle, PCWSTR(value.as_ptr()), None, REG_SZ, Some(bytes)) };
    let _ = unsafe { RegCloseKey(handle) };
    // Re-writing identical data is the idempotent reconcile; `Run` values are
    // a set, so a duplicate entry cannot appear.
    if status != ERROR_SUCCESS {
        return Err(format!(
            "writing HKCU\\{RUN_KEY}\\{RUN_VALUE} failed: Windows error {}",
            status.0
        ));
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Poisoning only means some caller panicked while holding the lock; the
    // tray state is still sound, and losing the icon over it would be worse.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// NUL-terminated UTF-16, as every Win32 `...W` call wants it.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copy into a fixed-size Win32 string field without splitting a surrogate
/// pair at the cut.
fn fill(field: &mut [u16], text: &str) {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.truncate(field.len().saturating_sub(1));
    if units
        .last()
        .is_some_and(|last| (0xD800..0xDC00).contains(last))
    {
        units.pop();
    }
    field[..units.len()].copy_from_slice(&units);
}

fn win_error(call: &str) -> String {
    format!(
        "{call} failed: Windows error {}",
        unsafe { GetLastError() }.0
    )
}
