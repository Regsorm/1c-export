//! GUI на нативных Win32-контролах (native-windows-gui, GDI).
//!
//! Почему не egui/eframe: рендер glow требует OpenGL 2.0+ (в RDP-сессиях
//! серверов доступен только 1.1), а программный рендер wgpu/WARP ронял DWM
//! в RDP-сессии Windows Server (проверено на тестовом стенде). Нативные
//! контролы рисует сама Windows через GDI — работают на любой машине,
//! виртуалке и в любой RDP-сессии без GPU, OpenGL и DirectX.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use native_windows_gui as nwg;

use crate::bases_config::{BaseEntry, DaemonConfig};
use crate::command_builder::{IbcmdDbAuth, IbcmdParams};
use crate::config::{AppConfig, AuthConfig, AuthType};
use crate::export::{ExportCoordinator, ExportOptions};
use crate::git_push::{self, GitAuth};
use crate::logging::{LogLevel, Logger};

// ── Поиск bases.json (как в прежней версии GUI) ─────────────────────────────

/// Результат поиска `bases.json`: список баз и путь, откуда прочитан, плюс
/// диагностика для UI/лога.
struct BasesLoadResult {
    bases: Vec<BaseEntry>,
    /// Откуда прочитан файл (абсолютный путь). Пусто, если не нашли.
    source: String,
    /// Реальный путь к файлу для перезаписи (origin per-base). None, если не нашли.
    source_path: Option<PathBuf>,
    /// Уровень журнала из реестра баз ("info"/"debug"). Пусто, если не нашли.
    log_level: String,
    /// Сообщения отладки (что искали, почему не подошло). Показываем в логе формы.
    diagnostics: Vec<String>,
}

/// Поиск `bases.json` для подгрузки реестра баз в GUI.
/// Проверяет последовательно: текущий каталог, `deploy/`, каталог рядом с exe.
fn load_bases_for_gui() -> BasesLoadResult {
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from("bases.json"),
        PathBuf::from("deploy/bases.json"),
    ];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("bases.json"));
        }
    }

    let mut diagnostics = Vec::new();
    for p in &candidates {
        let abs = p.canonicalize().unwrap_or_else(|_| p.clone());
        if p.is_file() {
            match DaemonConfig::load(p) {
                Ok(cfg) => {
                    diagnostics.push(format!(
                        "bases.json прочитан: {} ({} баз)",
                        abs.display(),
                        cfg.bases.len()
                    ));
                    return BasesLoadResult {
                        bases: cfg.bases,
                        source: abs.display().to_string(),
                        source_path: Some(p.clone()),
                        log_level: cfg.log_level,
                        diagnostics,
                    };
                }
                Err(e) => {
                    diagnostics.push(format!("файл {} не прошёл парсинг: {}", abs.display(), e));
                }
            }
        } else {
            diagnostics.push(format!("проверял {} — нет такого файла", abs.display()));
        }
    }
    BasesLoadResult {
        bases: Vec::new(),
        source: String::new(),
        source_path: None,
        log_level: String::new(),
        diagnostics,
    }
}

/// Уровень журнала до первых строк лога: сначала реестр баз, затем одиночный
/// `config.json`, иначе info. Файл-лог в GUI открывается раньше, чем читаются
/// настройки, поэтому уровень определяется отдельно и заранее.
fn detect_log_level() -> LogLevel {
    let load = load_bases_for_gui();
    if load.source_path.is_some() {
        return LogLevel::parse(&load.log_level);
    }
    match AppConfig::load_auto() {
        Ok(cfg) => LogLevel::parse(&cfg.log_level),
        Err(_) => LogLevel::Info,
    }
}

// ── Изменяемое состояние приложения (не-контролы) ───────────────────────────

struct AppState {
    bases: Vec<BaseEntry>,
    /// Путь к bases.json, откуда прочитан реестр — для записи origin per-base обратно.
    bases_path: Option<PathBuf>,
    extensions: Vec<String>,
    mcp_url: String,
    mcp_api_key: String,
    processings_meta_name: String,

    /// Полный текст лога (TextBox перерисовывается целиком при добавлении).
    log_text: String,
    log_receiver: Option<mpsc::Receiver<String>>,

    /// Была ли занятость на прошлом тике — для обновления «Истории» по завершении.
    was_busy: bool,
    history_loaded: bool,
    /// Полные записи журнала в том же порядке, что строки таблицы «История».
    /// Нужны для окна подробностей: в таблице текст ошибки обрезан по ширине колонки.
    history_rows: Vec<crate::state_db::ExportLogRow>,
    /// Кэш последнего применённого состояния доступности контролов.
    /// set_enabled из таймера ТОЛЬКО при изменении: постоянные вызовы
    /// EnableWindow закрывают раскрытый выпадающий список «База».
    ui_flags: Option<UiFlags>,
}

/// Снимок всех признаков, влияющих на доступность контролов.
#[derive(Clone, Copy, PartialEq, Eq)]
struct UiFlags {
    busy: bool,
    had_ops: bool,
    all_ok: bool,
    force: bool,
    auth_pwd: bool,
    db_sql: bool,
    git_pwd: bool,
    proc_checked: bool,
}

// ── Все контролы окна ────────────────────────────────────────────────────────

#[derive(Default)]
pub struct App {
    window: nwg::Window,
    timer: nwg::AnimationTimer,
    status: nwg::StatusBar,

    // Верхняя строка: выбор базы
    lbl_base: nwg::Label,
    cmb_base: nwg::ComboBox<String>,
    lbl_base_src: nwg::Label,

    tabs: nwg::TabsContainer,
    tab_settings: nwg::Tab,
    tab_export: nwg::Tab,
    tab_log: nwg::Tab,
    tab_history: nwg::Tab,

    // ── Вкладка «Настройки» ──
    lbl_conn: nwg::Label,
    lbl_server: nwg::Label,
    in_server: nwg::TextInput,
    lbl_server1c: nwg::Label,
    in_server1c: nwg::TextInput,
    lbl_database: nwg::Label,
    in_database: nwg::TextInput,
    lbl_auth: nwg::Label,
    r_auth_os: nwg::RadioButton,
    r_auth_pwd: nwg::RadioButton,
    lbl_login: nwg::Label,
    in_login: nwg::TextInput,
    lbl_password: nwg::Label,
    in_password: nwg::TextInput,
    lbl_ibcmd: nwg::Label,
    in_ibcmd: nwg::TextInput,
    btn_ibcmd_browse: nwg::Button,
    lbl_output: nwg::Label,
    in_output: nwg::TextInput,
    btn_output_browse: nwg::Button,
    lbl_git_remote: nwg::Label,
    in_git_remote: nwg::TextInput,
    btn_save_cfg: nwg::Button,
    btn_reset_cfg: nwg::Button,
    lbl_settings_hint: nwg::Label,

    // ── Вкладка «Выгрузка» ──
    lbl_what: nwg::Label,
    lbl_mode_hint: nwg::Label,
    chk_base: nwg::CheckBox,
    r_base_inc: nwg::RadioButton,
    r_base_full: nwg::RadioButton,
    chk_ext: nwg::CheckBox,
    r_ext_inc: nwg::RadioButton,
    r_ext_full: nwg::RadioButton,
    chk_proc: nwg::CheckBox,
    r_proc_inc: nwg::RadioButton,
    r_proc_full: nwg::RadioButton,
    chk_rediscover: nwg::CheckBox,
    chk_artifacts: nwg::CheckBox,

    lbl_ibcmd_params: nwg::Label,
    lbl_jobs: nwg::Label,
    in_jobs: nwg::TextInput,
    chk_ibconnection: nwg::CheckBox,
    lbl_dbms: nwg::Label,
    cmb_dbms: nwg::ComboBox<String>,
    r_db_win: nwg::RadioButton,
    r_db_sql: nwg::RadioButton,
    lbl_db_user: nwg::Label,
    in_db_user: nwg::TextInput,
    lbl_db_pwd: nwg::Label,
    in_db_pwd: nwg::TextInput,

    lbl_git: nwg::Label,
    r_git_domain: nwg::RadioButton,
    r_git_pwd: nwg::RadioButton,
    lbl_git_autocrlf: nwg::Label,
    cmb_git_autocrlf: nwg::ComboBox<String>,
    lbl_git_user: nwg::Label,
    in_git_user: nwg::TextInput,
    lbl_git_pwd: nwg::Label,
    in_git_pwd: nwg::TextInput,

    btn_start: nwg::Button,
    btn_stop: nwg::Button,
    btn_push: nwg::Button,
    chk_force_push: nwg::CheckBox,
    lbl_push_hint: nwg::Label,

    // ── Вкладка «Лог» ──
    btn_log_copy: nwg::Button,
    btn_log_clear: nwg::Button,
    tb_log: nwg::TextBox,

