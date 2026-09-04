use chrono::Local;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, Once};

/// Глобальный канал для отправки логов в GUI (если установлен)
static LOG_SENDER: Mutex<Option<std::sync::mpsc::Sender<String>>> = Mutex::new(None);

/// Уровень подробности журнала.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Старт, найденные настройки, ход выгрузки, ошибки.
    Info,
    /// То же плюс пошаговая трассировка (например, запуск GUI).
    Debug,
}

impl LogLevel {
    /// Разбор значения из файла настроек, без учёта регистра.
    /// Всё, кроме "debug", считается уровнем info — отдельной валидации нет.
    pub fn parse(value: &str) -> Self {
        if value.trim().eq_ignore_ascii_case("debug") {
            LogLevel::Debug
        } else {
            LogLevel::Info
        }
    }

    /// Значение для записи обратно в файл настроек.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
        }
    }
}

/// Текущий уровень журнала: 0 — info, 1 — debug.
static LOG_LEVEL: AtomicU8 = AtomicU8::new(0);

/// Файловый лог. Хранит каталог, префикс имени файла и текущий файл с датой
/// открытия. При смене даты файл переоткрывается на новый — ежедневная ротация.
struct FileLogState {
    dir: PathBuf,
    prefix: String,         // "watch", "gui", ...
    current_date: String,   // "YYYY-MM-DD"
    file: File,
}

static FILE_LOG: Mutex<Option<FileLogState>> = Mutex::new(None);

/// Логгер с таймштампами [HH:MM:SS]
/// В CLI-режиме выводит в stdout, в GUI-режиме отправляет через канал.
/// Дополнительно (если вызван init_file_in / init_file_named) дублирует в файл
/// `<префикс>-YYYY-MM-DD.log`.
pub struct Logger;

impl Logger {
    /// Установить канал для отправки логов (GUI-режим)
    pub fn set_sender(sender: std::sync::mpsc::Sender<String>) {
        *LOG_SENDER.lock().unwrap() = Some(sender);
    }

    /// Убрать канал (возврат к stdout)
    pub fn clear_sender() {
        *LOG_SENDER.lock().unwrap() = None;
    }

    /// Установить уровень подробности журнала (из файла настроек).
    pub fn set_level(level: LogLevel) {
        LOG_LEVEL.store(
            match level {
                LogLevel::Info => 0,
                LogLevel::Debug => 1,
            },
            Ordering::Relaxed,
        );
    }

    /// Текущий уровень подробности журнала.
    pub fn level() -> LogLevel {
        if LOG_LEVEL.load(Ordering::Relaxed) == 1 {
            LogLevel::Debug
        } else {
            LogLevel::Info
        }
    }

    /// Включить запись логов в файл `<dir>/watch-YYYY-MM-DD.log` с ежедневной ротацией.
    pub fn init_file_in(dir: PathBuf) {
        Self::init_file_named(dir, "watch");
    }

