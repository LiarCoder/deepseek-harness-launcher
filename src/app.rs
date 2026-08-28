use crate::browser;
use crate::dsh::{self, DshInstallation, PackageManager};
use crate::logger::Logger;
use crate::process::{HarnessProcessManager, ProcessEvent};
use crate::registry;
use crate::single_instance::SingleInstance;
use crate::state;
use crate::updater;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW, LoadIconW, MessageBoxW,
    PostMessageW, PostQuitMessage, RegisterClassExW, SetForegroundWindow, SetTimer,
    SetWindowLongPtrW, TrackPopupMenu, TranslateMessage, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA,
    HWND_MESSAGE, IDI_APPLICATION, IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK,
    MB_YESNO, MF_ENABLED, MF_GRAYED, MF_SEPARATOR, MF_STRING, SW_SHOWNORMAL, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, TPM_RETURNCMD, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK,
    WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSEXW, WS_POPUP,
};

const TRAY_CALLBACK_MESSAGE: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;
const COMMAND_OPEN: usize = 1001;
const COMMAND_RESTART: usize = 1002;
const COMMAND_LOG: usize = 1003;
const COMMAND_DSH_UPDATE: usize = 1004;
const COMMAND_LAUNCHER_UPDATE: usize = 1005;
const COMMAND_EXIT: usize = 1006;

enum WorkerEvent {
    StartFinished {
        result: Result<(u32, String), String>,
        open_browser: bool,
        refresh_existing_tab: bool,
    },
    VersionFinished(Result<VersionInfo, String>),
    DshUpdateCheckFinished(Result<DshUpdateInfo, String>),
    DshUpdateInstalled {
        result: Result<String, String>,
        latest_version: String,
    },
    LauncherUpdateCheckFinished(Result<LauncherUpdateInfo, String>),
    LauncherUpdateDownloaded(Result<PathBuf, String>),
    RestartStopped {
        open_browser: bool,
    },
    ExitFinished,
}

struct VersionInfo {
    version: String,
    manager_name: &'static str,
}

struct DshUpdateInfo {
    installed_version: String,
    latest_version: String,
    manager: PackageManager,
}

struct LauncherUpdateInfo {
    installed_version: String,
    latest_version: String,
    asset_url: String,
}

pub struct App {
    hwnd: HWND,
    instance: HINSTANCE,
    tray: Option<TrayIcon>,
    single_instance: SingleInstance,
    logger: Arc<Logger>,
    harness: Arc<HarnessProcessManager>,
    process_events: Receiver<ProcessEvent>,
    worker_sender: Sender<WorkerEvent>,
    worker_events: Receiver<WorkerEvent>,
    operation_in_progress: bool,
    automatic_restart_used: bool,
    exiting: bool,
    initial_started: bool,
    dsh_version_label: String,
    launcher_version_label: String,
}

pub fn show_fatal_error(error: &str) {
    message_box(
        &format!("DeepSeek Harness Launcher 启动失败：\r\n\r\n{error}"),
        MB_OK | MB_ICONERROR,
    );
}

impl App {
    pub fn new(single_instance: SingleInstance) -> Result<Self, String> {
        let logger = Arc::new(Logger::new()?);
        let (process_sender, process_events) = mpsc::channel();
        let (worker_sender, worker_events) = mpsc::channel();
        let harness = Arc::new(HarnessProcessManager::new(process_sender));
        logger.info("启动器已启动");

        Ok(Self {
            hwnd: HWND::default(),
            instance: HINSTANCE::default(),
            tray: None,
            single_instance,
            logger,
            harness,
            process_events,
            worker_sender,
            worker_events,
            operation_in_progress: false,
            automatic_restart_used: false,
            exiting: false,
            initial_started: false,
            dsh_version_label: unknown_version_label("dsh"),
            launcher_version_label: version_label("Launcher", env!("CARGO_PKG_VERSION")),
        })
    }

    pub fn run(mut self) -> Result<(), String> {
        let module = unsafe { GetModuleHandleW(None) }
            .map_err(|error| format!("读取程序模块失败：{error}"))?;
        self.instance = HINSTANCE(module.0);
        let class_name = w!("deepseek-harness-launcher-window");
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: self.instance,
            lpszClassName: class_name,
            ..Default::default()
        };
        let atom = unsafe { RegisterClassExW(&class) };
        if atom == 0 {
            return Err("注册启动器窗口类失败".to_owned());
        }