    // ── Вкладка «История» ──
    btn_hist_refresh: nwg::Button,
    lbl_hist_count: nwg::Label,
    lv_history: nwg::ListView,

    // ── Окно «Подробности записи журнала» ──
    wnd_detail: nwg::Window,
    tb_detail: nwg::TextBox,
    btn_detail_copy: nwg::Button,
    btn_detail_close: nwg::Button,

    dlg_file: nwg::FileDialog,
    dlg_dir: nwg::FileDialog,

    // Флаги фоновых операций (разделяются с рабочими потоками)
    is_exporting: Arc<AtomicBool>,
    is_pushing: Arc<AtomicBool>,
    last_export_had_ops: Arc<AtomicBool>,
    last_export_all_ok: Arc<AtomicBool>,

    state: RefCell<AppState>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            bases: Vec::new(),
            bases_path: None,
            extensions: Vec::new(),
            mcp_url: String::new(),
            mcp_api_key: String::new(),
            processings_meta_name: "Справочник.ДополнительныеОтчетыИОбработки".to_string(),
            log_text: String::new(),
            log_receiver: None,
            was_busy: false,
            history_loaded: false,
            history_rows: Vec::new(),
            ui_flags: None,
        }
    }
}

const W_LABEL: i32 = 150;
const X_INPUT: i32 = 170;

/// Полоса под статус-бар внизу окна (как в исходной вёрстке: 780 − 40 − 690).
const STATUS_H: u32 = 50;

/// Рамка вкладок по бокам и полоса заголовков сверху: на столько площадь панели
/// вкладки меньше, чем сам TabsContainer (nwg считает так же в WM_SIZE-хуке).
const TAB_CHROME_W: u32 = 11;
const TAB_CHROME_H: u32 = 45;

/// Колонки таблицы «История»: заголовок и стартовая ширина. Последняя колонка
/// («Детали / ошибка») при изменении размера окна забирает весь остаток ширины.
const HIST_COLS: [(&str, i32); 6] = [
    ("Время", 130),
    ("База", 90),
    ("Статус", 60),
    ("Длит., с", 65),
    ("События", 70),
    ("Детали / ошибка", 320),
];

impl App {
    // ── Построение окна ─────────────────────────────────────────────────────

    fn build() -> Result<Rc<App>, nwg::NwgError> {
        let mut app = App::default();

        nwg::Window::builder()
            .flags(nwg::WindowFlags::MAIN_WINDOW | nwg::WindowFlags::VISIBLE)
            .size((800, 780))
            .position((250, 80))
            .title("Выгрузка конфигурации 1С (IBCMD) — v3.0.0")
            .build(&mut app.window)?;
        Logger::debug("GUI: окно создано");

        nwg::AnimationTimer::builder()
            .parent(&app.window)
            .interval(std::time::Duration::from_millis(150))
            .active(true)
            .build(&mut app.timer)?;
        Logger::debug("GUI: таймер создан");

        nwg::StatusBar::builder()
            .parent(&app.window)
            .text("Готово")
            .build(&mut app.status)?;
        Logger::debug("GUI: статусбар создан");

        // Верхняя строка — выбор базы
        nwg::Label::builder().parent(&app.window).position((10, 12)).size((45, 22))
            .text("База:").build(&mut app.lbl_base)?;
        nwg::ComboBox::builder().parent(&app.window).position((58, 8)).size((220, 26))
            .build(&mut app.cmb_base)?;
        nwg::Label::builder().parent(&app.window).position((290, 12)).size((490, 22))
            .text("").build(&mut app.lbl_base_src)?;
        Logger::debug("GUI: комбобокс выбора базы создан");

        nwg::TabsContainer::builder().parent(&app.window)
            .position((5, 40)).size((778, 690)).build(&mut app.tabs)?;
        nwg::Tab::builder().parent(&app.tabs).text("Настройки").build(&mut app.tab_settings)?;
        nwg::Tab::builder().parent(&app.tabs).text("Выгрузка").build(&mut app.tab_export)?;
        nwg::Tab::builder().parent(&app.tabs).text("Лог").build(&mut app.tab_log)?;
        nwg::Tab::builder().parent(&app.tabs).text("История").build(&mut app.tab_history)?;
        Logger::debug("GUI: контейнер вкладок создан");

        app.build_tab_settings()?;
        Logger::debug("GUI: вкладка «Настройки» построена");
        app.build_tab_export()?;
        Logger::debug("GUI: вкладка «Выгрузка» построена");
        app.build_tab_log()?;
        Logger::debug("GUI: вкладка «Лог» построена");
        app.build_tab_history()?;
        Logger::debug("GUI: вкладка «История» построена");
        app.build_detail_window()?;
        Logger::debug("GUI: окно подробностей построено");

        nwg::FileDialog::builder()
            .title("Выбор ibcmd.exe")
            .action(nwg::FileDialogAction::Open)
            .filters("Исполняемый файл(*.exe)|Все файлы(*.*)")
            .build(&mut app.dlg_file)?;
        nwg::FileDialog::builder()
            .title("Выбор папки для выгрузки")
            .action(nwg::FileDialogAction::OpenDirectory)
            .build(&mut app.dlg_dir)?;
        Logger::debug("GUI: диалоги выбора файла и каталога созданы");

        Ok(Rc::new(app))
    }

    fn build_tab_settings(&mut self) -> Result<(), nwg::NwgError> {
        let p = &self.tab_settings;
        nwg::Label::builder().parent(p).position((10, 10)).size((400, 22))
            .text("Подключение").build(&mut self.lbl_conn)?;

        nwg::Label::builder().parent(p).position((10, 42)).size((W_LABEL, 22))
            .text("Сервер MSSQL:").build(&mut self.lbl_server)?;
        nwg::TextInput::builder().parent(p).position((X_INPUT, 40)).size((300, 24))
            .build(&mut self.in_server)?;

        nwg::Label::builder().parent(p).position((10, 72)).size((W_LABEL, 22))
            .text("Сервер 1С:").build(&mut self.lbl_server1c)?;
        nwg::TextInput::builder().parent(p).position((X_INPUT, 70)).size((300, 24))
            .build(&mut self.in_server1c)?;

        nwg::Label::builder().parent(p).position((10, 102)).size((W_LABEL, 22))
            .text("База данных:").build(&mut self.lbl_database)?;
        nwg::TextInput::builder().parent(p).position((X_INPUT, 100)).size((300, 24))
            .build(&mut self.in_database)?;

        nwg::Label::builder().parent(p).position((10, 134)).size((W_LABEL, 22))
            .text("Авторизация в ИБ:").build(&mut self.lbl_auth)?;
        nwg::RadioButton::builder().parent(p).position((X_INPUT, 132)).size((150, 24))
            .flags(nwg::RadioButtonFlags::VISIBLE | nwg::RadioButtonFlags::GROUP)
            .text("Windows").build(&mut self.r_auth_os)?;
        nwg::RadioButton::builder().parent(p).position((330, 132)).size((200, 24))
            .text("1С (логин/пароль)").build(&mut self.r_auth_pwd)?;

        nwg::Label::builder().parent(p).position((10, 166)).size((W_LABEL, 22))
            .text("Логин:").build(&mut self.lbl_login)?;
        nwg::TextInput::builder().parent(p).position((X_INPUT, 164)).size((300, 24))
            .build(&mut self.in_login)?;

        nwg::Label::builder().parent(p).position((10, 196)).size((W_LABEL, 22))
            .text("Пароль:").build(&mut self.lbl_password)?;
        nwg::TextInput::builder().parent(p).position((X_INPUT, 194)).size((300, 24))
            .password(Some('*')).build(&mut self.in_password)?;

        nwg::Label::builder().parent(p).position((10, 228)).size((W_LABEL, 22))
            .text("Путь к ibcmd.exe:").build(&mut self.lbl_ibcmd)?;
        nwg::TextInput::builder().parent(p).position((X_INPUT, 226)).size((450, 24))
            .build(&mut self.in_ibcmd)?;
        nwg::Button::builder().parent(p).position((630, 224)).size((90, 28))
            .text("Обзор...").build(&mut self.btn_ibcmd_browse)?;

        nwg::Label::builder().parent(p).position((10, 258)).size((W_LABEL, 22))
            .text("Путь выгрузки:").build(&mut self.lbl_output)?;
        nwg::TextInput::builder().parent(p).position((X_INPUT, 256)).size((450, 24))
            .build(&mut self.in_output)?;
        nwg::Button::builder().parent(p).position((630, 254)).size((90, 28))
            .text("Обзор...").build(&mut self.btn_output_browse)?;

        nwg::Label::builder().parent(p).position((10, 288)).size((W_LABEL, 22))
            .text("Git remote (origin):").build(&mut self.lbl_git_remote)?;
        nwg::TextInput::builder().parent(p).position((X_INPUT, 286)).size((450, 24))
            .build(&mut self.in_git_remote)?;

        nwg::Button::builder().parent(p).position((10, 330)).size((200, 32))
            .text("Сохранить настройки").build(&mut self.btn_save_cfg)?;
        nwg::Button::builder().parent(p).position((220, 330)).size((200, 32))
            .text("Сбросить по умолчанию").build(&mut self.btn_reset_cfg)?;

        nwg::Label::builder().parent(p).position((10, 376)).size((740, 60))
            .text("Путь выгрузки — это и есть git-репозиторий (для «Git commit && push»).\n\
                   Git remote (origin) — куда пушить (https://... или git@...). Если пусто — origin должен быть\n\
                   уже настроен в репозитории. Реестр баз читается из bases.json (текущий каталог, deploy/, рядом с exe).")
            .build(&mut self.lbl_settings_hint)?;
        Ok(())
    }