    /// Включить запись логов в файл `<dir>/<prefix>-YYYY-MM-DD.log` с ежедневной ротацией.
    /// Каталог создаётся, если его нет. При ошибке открытия — лог в stderr и продолжение
    /// без файла (stdout/GUI-канал продолжают работать).
    pub fn init_file_named(dir: PathBuf, prefix: &str) {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("Logger: не удалось создать каталог логов {}: {}", dir.display(), e);
            return;
        }
        let date = Local::now().format("%Y-%m-%d").to_string();
        let path = dir.join(format!("{}-{}.log", prefix, date));
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                *FILE_LOG.lock().unwrap() = Some(FileLogState {
                    dir,
                    prefix: prefix.to_string(),
                    current_date: date,
                    file,
                });
            }
            Err(e) => {
                eprintln!("Logger: не удалось открыть {}: {}", path.display(), e);
            }
        }
    }

    /// Вывод лога с таймштампом. Пишет в stdout/GUI-канал и в файл (если включён).
    /// Устойчив к poisoned-mutex (если предыдущий держатель паниковал — берём lock через
    /// PoisonError::into_inner и пишем дальше, не пугаемся).
    pub fn log(message: &str) {
        let now = Local::now();
        let timestamp = now.format("%H:%M:%S");
        let log_message = format!("[{}] {}", timestamp, message);

        // 1) stdout / GUI-канал
        let sender = LOG_SENDER.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(ref tx) = *sender {
            let _ = tx.send(log_message.clone());
        } else {
            println!("{}", log_message);
        }
        drop(sender);

        // 2) файл (с ротацией по дате)
        let mut file_log = FILE_LOG.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(state) = file_log.as_mut() {
            let today = now.format("%Y-%m-%d").to_string();
            if today != state.current_date {
                let new_path = state.dir.join(format!("{}-{}.log", state.prefix, today));
                if let Ok(new_file) = OpenOptions::new().create(true).append(true).open(&new_path) {
                    state.file = new_file;
                    state.current_date = today;
                }
                // Если переоткрытие упало — продолжаем писать в старый, но дата в имени
                // расходится с реальностью. Это лучше, чем потерять лог.
            }
            let _ = writeln!(state.file, "{}", log_message);
            let _ = state.file.flush();
        }
    }

    /// Подробная строка журнала: пишется тем же путём, что и `log`, но только
    /// при уровне Debug. В файле помечена префиксом `[debug] ` после метки времени.
    pub fn debug(message: &str) {
        if Self::level() != LogLevel::Debug {
            return;
        }
        Self::log(&format!("[debug] {}", message));
    }

    /// Установить hook на panic'и: panic-сообщение (включая место и payload) пишется
    /// через Logger::log → попадает в файл-лог и stdout. После этого вызывается
    /// предыдущий hook (default = вывод в stderr с бэктрейсом, если RUST_BACKTRACE=1).
    /// Идемпотентен: повторные вызовы — no-op (через std::sync::Once).
    pub fn install_panic_hook() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                Logger::log(&format!("PANIC: {}", info));
                prev(info);
            }));
        });
    }

    /// Вывод разделителя
    pub fn separator() {
        Self::log(&"=".repeat(60));
    }

    /// Вывод тонкого разделителя
    pub fn thin_separator() {
        Self::log(&"-".repeat(60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Разбор уровня из строки настроек: регистр не важен, всё неизвестное — info.
    #[test]
    fn parses_log_level() {
        assert_eq!(LogLevel::parse("DEBUG"), LogLevel::Debug);
        assert_eq!(LogLevel::parse("debug"), LogLevel::Debug);
        assert_eq!(LogLevel::parse("info"), LogLevel::Info);
        assert_eq!(LogLevel::parse("мусор"), LogLevel::Info);
        assert_eq!(LogLevel::parse(""), LogLevel::Info);
    }

    /// Файловый лог с заданным префиксом: создаётся `<prefix>-YYYY-MM-DD.log`,
    /// строка Logger::log в него попадает, а Logger::debug — только на уровне Debug.
    /// Тест один на весь модуль: FILE_LOG и LOG_LEVEL — глобальное состояние процесса,
    /// поэтому в конце файловый лог снимается, а уровень возвращается в Info.
    #[test]
    fn init_file_named_creates_prefixed_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        Logger::init_file_named(log_dir.clone(), "gui-test");
        Logger::log("проверка записи в файл");

        // Уровень info: подробная строка в файл не попадает.
        Logger::set_level(LogLevel::Info);
        Logger::debug("подробность при info");
        // Уровень debug: попадает.
        Logger::set_level(LogLevel::Debug);
        Logger::debug("подробность при debug");

        let date = Local::now().format("%Y-%m-%d").to_string();
        let path = log_dir.join(format!("gui-test-{}.log", date));
        let content = std::fs::read_to_string(&path).expect("файл лога должен существовать");
        assert!(content.contains("проверка записи в файл"), "содержимое: {}", content);
        assert!(!content.contains("подробность при info"), "содержимое: {}", content);
        assert!(content.contains("подробность при debug"), "содержимое: {}", content);

        // Вернуть глобальное состояние: иначе остальные тесты продолжат писать
        // во временный каталог и на уровне debug.
        Logger::set_level(LogLevel::Info);
        *FILE_LOG.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}