        let hwnd = unsafe {
            CreateWindowExW(
                Default::default(),
                class_name,
                w!("DeepSeek Harness Launcher"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(self.instance),
                None,
            )
        }
        .map_err(|error| format!("创建启动器窗口失败：{error}"))?;
        self.hwnd = hwnd;

        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut self as *mut Self) as isize);
        }
        self.tray = Some(TrayIcon::new(hwnd, self.instance)?);
        if unsafe { SetTimer(Some(hwnd), TIMER_ID, 250, None) } == 0 {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return Err("创建启动器计时器失败".to_owned());
        }

        let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
        while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }

        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), TIMER_ID);
            let _ = DestroyWindow(hwnd);
        }
        Ok(())
    }

    fn on_timer(&mut self) {
        if !self.initial_started {
            self.initial_started = true;
            self.begin_start(true, true, false);
            self.refresh_version_async();
        }

        if !self.exiting && self.single_instance.is_open_web_ui_requested() {
            self.open_web_ui(false);
        }

        let process_events = self.process_events.try_iter().collect::<Vec<_>>();
        for event in process_events {
            match event {
                ProcessEvent::Output(output) => self.logger.harness(output),
                ProcessEvent::UnexpectedExit => self.handle_unexpected_exit(),
            }
        }

        let worker_events = self.worker_events.try_iter().collect::<Vec<_>>();
        for event in worker_events {
            self.handle_worker_event(event);
        }
    }

    fn handle_worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::StartFinished {
                result,
                open_browser,
                refresh_existing_tab,
            } => self.handle_start_finished(result, open_browser, refresh_existing_tab),
            WorkerEvent::VersionFinished(result) => self.handle_version_finished(result),
            WorkerEvent::DshUpdateCheckFinished(result) => {
                self.handle_dsh_update_check_finished(result)
            }
            WorkerEvent::DshUpdateInstalled {
                result,
                latest_version,
            } => self.handle_dsh_update_installed(result, &latest_version),
            WorkerEvent::LauncherUpdateCheckFinished(result) => {
                self.handle_launcher_update_check_finished(result)
            }
            WorkerEvent::LauncherUpdateDownloaded(result) => {
                self.handle_launcher_update_downloaded(result)
            }
            WorkerEvent::RestartStopped { open_browser } => {
                if self.exiting {
                    state::clear();
                    self.operation_in_progress = false;
                    unsafe {
                        PostQuitMessage(0);
                    }
                } else {
                    self.operation_in_progress = false;
                    self.begin_start(open_browser, true, true);
                }
            }
            WorkerEvent::ExitFinished => {
                state::clear();
                self.operation_in_progress = false;
                unsafe {
                    PostQuitMessage(0);
                }
            }
        }
    }

    fn begin_start(
        &mut self,
        open_browser: bool,
        reset_restart_budget: bool,
        refresh_existing_tab: bool,
    ) {
        if self.operation_in_progress || self.exiting {
            return;
        }
        self.operation_in_progress = true;
        if reset_restart_budget {
            self.automatic_restart_used = false;
        }
        self.logger.info("正在启动 DeepSeek Harness");

        let harness = Arc::clone(&self.harness);
        let sender = self.worker_sender.clone();
        thread::spawn(move || {
            let result = harness.start_and_wait(Duration::from_secs(30));
            let _ = sender.send(WorkerEvent::StartFinished {
                result,
                open_browser,
                refresh_existing_tab,
            });
        });
    }

    fn handle_start_finished(
        &mut self,
        result: Result<(u32, String), String>,
        open_browser: bool,
        refresh_existing_tab: bool,
    ) {
        self.operation_in_progress = false;
        match result {
            Ok((process_id, web_ui_url)) => {
                if let Err(error) = state::save(process_id, &web_ui_url) {
                    self.logger.error(format!("写入启动器状态失败：{error}"));
                }
                self.logger
                    .info(format!("DeepSeek Harness 已就绪：{web_ui_url}"));
                if open_browser {
                    self.open_web_ui(refresh_existing_tab);
                }
            }
            Err(error) => {
                state::clear();
                self.logger
                    .error(format!("DeepSeek Harness 启动失败：{error}"));
                message_box(
                    &format!("DeepSeek Harness 启动失败：\r\n\r\n{error}"),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
    }

    fn begin_restart(&mut self, open_browser: bool) {
        if self.operation_in_progress || self.exiting {
            return;
        }
        self.operation_in_progress = true;
        self.logger.info("正在停止 DeepSeek Harness");
        let harness = Arc::clone(&self.harness);
        let sender = self.worker_sender.clone();
        thread::spawn(move || {
            harness.stop();
            state::clear();
            let _ = sender.send(WorkerEvent::RestartStopped { open_browser });
        });
    }

    fn begin_exit(&mut self) {
        if self.exiting {
            return;
        }
        self.exiting = true;
        self.operation_in_progress = true;
        if let Some(tray) = self.tray.as_ref() {
            tray.remove();
        }
        self.logger.info("正在退出启动器并停止 DeepSeek Harness");

        let harness = Arc::clone(&self.harness);
        let sender = self.worker_sender.clone();
        thread::spawn(move || {
            harness.stop();
            let _ = sender.send(WorkerEvent::ExitFinished);
        });
    }

    fn refresh_version_async(&self) {
        let sender = self.worker_sender.clone();
        let logger = Arc::clone(&self.logger);
        thread::spawn(move || {
            let result = (|| {
                let installation = dsh::locate_installation()?;
                let version = dsh::version(&installation)?;
                Ok(VersionInfo {
                    version,
                    manager_name: installation.manager.display_name(),
                })
            })();
            if let Ok(info) = &result {
                logger.info(format!("检测到 dsh 由 {} 管理", info.manager_name));
            }
            let _ = sender.send(WorkerEvent::VersionFinished(result));
        });
    }

    fn handle_version_finished(&mut self, result: Result<VersionInfo, String>) {
        match result {
            Ok(info) => self.dsh_version_label = version_label("dsh", &info.version),
            Err(error) => {
                self.logger
                    .error(format!("读取 DeepSeek Harness 版本失败：{error}"));
                self.dsh_version_label = unknown_version_label("dsh");
            }
        }
    }

    fn begin_dsh_update_check(&mut self) {
        if self.operation_in_progress || self.exiting {
            return;
        }
        self.operation_in_progress = true;
        self.logger.info("正在检查 DeepSeek Harness 更新");
        let sender = self.worker_sender.clone();
        thread::spawn(move || {
            let result = (|| {
                let installation: DshInstallation = dsh::locate_installation()?;
                let installed_version = dsh::version(&installation)?;
                let latest_version = registry::latest_version()?;
                Ok(DshUpdateInfo {
                    installed_version,
                    latest_version,
                    manager: installation.manager,
                })
            })();
            let _ = sender.send(WorkerEvent::DshUpdateCheckFinished(result));
        });
    }

    fn handle_dsh_update_check_finished(&mut self, result: Result<DshUpdateInfo, String>) {
        let info = match result {
            Ok(info) => info,
            Err(error) => {
                self.operation_in_progress = false;
                self.logger
                    .error(format!("检查 DeepSeek Harness 更新失败：{error}"));
                message_box(
                    &format!("检查或安装更新失败：\r\n\r\n{error}"),
                    MB_OK | MB_ICONERROR,
                );
                return;
            }
        };

        self.dsh_version_label = version_label("dsh", &info.installed_version);
        if versions_equal(&info.installed_version, &info.latest_version) {
            self.operation_in_progress = false;
            message_box(
                &format!(
                    "当前已是最新版（v{}）。",
                    trim_version(&info.installed_version)
                ),
                MB_OK | MB_ICONINFORMATION,
            );
            return;
        }

        let answer = message_box_result(
            &format!(
                "发现 DeepSeek Harness 新版本：\r\n\r\n当前版本：v{}\r\n最新版本：v{}\r\n\r\n是否立即更新？",
                trim_version(&info.installed_version),
                trim_version(&info.latest_version)
            ),
            MB_YESNO | MB_ICONQUESTION,
        );
        if answer == IDYES {
            self.begin_dsh_install(info);
        } else {
            self.operation_in_progress = false;
        }
    }

    fn begin_dsh_install(&mut self, info: DshUpdateInfo) {
        self.logger.info("正在停止 DeepSeek Harness 以安装更新");
        let harness = Arc::clone(&self.harness);
        self.logger.info(format!(
            "使用 {} 更新 DeepSeek Harness",
            info.manager.display_name()
        ));
        let sender = self.worker_sender.clone();
        let logger = Arc::clone(&self.logger);
        let latest_version = info.latest_version.clone();
        thread::spawn(move || {
            harness.stop();
            state::clear();
            let result = info.manager.run_update(Duration::from_secs(300));
            if let Ok(output) = &result {
                if !output.is_empty() {
                    logger.info(output);
                }
            }
            let _ = sender.send(WorkerEvent::DshUpdateInstalled {
                result,
                latest_version,
            });
        });
    }

    fn handle_dsh_update_installed(
        &mut self,
        result: Result<String, String>,
        latest_version: &str,
    ) {
        match result {
            Ok(_) => {
                self.dsh_version_label = version_label("dsh", latest_version);
                self.operation_in_progress = false;
                message_box(
                    &format!(
                        "DeepSeek Harness 已更新到 v{}，即将重启服务。",
                        trim_version(latest_version)
                    ),
                    MB_OK | MB_ICONINFORMATION,
                );
                self.begin_start(true, true, false);
            }
            Err(error) => {
                self.operation_in_progress = false;
                self.logger
                    .error(format!("安装 DeepSeek Harness 更新失败：{error}"));
                message_box(
                    &format!("检查或安装更新失败，正在尝试重新启动 Harness：\r\n\r\n{error}"),
                    MB_OK | MB_ICONERROR,
                );
                self.begin_start(true, true, false);
            }
        }
    }

    fn begin_launcher_update_check(&mut self) {
        if self.operation_in_progress || self.exiting {
            return;
        }
        self.operation_in_progress = true;
        self.logger.info("正在检查 Launcher 更新");
        let sender = self.worker_sender.clone();
        thread::spawn(move || {
            let result = (|| {
                let release = registry::latest_launcher_release()?;
                Ok(LauncherUpdateInfo {
                    installed_version: env!("CARGO_PKG_VERSION").to_owned(),
                    latest_version: release.version,
                    asset_url: release.asset_url,
                })
            })();
            let _ = sender.send(WorkerEvent::LauncherUpdateCheckFinished(result));
        });
    }

    fn handle_launcher_update_check_finished(
        &mut self,
        result: Result<LauncherUpdateInfo, String>,
    ) {
        let info = match result {
            Ok(info) => info,
            Err(error) => {
                self.operation_in_progress = false;
                self.logger
                    .error(format!("检查 Launcher 更新失败：{error}"));
                message_box(
                    &format!("检查 Launcher 更新失败：\r\n\r\n{error}"),
                    MB_OK | MB_ICONERROR,
                );
                return;
            }
        };

        self.launcher_version_label = version_label("Launcher", &info.installed_version);
        if versions_equal(&info.installed_version, &info.latest_version) {
            self.operation_in_progress = false;
            message_box(
                &format!(
                    "当前 Launcher 已是最新版（v{}）。",
                    trim_version(&info.installed_version)
                ),
                MB_OK | MB_ICONINFORMATION,
            );
            return;
        }

        let answer = message_box_result(
            &format!(
                "发现 Launcher 新版本：\r\n\r\n当前版本：v{}\r\n最新版本：v{}\r\n\r\n是否立即下载并重启 Launcher？",
                trim_version(&info.installed_version),
                trim_version(&info.latest_version)
            ),
            MB_YESNO | MB_ICONQUESTION,
        );
        if answer != IDYES {
            self.operation_in_progress = false;
            return;
        }

        self.begin_launcher_download(info);
    }

    fn begin_launcher_download(&mut self, info: LauncherUpdateInfo) {
        self.logger.info("正在下载 Launcher 更新");
        let sender = self.worker_sender.clone();
        let logger = Arc::clone(&self.logger);
        thread::spawn(move || {
            let result = (|| {
                let bytes = registry::download_launcher_asset(&info.asset_url)?;
                updater::save_download(&bytes)
            })();
            match &result {
                Ok(_) => logger.info("Launcher 更新文件下载完成"),
                Err(error) => logger.error(format!("下载 Launcher 更新失败：{error}")),
            }
            let _ = sender.send(WorkerEvent::LauncherUpdateDownloaded(result));
        });
    }

    fn handle_launcher_update_downloaded(&mut self, result: Result<PathBuf, String>) {
        let download_path = match result {
            Ok(path) => path,
            Err(error) => {
                self.operation_in_progress = false;
                message_box(
                    &format!("下载或安装 Launcher 更新失败：\r\n\r\n{error}"),
                    MB_OK | MB_ICONERROR,
                );
                return;
            }
        };

        if let Err(error) = updater::install_and_restart(&download_path) {
            let _ = std::fs::remove_file(&download_path);
            self.operation_in_progress = false;
            self.logger
                .error(format!("安装 Launcher 更新失败：{error}"));
            message_box(
                &format!("下载或安装 Launcher 更新失败：\r\n\r\n{error}"),
                MB_OK | MB_ICONERROR,
            );
            return;
        }

        self.operation_in_progress = false;
        self.logger.info("Launcher 更新已准备完成，正在重启启动器");
        message_box(
            "Launcher 更新已下载，应用即将重启。",
            MB_OK | MB_ICONINFORMATION,
        );
        self.begin_exit();
    }

    fn handle_unexpected_exit(&mut self) {
        if self.exiting {
            return;
        }
        if !self.automatic_restart_used {
            self.automatic_restart_used = true;
            self.logger
                .error("DeepSeek Harness 意外退出，准备自动重启一次");
            self.begin_start(false, false, false);
        } else {
            self.logger
                .error("DeepSeek Harness 再次意外退出，停止自动重启");
            message_box(
                "DeepSeek Harness 再次意外退出，启动器已停止自动重启。",
                MB_OK | MB_ICONERROR,
            );
        }
    }

    fn open_web_ui(&self, refresh_existing_tab: bool) {
        if !self.harness.is_running() {
            return;
        }
        let Some(web_ui_url) = self.harness.web_ui_url() else {
            return;
        };

        if browser::try_activate(&web_ui_url, refresh_existing_tab) {
            self.logger.info("已切换到现有 Web UI 标签页");
            return;
        }
        if let Err(error) = shell_open(&web_ui_url) {
            self.logger.error(format!("无法打开默认浏览器：{error}"));
            message_box(
                &format!("无法打开默认浏览器：\r\n\r\n{error}"),
                MB_OK | MB_ICONERROR,
            );
        }
    }

    fn open_current_log(&self) {
        if let Err(error) = shell_open(self.logger.current_log_file().as_path()) {
            self.logger.error(format!("无法打开日志文件：{error}"));
            message_box(
                &format!("无法打开日志文件：\r\n\r\n{error}"),
                MB_OK | MB_ICONERROR,
            );
        }
    }

    fn handle_command(&mut self, command: usize) {
        match command {
            COMMAND_OPEN => self.open_web_ui(false),
            COMMAND_RESTART => self.begin_restart(true),
            COMMAND_LOG => self.open_current_log(),
            COMMAND_DSH_UPDATE => self.begin_dsh_update_check(),
            COMMAND_LAUNCHER_UPDATE => self.begin_launcher_update_check(),
            COMMAND_EXIT => self.begin_exit(),
            _ => {}
        }
    }

    fn show_context_menu(&mut self) {
        let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
            return;
        };
        let open_enabled = !self.operation_in_progress && self.harness.is_running();
        let restart_enabled = !self.operation_in_progress && !self.exiting;
        let update_enabled = !self.operation_in_progress && !self.exiting;
        let _ = append_menu(menu, COMMAND_OPEN, "打开 Web UI", open_enabled);
        let _ = append_menu(menu, COMMAND_RESTART, "重启 Harness", restart_enabled);
        let _ = append_menu(menu, COMMAND_LOG, "查看日志", true);
        unsafe {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        }
        let _ = append_menu(
            menu,
            COMMAND_DSH_UPDATE,
            &self.dsh_version_label,
            update_enabled,
        );
        let _ = append_menu(
            menu,
            COMMAND_LAUNCHER_UPDATE,
            &self.launcher_version_label,
            update_enabled,
        );
        unsafe {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        }
        let _ = append_menu(menu, COMMAND_EXIT, "退出", true);

        let mut point = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut point);
            let _ = SetForegroundWindow(self.hwnd);
        }
        let command = unsafe {
            TrackPopupMenu(
                menu,
                TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RETURNCMD,
                point.x,
                point.y,
                None,
                self.hwnd,
                None,
            )
        };
        unsafe {
            let _ = DestroyMenu(menu);
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        }
        if command.as_bool() {
            self.handle_command(command.0 as usize);
        }
    }
}