    fn build_tab_export(&mut self) -> Result<(), nwg::NwgError> {
        let p = &self.tab_export;
        nwg::Label::builder().parent(p).position((10, 8)).size((300, 20))
            .text("Что выгружать и как").build(&mut self.lbl_what)?;
        nwg::Label::builder().parent(p).position((10, 28)).size((750, 20))
            .text("«Инкрементально» — только изменённое с прошлой выгрузки. «Полностью» — папка операции очищается и выгружается заново.")
            .build(&mut self.lbl_mode_hint)?;

        nwg::CheckBox::builder().parent(p).position((10, 54)).size((400, 22))
            .text("Основная конфигурация  →  base/")
            .check_state(nwg::CheckBoxState::Checked).build(&mut self.chk_base)?;
        nwg::RadioButton::builder().parent(p).position((40, 78)).size((280, 22))
            .flags(nwg::RadioButtonFlags::VISIBLE | nwg::RadioButtonFlags::GROUP)
            .text("Инкрементально (--sync)").build(&mut self.r_base_inc)?;
        nwg::RadioButton::builder().parent(p).position((340, 78)).size((330, 22))
            .text("Полностью (перезапись base/)").build(&mut self.r_base_full)?;

        nwg::CheckBox::builder().parent(p).position((10, 106)).size((400, 22))
            .text("Все расширения  →  extensions/")
            .check_state(nwg::CheckBoxState::Checked).build(&mut self.chk_ext)?;
        nwg::RadioButton::builder().parent(p).position((40, 130)).size((280, 22))
            .flags(nwg::RadioButtonFlags::VISIBLE | nwg::RadioButtonFlags::GROUP)
            .text("Инкрементально (изменившиеся)").build(&mut self.r_ext_inc)?;
        nwg::RadioButton::builder().parent(p).position((340, 130)).size((330, 22))
            .text("Полностью (перезапись extensions/)").build(&mut self.r_ext_full)?;

        nwg::CheckBox::builder().parent(p).position((10, 158)).size((520, 22))
            .text("Доп. обработки (справочник БСП, из MSSQL)  →  External/")
            .build(&mut self.chk_proc)?;
        nwg::RadioButton::builder().parent(p).position((40, 182)).size((280, 22))
            .flags(nwg::RadioButtonFlags::VISIBLE | nwg::RadioButtonFlags::GROUP)
            .text("Инкрементально (КонтрольнаяСумма)").build(&mut self.r_proc_inc)?;
        nwg::RadioButton::builder().parent(p).position((340, 182)).size((330, 22))
            .text("Полностью (перезапись External/)").build(&mut self.r_proc_full)?;
        nwg::CheckBox::builder().parent(p).position((40, 206)).size((520, 22))
            .text("--rediscover: переразведка структуры хранения")
            .build(&mut self.chk_rediscover)?;
        // Правая колонка первой строки блока «что выгружать» — свободна:
        // chk_base занимает x 10..410, здесь начинаем с 420.
        nwg::CheckBox::builder().parent(p).position((420, 54)).size((340, 22))
            .text("Сохранять бинарные снимки .cf/.cfe (_artifacts/)")
            .build(&mut self.chk_artifacts)?;

        nwg::Label::builder().parent(p).position((10, 238)).size((300, 20))
            .text("Параметры IBCMD").build(&mut self.lbl_ibcmd_params)?;
        nwg::Label::builder().parent(p).position((10, 262)).size((90, 22))
            .text("Потоков:").build(&mut self.lbl_jobs)?;
        nwg::TextInput::builder().parent(p).position((100, 260)).size((50, 24))
            .text("8").build(&mut self.in_jobs)?;
        nwg::CheckBox::builder().parent(p).position((190, 260)).size((450, 24))
            .text("Строка подключения (--ibconnection) — через кластер 1С")
            .build(&mut self.chk_ibconnection)?;

        nwg::Label::builder().parent(p).position((10, 292)).size((80, 22))
            .text("СУБД:").build(&mut self.lbl_dbms)?;
        nwg::ComboBox::builder().parent(p).position((100, 290)).size((160, 26))
            .collection(vec![
                "MSSQLServer".to_string(),
                "PostgreSQL".to_string(),
                "IBMDB2".to_string(),
                "OracleDatabase".to_string(),
            ])
            .selected_index(Some(0))
            .build(&mut self.cmb_dbms)?;
        nwg::RadioButton::builder().parent(p).position((290, 290)).size((230, 24))
            .flags(nwg::RadioButtonFlags::VISIBLE | nwg::RadioButtonFlags::GROUP)
            .text("Доменная (Windows)").build(&mut self.r_db_win)?;
        nwg::RadioButton::builder().parent(p).position((530, 290)).size((200, 24))
            .text("Логин+пароль SQL").build(&mut self.r_db_sql)?;

        nwg::Label::builder().parent(p).position((10, 322)).size((150, 22))
            .text("Пользователь БД:").build(&mut self.lbl_db_user)?;
        nwg::TextInput::builder().parent(p).position((170, 320)).size((180, 24))
            .build(&mut self.in_db_user)?;
        nwg::Label::builder().parent(p).position((370, 322)).size((90, 22))
            .text("Пароль БД:").build(&mut self.lbl_db_pwd)?;
        nwg::TextInput::builder().parent(p).position((470, 320)).size((180, 24))
            .password(Some('*')).build(&mut self.in_db_pwd)?;

        nwg::Label::builder().parent(p).position((10, 356)).size((100, 22))
            .text("Git:").build(&mut self.lbl_git)?;
        nwg::RadioButton::builder().parent(p).position((100, 354)).size((260, 24))
            .flags(nwg::RadioButtonFlags::VISIBLE | nwg::RadioButtonFlags::GROUP)
            .text("Доменная (credentials)").build(&mut self.r_git_domain)?;
        nwg::RadioButton::builder().parent(p).position((380, 354)).size((130, 24))
            .text("Логин+пароль").build(&mut self.r_git_pwd)?;
        // Правая часть строки «Git:» — концы строк (core.autocrlf).
        // Ширина панели вкладки ~770, поэтому радиокнопка «Логин+пароль»
        // сужена до 130 (её текст короче), а список занимает 620..760.
        nwg::Label::builder().parent(p).position((520, 356)).size((95, 22))
            .text("Концы строк:").build(&mut self.lbl_git_autocrlf)?;
        nwg::ComboBox::builder().parent(p).position((620, 354)).size((140, 26))
            .collection(AUTOCRLF_ITEMS.iter().map(|s| s.to_string()).collect::<Vec<String>>())
            .selected_index(Some(0))
            .build(&mut self.cmb_git_autocrlf)?;
        nwg::Label::builder().parent(p).position((10, 386)).size((150, 22))
            .text("Логин git:").build(&mut self.lbl_git_user)?;
        nwg::TextInput::builder().parent(p).position((170, 384)).size((180, 24))
            .build(&mut self.in_git_user)?;
        nwg::Label::builder().parent(p).position((370, 386)).size((90, 22))
            .text("Пароль git:").build(&mut self.lbl_git_pwd)?;
        nwg::TextInput::builder().parent(p).position((470, 384)).size((180, 24))
            .password(Some('*')).build(&mut self.in_git_pwd)?;

        nwg::Button::builder().parent(p).position((10, 428)).size((210, 36))
            .text("▶  Начать выгрузку").build(&mut self.btn_start)?;
        nwg::Button::builder().parent(p).position((230, 428)).size((140, 36))
            .text("Остановить").build(&mut self.btn_stop)?;

        nwg::Button::builder().parent(p).position((10, 476)).size((210, 32))
            .text("Git commit && push").build(&mut self.btn_push)?;
        nwg::CheckBox::builder().parent(p).position((230, 480)).size((290, 24))
            .text("Пушить принудительно (с ошибками)").build(&mut self.chk_force_push)?;

        nwg::Label::builder().parent(p).position((10, 520)).size((750, 60))
            .text("Push доступен после успешной выгрузки.\n\
                   Ход выполнения — на вкладке «Лог», итоги — на вкладке «История».")
            .build(&mut self.lbl_push_hint)?;
        Ok(())
    }

    fn build_tab_log(&mut self) -> Result<(), nwg::NwgError> {
        let p = &self.tab_log;
        nwg::Button::builder().parent(p).position((10, 10)).size((170, 30))
            .text("Копировать лог").build(&mut self.btn_log_copy)?;
        nwg::Button::builder().parent(p).position((190, 10)).size((170, 30))
            .text("Очистить лог").build(&mut self.btn_log_clear)?;
        nwg::TextBox::builder().parent(p).position((10, 50)).size((750, 580))
            .flags(nwg::TextBoxFlags::VISIBLE | nwg::TextBoxFlags::VSCROLL | nwg::TextBoxFlags::AUTOVSCROLL)
            .readonly(true)
            .build(&mut self.tb_log)?;
        Ok(())
    }

    fn build_tab_history(&mut self) -> Result<(), nwg::NwgError> {
        let p = &self.tab_history;
        nwg::Button::builder().parent(p).position((10, 10)).size((140, 30))
            .text("⟳ Обновить").build(&mut self.btn_hist_refresh)?;
        nwg::Label::builder().parent(p).position((170, 14)).size((400, 22))
            .text("записей: 0").build(&mut self.lbl_hist_count)?;
        nwg::ListView::builder().parent(p).position((10, 50)).size((750, 580))
            .list_style(nwg::ListViewStyle::Detailed)
            // Выделение всей строки: иначе подсвечивается только первая колонка,
            // а клик по остальным колонкам не даёт номера строки.
            .ex_flags(nwg::ListViewExFlags::FULL_ROW_SELECT)
            .build(&mut self.lv_history)?;

        for (i, (name, w)) in HIST_COLS.iter().enumerate() {
            self.lv_history.insert_column(nwg::InsertListViewColumn {
                index: Some(i as i32),
                fmt: None,
                width: Some(*w),
                text: Some((*name).to_string()),
            });
        }
        // В native-windows-gui строка заголовков выключена по умолчанию — включаем.
        self.lv_history.set_headers_enabled(true);
        Ok(())
    }