struct TrayIcon {
    data: NOTIFYICONDATAW,
    visible: bool,
}

impl TrayIcon {
    fn new(hwnd: HWND, instance: HINSTANCE) -> Result<Self, String> {
        let icon = unsafe {
            LoadIconW(Some(instance), PCWSTR(1 as *const u16))
                .or_else(|_| LoadIconW(None, IDI_APPLICATION))
        }
        .map_err(|error| format!("加载启动器图标失败：{error}"))?;
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: TRAY_CALLBACK_MESSAGE,
            hIcon: icon,
            ..Default::default()
        };
        copy_wide(&mut data.szTip, "DeepSeek Harness Launcher");
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            return Err("添加系统托盘图标失败".to_owned());
        }
        Ok(Self {
            data,
            visible: true,
        })
    }

    fn remove(&self) {
        if self.visible {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.data);
            }
        }
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        self.remove();
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if !pointer.is_null() {
        let app = &mut *pointer;
        match message {
            WM_TIMER if wparam.0 == TIMER_ID => {
                app.on_timer();
                return LRESULT(0);
            }
            WM_COMMAND => {
                app.handle_command(wparam.0 & 0xffff);
                return LRESULT(0);
            }
            TRAY_CALLBACK_MESSAGE => {
                match lparam.0 as u32 {
                    WM_LBUTTONDBLCLK => app.open_web_ui(false),
                    WM_RBUTTONUP | WM_CONTEXTMENU => app.show_context_menu(),
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

fn append_menu(
    menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    id: usize,
    label: &str,
    enabled: bool,
) -> Result<(), String> {
    let label = wide(label);
    let flags = MF_STRING | if enabled { MF_ENABLED } else { MF_GRAYED };
    unsafe { AppendMenuW(menu, flags, id, PCWSTR(label.as_ptr())) }
        .map_err(|error| format!("添加托盘菜单失败：{error}"))
}

fn shell_open(path: impl AsRef<OsStr>) -> Result<(), String> {
    let operation = wide("open");
    let path = wide_os(path.as_ref());
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if (result.0 as usize) <= 32 {
        Err(format!("ShellExecuteW 返回错误代码 {}", result.0 as usize))
    } else {
        Ok(())
    }
}

fn message_box(text: &str, style: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE) {
    let _ = message_box_result(text, style);
}

fn message_box_result(
    text: &str,
    style: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE,
) -> windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_RESULT {
    let text = wide(text);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            w!("DeepSeek Harness Launcher"),
            style,
        )
    }
}

fn copy_wide(target: &mut [u16], value: &str) {
    let encoded = value.encode_utf16().take(target.len().saturating_sub(1));
    for (slot, value) in target.iter_mut().zip(encoded.chain(std::iter::once(0))) {
        *slot = value;
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_os(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn trim_version(version: &str) -> &str {
    version
        .trim()
        .trim_start_matches(|character| character == 'v' || character == 'V')
}

fn version_label(component: &str, version: &str) -> String {
    format!("{component} 版本：v{} - 检查更新", trim_version(version))
}

fn unknown_version_label(component: &str) -> String {
    format!("{component} 版本：未知 - 检查更新")
}

fn versions_equal(left: &str, right: &str) -> bool {
    trim_version(left).eq_ignore_ascii_case(trim_version(right))
}

#[cfg(test)]
mod tests {
    use super::{unknown_version_label, version_label, versions_equal};

    #[test]
    fn version_labels_identify_the_component() {
        assert_eq!(
            version_label("dsh", "v1.2.3"),
            "dsh 版本：v1.2.3 - 检查更新"
        );
        assert_eq!(
            version_label("Launcher", "0.2.0"),
            "Launcher 版本：v0.2.0 - 检查更新"
        );
        assert_eq!(unknown_version_label("dsh"), "dsh 版本：未知 - 检查更新");
    }

    #[test]
    fn version_comparison_ignores_v_prefix() {
        assert!(versions_equal("v0.2.0", "0.2.0"));
        assert!(!versions_equal("0.2.0", "0.2.1"));
    }
}