    /// Отдельное окно с полным текстом записи журнала: в таблице длинная ошибка
    /// обрезается по ширине колонки, здесь она видна целиком и копируется.
    /// Создаётся скрытым один раз; закрытие крестиком только прячет окно.
    fn build_detail_window(&mut self) -> Result<(), nwg::NwgError> {
        nwg::Window::builder()
            .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::RESIZABLE)
            .size((760, 470))
            .position((320, 200))
            .title("Подробности записи журнала")
            .build(&mut self.wnd_detail)?;
        nwg::TextBox::builder().parent(&self.wnd_detail).position((10, 10)).size((735, 400))
            .flags(nwg::TextBoxFlags::VISIBLE | nwg::TextBoxFlags::VSCROLL | nwg::TextBoxFlags::AUTOVSCROLL)
            .readonly(true)
            .build(&mut self.tb_detail)?;
        nwg::Button::builder().parent(&self.wnd_detail).position((10, 420)).size((190, 32))
            .text("Копировать текст").build(&mut self.btn_detail_copy)?;
        nwg::Button::builder().parent(&self.wnd_detail).position((210, 420)).size((130, 32))
            .text("Закрыть").build(&mut self.btn_detail_close)?;
        Ok(())
    }

    // ── Растяжение под размер окна ──────────────────────────────────────────

    /// Подогнать вкладки, таблицу истории и поле лога под текущий размер окна.
    /// Вызывается на каждое изменение размера главного окна. Поля настроек
    /// остаются фиксированными — растягиваются только элементы-«полотна».
    fn relayout(&self) {
        let (w, h) = self.window.size();
        if w < 300 || h < 200 {
            return;
        }
        self.lbl_base_src.set_size(w.saturating_sub(300), 22);
        self.tabs.set_size(w.saturating_sub(10), h.saturating_sub(40 + STATUS_H));

        // Панели вкладок ресайзит сам TabsContainer (WM_SIZE-хук nwg): рамка по
        // бокам и полоса с заголовками сверху. Своего size() у Tab нет, поэтому
        // площадь панели считаем от контейнера по этим же полям.
        let (cw, ch) = self.tabs.size();
        let inner_w = cw.saturating_sub(TAB_CHROME_W + 20);
        let inner_h = ch.saturating_sub(TAB_CHROME_H + 60);
        self.lv_history.set_size(inner_w, inner_h);
        self.tb_log.set_size(inner_w, inner_h);
        self.lbl_hist_count.set_size(inner_w.saturating_sub(170), 22);

        // Последняя колонка таблицы забирает остаток ширины (минус полоса прокрутки).
        let fixed: i32 = HIST_COLS[..HIST_COLS.len() - 1].iter().map(|(_, w)| *w).sum();
        let rest = (inner_w as i32 - fixed - 24).max(HIST_COLS[HIST_COLS.len() - 1].1);
        self.lv_history.set_column_width(HIST_COLS.len() - 1, rest as isize);
    }

    /// То же для окна подробностей: текстовое поле во всю площадь, кнопки снизу.
    fn relayout_detail(&self) {
        let (w, h) = self.wnd_detail.size();
        if w < 200 || h < 150 {
            return;
        }
        self.tb_detail.set_size(w.saturating_sub(25), h.saturating_sub(70));
        let btn_y = (h as i32) - 50;
        self.btn_detail_copy.set_position(10, btn_y);
        self.btn_detail_close.set_position(210, btn_y);
    }

    /// Показать полный текст записи журнала по индексу строки таблицы.
    fn show_history_detail(&self, row_index: usize) {
        let row = match self.state.borrow().history_rows.get(row_index) {
            Some(r) => r.clone(),
            None => return,
        };
        let crlf = |s: &str| s.replace("\r\n", "\n").replace('\n', "\r\n");
        let text = format!(
            "Время:        {}\r\n\
             База:         {}\r\n\
             Статус:       {}\r\n\
             Длительность: {}\r\n\
             Событий:      {}\r\n\
             \r\n\
             Детали:\r\n{}\r\n\
             \r\n\
             Ошибка:\r\n{}",
            row.finished_at,
            row.repo,
            row.status,
            row.duration_sec.map(|d| format!("{} с", d)).unwrap_or_else(|| "—".to_string()),
            row.events.map(|e| e.to_string()).unwrap_or_else(|| "—".to_string()),
            crlf(row.details.as_deref().unwrap_or("—")),
            crlf(row.error.as_deref().unwrap_or("—")),
        );
        self.tb_detail.set_text(&text);
        self.wnd_detail.set_visible(true);
        self.relayout_detail();
    }

    // ── Инициализация данных ────────────────────────────────────────────────

    fn init_data(&self) {
        Logger::debug("GUI: init_data начата");
        // config.json — как в прежней версии
        let loaded = AppConfig::load_auto();
        match &loaded {
            Ok(_) => Logger::debug("GUI: config.json прочитан"),
            Err(e) => Logger::debug(&format!(
                "GUI: config.json не прочитан ({}) — поля пустые",
                e
            )),
        }
        let config = loaded.unwrap_or_else(|_| AppConfig {
            server: String::new(),
            server_1c: String::new(),
            database: String::new(),
            sql_database: String::new(),
            authentication: AuthConfig {
                auth_type: AuthType::Os,
                login: String::new(),
                password: String::new(),
            },
            ibcmd_path: String::new(),
            mcp_url: String::new(),
            mcp_api_key: String::new(),
            processings_meta_name: String::new(),
            git_remote_url: String::new(),
            git_autocrlf: "false".to_string(),
            output_path: String::new(),
            log_level: Logger::level().as_str().to_string(),
            save_artifacts: false,
            extensions: Vec::new(),
        });

        self.in_server.set_text(&config.server);
        self.in_server1c.set_text(&config.server_1c);
        self.in_database.set_text(&config.database);
        self.in_login.set_text(&config.authentication.login);
        self.in_password.set_text(&config.authentication.password);
        self.in_ibcmd.set_text(&config.ibcmd_path);
        self.in_output.set_text(&config.output_path);
        self.in_git_remote.set_text(&config.git_remote_url);
        let auth_os = matches!(config.authentication.auth_type, AuthType::Os);
        self.set_radio_pair(&self.r_auth_os, &self.r_auth_pwd, auth_os);

        // Режимы по умолчанию: всё инкрементально
        self.set_radio_pair(&self.r_base_inc, &self.r_base_full, true);
        self.set_radio_pair(&self.r_ext_inc, &self.r_ext_full, true);
        self.set_radio_pair(&self.r_proc_inc, &self.r_proc_full, true);
        self.set_radio_pair(&self.r_db_win, &self.r_db_sql, true);
        self.set_radio_pair(&self.r_git_domain, &self.r_git_pwd, true);
        // Режим одиночной базы: концы строк берём из config.json.
        self.cmb_git_autocrlf
            .set_selection(Some(autocrlf_to_index(&config.git_autocrlf)));

        {
            let mut st = self.state.borrow_mut();
            st.extensions = config.extensions.clone();
            st.mcp_url = config.mcp_url.clone();
            st.mcp_api_key = config.mcp_api_key.clone();
            if !config.processings_meta_name.is_empty() {
                st.processings_meta_name = config.processings_meta_name.clone();
            }
        }

        // bases.json
        let load = load_bases_for_gui();
        // Промежуточные шаги поиска — только на уровне debug; итог — одной строкой.
        for line in &load.diagnostics {
            Logger::debug(&format!("GUI: bases.json — {}", line));
        }
        if load.source_path.is_some() {
            Logger::log(&format!(
                "GUI: bases.json прочитан: {} ({} баз)",
                load.source,
                load.bases.len()
            ));
        } else {
            // Файл есть, но не разобрался — это ошибка настройки, а не отсутствие файла;
            // причину показываем на уровне info, иначе пользователь увидит «не найден».
            let parse_errors: Vec<&String> = load
                .diagnostics
                .iter()
                .filter(|l| l.contains("не прошёл парсинг"))
                .collect();
            if parse_errors.is_empty() {
                Logger::log("GUI: bases.json не найден");
            } else {
                for line in parse_errors {
                    Logger::log(&format!("GUI: bases.json не загружен — {}", line));
                }
            }
        }
        {
            let mut st = self.state.borrow_mut();
            for line in &load.diagnostics {
                st.log_text.push_str("[init] ");
                st.log_text.push_str(line);
                st.log_text.push('\n');
            }
            if !load.bases.is_empty() {
                let aliases: Vec<&str> = load.bases.iter().map(|b| b.alias.as_str()).collect();
                st.log_text.push_str(&format!(
                    "[init] Реестр баз ({} шт.): {}\n",
                    load.bases.len(),
                    aliases.join(", ")
                ));
            }
            st.bases_path = load.source_path.clone();
            st.bases = load.bases;
        }
        self.flush_log_to_view();

        let st_bases: Vec<String> = self
            .state
            .borrow()
            .bases
            .iter()
            .map(|b| b.alias.clone())
            .collect();
        if st_bases.is_empty() {
            Logger::log("GUI: реестр баз пуст — режим одиночной базы (config.json)");
            self.lbl_base_src
                .set_text("bases.json не найден — режим одиночной базы (config.json)");
            self.status.set_text(0, "Готово (bases.json не найден — режим одиночной базы)");
        } else {
            Logger::log(&format!(
                "GUI: реестр баз ({} шт.): {}",
                st_bases.len(),
                st_bases.join(", ")
            ));
            self.cmb_base.set_collection(st_bases.clone());
            self.cmb_base.set_selection(Some(0));
            self.lbl_base_src.set_text(&format!("(из {})", load.source));
            self.status
                .set_text(0, &format!("Готово (реестр баз: {} шт.)", st_bases.len()));
            self.apply_base_by_index(0);
            Logger::debug("GUI: настройки первой базы применены");
        }
        Logger::debug("GUI: init_data завершена");
    }

    /// Радио-пара: first_checked=true → первая включена, вторая выключена.
    fn set_radio_pair(&self, a: &nwg::RadioButton, b: &nwg::RadioButton, first_checked: bool) {
        a.set_check_state(if first_checked {
            nwg::RadioButtonState::Checked
        } else {
            nwg::RadioButtonState::Unchecked
        });
        b.set_check_state(if first_checked {
            nwg::RadioButtonState::Unchecked
        } else {
            nwg::RadioButtonState::Checked
        });
    }

    fn radio_first_checked(&self, a: &nwg::RadioButton) -> bool {
        a.check_state() == nwg::RadioButtonState::Checked
    }

    fn checked(&self, c: &nwg::CheckBox) -> bool {
        c.check_state() == nwg::CheckBoxState::Checked
    }

    /// Значение `core.autocrlf`, выбранное в списке «Концы строк».
    /// Ничего не выбрано (список ещё не заполнен) — «false», как по умолчанию.
    fn selected_autocrlf(&self) -> String {
        autocrlf_from_index(self.cmb_git_autocrlf.selection().unwrap_or(0))
    }

    /// Залить настройки выбранной базы в поля формы.
    fn apply_base_by_index(&self, idx: usize) {
        let Some(b) = self.state.borrow().bases.get(idx).cloned() else { return };

        self.in_server.set_text(&b.sql_server);
        self.in_server1c.set_text(&b.server_1c);
        self.in_database.set_text(&b.sql_database);
        self.set_radio_pair(&self.r_auth_os, &self.r_auth_pwd, false); // bases.json → 1С-логин
        self.in_login.set_text(&b.login);
        self.in_password.set_text(&b.password);
        self.in_ibcmd.set_text(&b.ibcmd_path);
        self.in_output.set_text(&b.output_path);
        // Каждая база помнит свой origin в bases.json (git_remote_url) —
        // показываем именно её адрес; сохранение пишет его обратно per-base.
        self.in_git_remote.set_text(&b.git_remote_url);

        self.chk_base.set_check_state(bool_chk(b.export_base));
        self.chk_ext.set_check_state(bool_chk(b.export_extensions));
        self.chk_proc.set_check_state(bool_chk(b.export_processings));
        self.chk_rediscover.set_check_state(bool_chk(false));
        self.chk_artifacts.set_check_state(bool_chk(b.save_artifacts));

        self.set_radio_pair(&self.r_base_inc, &self.r_base_full, b.ibcmd_sync);
        self.set_radio_pair(&self.r_ext_inc, &self.r_ext_full, b.ibcmd_incremental);
        self.set_radio_pair(&self.r_proc_inc, &self.r_proc_full, b.processings_incremental);
        self.set_radio_pair(&self.r_db_win, &self.r_db_sql, b.ibcmd_db_auth_windows);
        self.in_db_user.set_text(b.db_user.as_deref().unwrap_or(""));
        self.in_db_pwd.set_text(b.db_pwd.as_deref().unwrap_or(""));
        if let Some(j) = b.ibcmd_jobs {
            self.in_jobs.set_text(&j.to_string());
        }
        self.chk_ibconnection.set_check_state(bool_chk(false));

        self.set_radio_pair(&self.r_git_domain, &self.r_git_pwd, b.git_auth_type == "domain");
        self.in_git_user.set_text(b.git_user.as_deref().unwrap_or(""));
        self.in_git_pwd.set_text(b.git_password.as_deref().unwrap_or(""));
        self.cmb_git_autocrlf
            .set_selection(Some(autocrlf_to_index(&b.git_autocrlf)));
        self.chk_force_push.set_check_state(bool_chk(false));

        {
            let mut st = self.state.borrow_mut();
            st.mcp_url = b.mcp_url.clone();
            st.mcp_api_key = b.mcp_api_key.clone();
            let timestamp = chrono::Local::now().format("%H:%M:%S");
            st.log_text.push_str(&format!(
                "[{}] Применены настройки базы '{}' (output={}, sql={}@{})\n",
                timestamp, b.alias, b.output_path, b.sql_database, b.sql_server
            ));
        }
        self.flush_log_to_view();

        // Push недоступен до первой выгрузки на новой базе
        self.last_export_had_ops.store(false, Ordering::Relaxed);
        self.last_export_all_ok.store(false, Ordering::Relaxed);
        self.status.set_text(0, &format!("Выбрана база: {}", b.alias));
    }

    // ── Сбор конфигурации из полей ──────────────────────────────────────────

    fn build_config(&self) -> AppConfig {
        let st = self.state.borrow();
        AppConfig {
            server: self.in_server.text(),
            server_1c: self.in_server1c.text(),
            database: self.in_database.text(),
            sql_database: self.in_database.text(),
            authentication: AuthConfig {
                auth_type: if self.radio_first_checked(&self.r_auth_os) {
                    AuthType::Os
                } else {
                    AuthType::Password
                },
                login: self.in_login.text(),
                password: self.in_password.text(),
            },
            ibcmd_path: self.in_ibcmd.text(),
            mcp_url: st.mcp_url.clone(),
            mcp_api_key: st.mcp_api_key.clone(),
            processings_meta_name: st.processings_meta_name.clone(),
            git_remote_url: self.in_git_remote.text(),
            git_autocrlf: self.selected_autocrlf(),
            output_path: self.in_output.text(),
            // Уровень журнала на форме не редактируется — сохраняем действующий.
            log_level: Logger::level().as_str().to_string(),
            save_artifacts: self.checked(&self.chk_artifacts),
            extensions: st.extensions.clone(),
        }
    }

    fn save_config(&self) {
        let config = self.build_config();
        let config_path = PathBuf::from("config").join("config.json");
        let _ = std::fs::create_dir_all("config");
        match serde_json::to_string_pretty(&config) {
            Ok(json) => match std::fs::write(&config_path, json) {
                Ok(_) => self.status.set_text(0, "Настройки сохранены в config/config.json"),
                Err(e) => self.status.set_text(0, &format!("Ошибка сохранения: {}", e)),
            },
            Err(e) => self.status.set_text(0, &format!("Ошибка сериализации: {}", e)),
        }
        // Настройки выбранной базы пишем в bases.json (если реестр загружен).
        self.persist_base_to_bases();
    }

    /// Записать git-адрес (origin) выбранной базы обратно в bases.json.
    /// Реестр перечитывается целиком (чтобы не потерять верхнеуровневые поля и
    /// другие базы), нужная база находится по alias, обновляется gitRemoteUrl.
    /// В режиме одиночной базы (bases.json не найден) — тихо пропускаем: origin
    /// в этом случае живёт в config.json (build_config его уже сохранил).
    fn persist_base_to_bases(&self) {
        let Some(idx) = self.cmb_base.selection() else { return };
        let (path, alias) = {
            let st = self.state.borrow();
            let Some(path) = st.bases_path.clone() else { return };
            let Some(b) = st.bases.get(idx) else { return };
            (path, b.alias.clone())
        };
        // Перечитываем реестр целиком: сохраняем верхнеуровневые поля и остальные базы.
        let mut cfg = match DaemonConfig::load(&path) {
            Ok(c) => c,
            Err(e) => {
                self.status.set_text(0, &format!("bases.json не перечитан: {}", e));
                return;
            }
        };
        let Some(entry) = cfg.bases.iter_mut().find(|b| b.alias == alias) else { return };
        self.fill_base_from_form(entry);
        let updated = entry.clone();
        match serde_json::to_string_pretty(&cfg) {
            Ok(json) => match std::fs::write(&path, json) {
                Ok(_) => {
                    // Синхронизируем копию в памяти, чтобы выбор базы показывал
                    // свежие значения без перезапуска программы.
                    if let Some(b) = self
                        .state
                        .borrow_mut()
                        .bases
                        .iter_mut()
                        .find(|b| b.alias == alias)
                    {
                        *b = updated;
                    }
                    self.status.set_text(
                        0,
                        &format!("Настройки базы '{}' записаны в {}", alias, path.display()),
                    );
                }
                Err(e) => self.status.set_text(0, &format!("bases.json не записан: {}", e)),
            },
            Err(e) => self.status.set_text(0, &format!("bases.json сериализация: {}", e)),
        }
    }

    /// Перенести значения с формы в запись базы. Обратная операция к
    /// `apply_base_by_index` — правится ровно то, что показано на форме;
    /// alias, MCP-доступ и прочие поля записи остаются нетронутыми.
    fn fill_base_from_form(&self, b: &mut BaseEntry) {
        b.sql_server = self.in_server.text();
        b.server_1c = self.in_server1c.text();
        b.sql_database = self.in_database.text();
        b.login = self.in_login.text();
        b.password = self.in_password.text();
        b.ibcmd_path = self.in_ibcmd.text();
        b.output_path = self.in_output.text();
        b.git_remote_url = self.in_git_remote.text();

        b.export_base = self.checked(&self.chk_base);
        b.export_extensions = self.checked(&self.chk_ext);
        b.export_processings = self.checked(&self.chk_proc);
        b.save_artifacts = self.checked(&self.chk_artifacts);
        b.ibcmd_sync = self.radio_first_checked(&self.r_base_inc);
        b.ibcmd_incremental = self.radio_first_checked(&self.r_ext_inc);
        b.processings_incremental = self.radio_first_checked(&self.r_proc_inc);

        b.ibcmd_db_auth_windows = self.radio_first_checked(&self.r_db_win);
        b.db_user = opt_text(self.in_db_user.text());
        b.db_pwd = opt_text(self.in_db_pwd.text());
        b.ibcmd_jobs = self.in_jobs.text().trim().parse::<u32>().ok();

        b.git_auth_type = if self.radio_first_checked(&self.r_git_domain) {
            "domain".to_string()
        } else {
            "password".to_string()
        };
        b.git_user = opt_text(self.in_git_user.text());
        b.git_password = opt_text(self.in_git_pwd.text());
        b.git_autocrlf = self.selected_autocrlf();
    }

    fn reset_config(&self) {
        for input in [
            &self.in_server, &self.in_server1c, &self.in_database,
            &self.in_login, &self.in_password, &self.in_ibcmd, &self.in_output,
            &self.in_git_remote,
        ] {
            input.set_text("");
        }
        self.set_radio_pair(&self.r_auth_os, &self.r_auth_pwd, true);
        self.status.set_text(0, "Настройки сброшены");
    }

    // ── Фоновые операции (та же логика, что в прежнем GUI) ──────────────────

    fn is_busy(&self) -> bool {
        self.is_exporting.load(Ordering::Relaxed)
            || self.is_pushing.load(Ordering::Relaxed)
    }

    fn start_export(&self) {
        if self.is_busy() {
            return;
        }
        let config = self.build_config();
        if let Err(errors) = config.validate() {
            self.status
                .set_text(0, &format!("Ошибка валидации: {}", errors.join("; ")));
            return;
        }
        let ibcmd_path = match config.ibcmd_path() {
            Ok(p) => p,
            Err(e) => {
                self.status.set_text(0, &format!("Ошибка IBCMD: {}", e));
                return;
            }
        };

        let jobs: u32 = self.in_jobs.text().trim().parse().unwrap_or(8);
        let sync = self.radio_first_checked(&self.r_base_inc);
        let db_windows = self.radio_first_checked(&self.r_db_win);
        let db_user = self.in_db_user.text();
        let db_pwd = self.in_db_pwd.text();

        let ibcmd_params = IbcmdParams {
            ibcmd_path,
            dbms: self.cmb_dbms.selection_string().unwrap_or_else(|| "MSSQLServer".to_string()),
            db_auth: if db_windows { IbcmdDbAuth::Windows } else { IbcmdDbAuth::SqlLogin },
            db_user: if db_user.is_empty() { None } else { Some(db_user) },
            db_pwd: if db_pwd.is_empty() { None } else { Some(db_pwd) },
            use_connection_string: self.checked(&self.chk_ibconnection),
            jobs,
            sync,
            // Режим «Полностью» = без --sync; --force разрешает полный дамп
            // (папку base/ приложение чистит само в export_base).
            force: !sync,
            incremental_extensions: self.radio_first_checked(&self.r_ext_inc),
        };

        let processings_params = if self.checked(&self.chk_proc) {
            Some(crate::export::ProcessingsCliParams {
                sql_server: config.server.clone(),
                override_mapping: None,
                rediscover: self.checked(&self.chk_rediscover),
                incremental: self.radio_first_checked(&self.r_proc_inc),
                // GUI отдельной настройки источника не имеет: сначала прямое
                // определение по MS SQL, при ошибке — HTTP-сервис, если он задан.
                discovery: crate::export::DiscoveryMode::Auto,
            })
        } else {
            None
        };

        let opts = ExportOptions {
            export_base: self.checked(&self.chk_base),
            export_extensions: self.checked(&self.chk_ext),
            export_processings: self.checked(&self.chk_proc),
            save_artifacts: self.checked(&self.chk_artifacts),
            // Ручной запуск: менялась ли конфигурация, неизвестно — снимок пишем как раньше.
            config_changed: None,
            ibcmd_params,
            processings_params,
        };
        let had_ops = opts.export_base || opts.export_extensions || opts.export_processings;

        // Идентификатор базы в state.db (журнал выгрузок И состояние инкремента):
        // alias выбранной базы из bases.json. Тот же alias использует служба watch —
        // иначе ручной и автоматический запуски ведут разные наборы хэшей, и
        // инкремент каждый раз выгружает все расширения заново.
        // Базы нет в реестре — откатываемся на имя папки, как было раньше.
        let repo_id = {
            let st = self.state.borrow();
            if let Some(i) = self.cmb_base.selection() {
                st.bases.get(i).map(|b| b.alias.clone())
            } else {
                None
            }
            .or_else(|| crate::bases_config::alias_for_output_path(&config.output_path))
            .unwrap_or_else(|| ExportCoordinator::derive_repo_id(&config.output_path))
        };

        let (tx, rx) = mpsc::channel::<String>();
        {
            let mut st = self.state.borrow_mut();
            st.log_receiver = Some(rx);
            st.log_text.clear();
        }
        self.flush_log_to_view();
        self.tabs.set_selected_tab(2); // «Лог»

        let is_exporting = self.is_exporting.clone();
        let last_had_ops = self.last_export_had_ops.clone();
        let last_all_ok = self.last_export_all_ok.clone();
        is_exporting.store(true, Ordering::Relaxed);
        self.status.set_text(0, "Выгрузка...");

        std::thread::spawn(move || {
            Logger::set_sender(tx);

            let started = std::time::Instant::now();
            let coordinator = ExportCoordinator::new(config).with_repo_id(repo_id.as_str());
            let results = coordinator.export_full(&opts);

            let success = results.overall_success();
            // Журнал выгрузок в state.db рядом с exe — вкладка «История».
            crate::export::record_export_log(&repo_id, &results, started.elapsed().as_secs());

            if success {
                Logger::log("=== ВСЕ ОПЕРАЦИИ ВЫПОЛНЕНЫ УСПЕШНО! ===");
            } else {
                Logger::log("=== ВЫГРУЗКА ЗАВЕРШЕНА С ОШИБКАМИ ===");
            }

            last_had_ops.store(had_ops, Ordering::Relaxed);
            last_all_ok.store(success, Ordering::Relaxed);

            Logger::clear_sender();
            is_exporting.store(false, Ordering::Relaxed);
        });
    }

    fn stop_export(&self) {
        self.is_exporting.store(false, Ordering::Relaxed);
        self.state.borrow_mut().log_receiver = None;
        Logger::clear_sender();
        self.status.set_text(0, "Остановлено пользователем");
        self.append_log_line("--- Выгрузка остановлена пользователем ---");
    }

    fn start_git_push(&self) {
        if self.is_busy() {
            return;
        }
        let output = self.in_output.text();
        if output.trim().is_empty() {
            self.status
                .set_text(0, "Не указан путь выгрузки (он же git-репозиторий)");
            return;
        }
        let repo = PathBuf::from(output);

        let auth = if self.radio_first_checked(&self.r_git_domain) {
            GitAuth::Domain
        } else {
            GitAuth::UserPassword {
                user: self.in_git_user.text(),
                password: self.in_git_pwd.text(),
            }
        };

        let remote_url = self.in_git_remote.text();

        // core.autocrlf — как выбрано на форме в списке «Концы строк».
        let git_opts = git_push::GitOptions::new(&self.selected_autocrlf());

        let (tx, rx) = mpsc::channel::<String>();
        self.state.borrow_mut().log_receiver = Some(rx);
        self.tabs.set_selected_tab(2);

        let is_pushing = self.is_pushing.clone();
        is_pushing.store(true, Ordering::Relaxed);
        self.status.set_text(0, "Git push...");

        let commit_message = git_push::default_commit_message();
        std::thread::spawn(move || {
            Logger::set_sender(tx);
            // git init при необходимости + прописать/обновить origin из поля настроек.
            if let Err(e) = git_push::ensure_repo_and_remote(&repo, &remote_url, &git_opts) {
                Logger::log(&format!("=== GIT: ОШИБКА подготовки репозитория — {} ===", e));
                Logger::clear_sender();
                is_pushing.store(false, Ordering::Relaxed);
                return;
            }
            match git_push::commit_and_push(&repo, &commit_message, &auth, &git_opts) {
                Ok(r) => {
                    if r.committed {
                        Logger::log(&format!("=== GIT: коммит '{}' запушен ===", commit_message));
                    } else {
                        Logger::log("=== GIT: нечего коммитить, push не выполнен ===");
                    }
                }
                Err(e) => Logger::log(&format!("=== GIT: ОШИБКА — {} ===", e)),
            }
            Logger::clear_sender();
            is_pushing.store(false, Ordering::Relaxed);
        });
    }

    // ── Лог и История ────────────────────────────────────────────────────────

    fn append_log_line(&self, line: &str) {
        {
            let mut st = self.state.borrow_mut();
            st.log_text.push_str(line);
            st.log_text.push('\n');
        }
        self.flush_log_to_view();
    }

    /// Перерисовать TextBox лога из state.log_text (EDIT ждёт CRLF).
    fn flush_log_to_view(&self) {
        let text = self.state.borrow().log_text.replace('\n', "\r\n");
        self.tb_log.set_text(&text);
        self.tb_log.scroll_lastline();
    }

    /// Перечитать журнал выгрузок из state.db (рядом с exe) в таблицу.
    fn reload_history(&self) {
        self.lv_history.clear();
        match crate::state_db::StateDb::open_default() {
            Ok(db) => match db.read_export_log(None, 200) {
                Ok(rows) => {
                    self.lbl_hist_count.set_text(&format!(
                        "записей: {} (клик по строке — полный текст)",
                        rows.len()
                    ));
                    for r in &rows {
                        // В таблице — одна строка: переносы ломают вид списка,
                        // полный текст показывает окно подробностей.
                        let detail = r
                            .error
                            .clone()
                            .or_else(|| r.details.clone())
                            .unwrap_or_default()
                            .replace(['\r', '\n'], " ");
                        let row: [String; 6] = [
                            r.finished_at.clone(),
                            r.repo.clone(),
                            r.status.clone(),
                            r.duration_sec.map(|d| d.to_string()).unwrap_or_default(),
                            r.events.map(|e| e.to_string()).unwrap_or_default(),
                            detail,
                        ];
                        self.lv_history.insert_items_row(None, &row);
                    }
                    self.state.borrow_mut().history_rows = rows;
                }
                Err(e) => self
                    .append_log_line(&format!("[история] ошибка чтения state.db: {}", e)),
            },
            Err(e) => self.append_log_line(&format!("[история] state.db не открылась: {}", e)),
        }
        self.state.borrow_mut().history_loaded = true;
    }

    // ── Периодический тик: перекачка лога из канала + доступность кнопок ────

    fn on_tick(&self) {
        // 1. Сообщения от рабочего потока
        let mut drained: Vec<String> = Vec::new();
        {
            let st = self.state.borrow();
            if let Some(rx) = st.log_receiver.as_ref() {
                while let Ok(msg) = rx.try_recv() {
                    drained.push(msg);
                }
            }
        }
        if !drained.is_empty() {
            {
                let mut st = self.state.borrow_mut();
                for msg in &drained {
                    st.log_text.push_str(msg);
                    st.log_text.push('\n');
                }
            }
            self.flush_log_to_view();
        }

        // 2. Завершение операции: статус + обновление «Истории»
        let busy = self.is_busy();
        let was_busy = self.state.borrow().was_busy;
        if was_busy && !busy {
            self.status.set_text(0, "Готово");
            self.reload_history();
        }
        self.state.borrow_mut().was_busy = busy;

        // 3. Доступность кнопок — применяем ТОЛЬКО при изменении состояния:
        // постоянные EnableWindow каждые 150 мс закрывают раскрытые
        // выпадающие списки (комбо «База» открывался и тут же схлопывался).
        let flags = UiFlags {
            busy,
            had_ops: self.last_export_had_ops.load(Ordering::Relaxed),
            all_ok: self.last_export_all_ok.load(Ordering::Relaxed),
            force: self.checked(&self.chk_force_push),
            auth_pwd: !self.radio_first_checked(&self.r_auth_os),
            db_sql: !self.radio_first_checked(&self.r_db_win),
            git_pwd: !self.radio_first_checked(&self.r_git_domain),
            proc_checked: self.checked(&self.chk_proc),
        };
        if self.state.borrow().ui_flags == Some(flags) {
            return;
        }
        self.state.borrow_mut().ui_flags = Some(flags);

        self.btn_start.set_enabled(!flags.busy);
        self.btn_stop.set_enabled(flags.busy);
        self.btn_push
            .set_enabled(!flags.busy && flags.had_ops && (flags.all_ok || flags.force));
        self.chk_force_push
            .set_enabled(!flags.busy && flags.had_ops && !flags.all_ok);
        self.btn_log_clear.set_enabled(!flags.busy);
        self.cmb_base.set_enabled(!flags.busy);
        // Концы строк от режима авторизации git не зависят — блокируем только на время работы.
        self.cmb_git_autocrlf.set_enabled(!flags.busy);

        // Поля логина/пароля ИБ активны только при 1С-авторизации;
        // поля БД — только при SQL-логине; поля git — при логин+пароль.
        self.in_login.set_enabled(flags.auth_pwd);
        self.in_password.set_enabled(flags.auth_pwd);
        self.in_db_user.set_enabled(flags.db_sql);
        self.in_db_pwd.set_enabled(flags.db_sql);
        self.in_git_user.set_enabled(flags.git_pwd);
        self.in_git_pwd.set_enabled(flags.git_pwd);
        self.chk_rediscover.set_enabled(flags.proc_checked);
    }

    // ── Диспетчер событий ────────────────────────────────────────────────────

    fn on_event(&self, evt: nwg::Event, data: &nwg::EventData, handle: nwg::ControlHandle) {
        use nwg::Event as E;
        match evt {
            E::OnWindowClose => {
                if handle == self.window {
                    nwg::stop_thread_dispatch();
                } else if handle == self.wnd_detail {
                    // Окно подробностей переиспользуем — прячем, а не разрушаем.
                    if let nwg::EventData::OnWindowClose(d) = data {
                        d.close(false);
                    }
                    self.wnd_detail.set_visible(false);
                }
            }
            E::OnResize | E::OnWindowMaximize => {
                if handle == self.window {
                    self.relayout();
                } else if handle == self.wnd_detail {
                    self.relayout_detail();
                }
            }
            E::OnListViewClick | E::OnListViewDoubleClick => {
                if handle == self.lv_history {
                    // Номер строки берём из события; если клик пришёл мимо строки
                    // (пустая область, заголовок) — падаем на выделенную строку.
                    let rows = self.lv_history.len();
                    let idx = match data {
                        nwg::EventData::OnListViewItemIndex { row_index, .. }
                            if *row_index < rows =>
                        {
                            Some(*row_index)
                        }
                        _ => self.lv_history.selected_item(),
                    };
                    if let Some(i) = idx {
                        self.show_history_detail(i);
                    }
                }
            }
            // Таймер в окне один — сверка handle не нужна.
            E::OnTimerTick | E::OnTimerStop => self.on_tick(),
            E::OnComboxBoxSelection => {
                if handle == self.cmb_base {
                    if let Some(i) = self.cmb_base.selection() {
                        self.apply_base_by_index(i);
                    }
                }
            }
            E::TabsContainerChanged => {
                if handle == self.tabs && self.tabs.selected_tab() == 3 {
                    self.reload_history();
                }
            }
            E::OnButtonClick => self.on_button_click(handle),
            _ => {}
        }
    }

    fn on_button_click(&self, handle: nwg::ControlHandle) {
        // Радио-пары: Win32-группы дополняем явной логикой (надёжно при любом tab-order)
        let pairs: [(&nwg::RadioButton, &nwg::RadioButton); 6] = [
            (&self.r_auth_os, &self.r_auth_pwd),
            (&self.r_base_inc, &self.r_base_full),
            (&self.r_ext_inc, &self.r_ext_full),
            (&self.r_proc_inc, &self.r_proc_full),
            (&self.r_db_win, &self.r_db_sql),
            (&self.r_git_domain, &self.r_git_pwd),
        ];
        for (a, b) in pairs {
            if handle == *a {
                self.set_radio_pair(a, b, true);
                return;
            }
            if handle == *b {
                self.set_radio_pair(a, b, false);
                return;
            }
        }

        if handle == self.btn_start {
            self.start_export();
        } else if handle == self.btn_stop {
            self.stop_export();
        } else if handle == self.btn_push {
            self.start_git_push();
        } else if handle == self.btn_save_cfg {
            self.save_config();
        } else if handle == self.btn_reset_cfg {
            self.reset_config();
        } else if handle == self.btn_hist_refresh {
            self.reload_history();
        } else if handle == self.btn_log_copy {
            let text = self.state.borrow().log_text.clone();
            let _ = nwg::Clipboard::set_data_text(&self.window, &text);
            self.status.set_text(0, "Лог скопирован в буфер обмена");
        } else if handle == self.btn_log_clear {
            self.state.borrow_mut().log_text.clear();
            self.flush_log_to_view();
        } else if handle == self.btn_ibcmd_browse {
            if self.dlg_file.run(Some(&self.window)) {
                if let Ok(item) = self.dlg_file.get_selected_item() {
                    self.in_ibcmd.set_text(&item.to_string_lossy());
                }
            }
        } else if handle == self.btn_detail_copy {
            let text = self.tb_detail.text();
            let _ = nwg::Clipboard::set_data_text(&self.wnd_detail, &text);
        } else if handle == self.btn_detail_close {
            self.wnd_detail.set_visible(false);
        } else if handle == self.btn_output_browse {
            if self.dlg_dir.run(Some(&self.window)) {
                if let Ok(item) = self.dlg_dir.get_selected_item() {
                    self.in_output.set_text(&item.to_string_lossy());
                }
            }
        }
    }
}

/// Пустое поле формы — это отсутствие значения, а не пустая строка в bases.json.
fn opt_text(s: String) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

fn bool_chk(v: bool) -> nwg::CheckBoxState {
    if v {
        nwg::CheckBoxState::Checked
    } else {
        nwg::CheckBoxState::Unchecked
    }
}

/// Пункты списка «Концы строк» в порядке отображения. Последний означает
/// «не передавать `core.autocrlf`», то есть действует настройка машины.
const AUTOCRLF_ITEMS: [&str; 4] = ["false", "true", "input", "как на машине"];

/// Значение `gitAutocrlf` → индекс пункта списка. Неизвестное значение — «false».
fn autocrlf_to_index(value: &str) -> usize {
    match value.trim() {
        "true" => 1,
        "input" => 2,
        "" => 3,
        _ => 0,
    }
}

/// Индекс пункта списка → значение `gitAutocrlf`. Индекс вне диапазона — «false».
fn autocrlf_from_index(idx: usize) -> String {
    match idx {
        1 => "true".to_string(),
        2 => "input".to_string(),
        3 => String::new(),
        _ => "false".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autocrlf_index_roundtrip() {
        for (idx, value) in ["false", "true", "input", ""].iter().enumerate() {
            assert_eq!(autocrlf_to_index(value), idx);
            assert_eq!(autocrlf_from_index(idx), *value);
        }
    }

    #[test]
    fn autocrlf_unknown_value_and_index_fall_back_to_false() {
        assert_eq!(autocrlf_to_index("CRLF"), 0);
        assert_eq!(autocrlf_to_index("  input  "), 2);
        assert_eq!(autocrlf_from_index(99), "false");
    }

    #[test]
    fn autocrlf_items_match_indexes() {
        assert_eq!(AUTOCRLF_ITEMS.len(), 4);
        assert_eq!(AUTOCRLF_ITEMS[3], "как на машине");
    }
}

// ── Точка входа GUI ──────────────────────────────────────────────────────────

pub fn run_gui() {
    // Журнал шагов запуска рядом с exe: если окно зависнет на полпути, по
    // последней строке видно, на каком шаге встало.
    let exe_path = std::env::current_exe().ok();
    let exe_dir = exe_path
        .as_ref()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    Logger::init_file_named(exe_dir.join("logs"), "gui");
    Logger::install_panic_hook();
    Logger::set_level(detect_log_level());
    Logger::log(&format!(
        "GUI: старт, версия {}, exe {}, текущий каталог {}, пользователь {}",
        env!("CARGO_PKG_VERSION"),
        exe_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "неизвестен".to_string()),
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "неизвестен".to_string()),
        std::env::var("USERNAME").unwrap_or_else(|_| "неизвестен".to_string()),
    ));

    if let Err(e) = run_gui_inner() {
        Logger::log(&format!("GUI: запуск не удался: {}", e));
        // Диалог вместо тихого выхода: exe собран без консоли.
        nwg::error_message(
            "1c-export: не удалось запустить GUI",
            &format!("Ошибка: {}\n\nCLI-режим работает независимо: 1c-export.exe --help", e),
        );
    }
}

fn run_gui_inner() -> Result<(), nwg::NwgError> {
    nwg::init()?;
    Logger::debug("GUI: nwg::init выполнен");
    // Читаемый системный шрифт покрупнее (по умолчанию у Win32 — мелкий MS Shell Dlg)
    let mut font = nwg::Font::default();
    nwg::Font::builder()
        .size(18)
        .family("Segoe UI")
        .build(&mut font)?;
    let _ = nwg::Font::set_global_default(Some(font));
    Logger::debug("GUI: шрифт установлен");

    let app = App::build()?;
    app.init_data();
    app.relayout();
    Logger::debug("GUI: relayout выполнен");

    let evt_app = Rc::downgrade(&app);
    let handler = nwg::full_bind_event_handler(&app.window.handle, move |evt, data, handle| {
        if let Some(app) = evt_app.upgrade() {
            app.on_event(evt, &data, handle);
        }
    });
    // Окно подробностей — отдельное top-level окно, события идут мимо
    // обработчика главного окна, поэтому нужна своя привязка.
    let evt_detail = Rc::downgrade(&app);
    let handler_detail =
        nwg::full_bind_event_handler(&app.wnd_detail.handle, move |evt, data, handle| {
            if let Some(app) = evt_detail.upgrade() {
                app.on_event(evt, &data, handle);
            }
        });

    Logger::debug("GUI: вход в цикл сообщений");
    nwg::dispatch_thread_events();
    Logger::debug("GUI: цикл сообщений завершён");
    nwg::unbind_event_handler(&handler_detail);
    nwg::unbind_event_handler(&handler);
    Ok(())
}
