use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use crate::error::ExportError;
use crate::logging::Logger;

/// Декодирование вывода консольной утилиты Windows.
/// ibcmd на Windows с русской локалью пишет в CP866 (консольная)
/// или CP1251 в зависимости от настроек системы. Пробуем в порядке:
/// UTF-8 → CP866 → CP1251.
fn decode_console_output(bytes: &[u8]) -> String {
    // Пустой вход
    if bytes.is_empty() {
        return String::new();
    }
    // UTF-8: если нет символов замены — оставляем
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    // CP866 (русский DOS) — основная кодировка cmd.exe
    let (cow, _, had_errors) = encoding_rs::IBM866.decode(bytes);
    if !had_errors {
        return cow.into_owned();
    }
    // CP1251 (русский Windows) — fallback
    let (cow, _, _) = encoding_rs::WINDOWS_1251.decode(bytes);
    cow.into_owned()
}

/// Ключи, значение которых является паролем и не должно попадать в журнал.
/// Сравнение регистронезависимое: ibcmd пишет ключи в нижнем регистре,
/// но подстраховываемся.
const SECRET_KEYS: [&str; 3] = ["--password", "--db-pwd", "--pwd"];

/// Скрыть значения чувствительных параметров и собрать строку для журнала.
///
/// Журнал выгрузки живёт в файле долго, пароль пользователя ИБ и пароль СУБД
/// туда попадать не должны. Скрываются три формы записи:
///   * `--password=секрет`  → `--password=***`
///   * `--password секрет`  → `--password ***` (значение отдельным аргументом)
///   * `/Pсекрет`           → `/P***` (слитный ключ платформы 1cv8.exe)
///
/// Ключ и все остальные аргументы сохраняются как есть — команду по-прежнему
/// можно прочитать при разборе сбоя. Логин (`--user`, `/N`) не скрывается:
/// он не секрет и нужен для диагностики.
///
/// Пустое значение не маскируется — скрывать нечего.
pub fn mask_command(args: &[String]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(args.len());
    // Признак: предыдущий аргумент был ключом пароля, текущий — его значение.
    let mut next_is_secret = false;

    for arg in args {
        let masked = if next_is_secret {
            next_is_secret = false;
            if arg.is_empty() { arg.clone() } else { "***".to_string() }
        } else if let Some((key, value)) = arg.split_once('=') {
            if is_secret_key(key) && !value.is_empty() {
                format!("{}=***", key)
            } else {
                arg.clone()
            }
        } else if is_secret_key(arg) {
            // Значение придёт следующим аргументом.
            next_is_secret = true;
            arg.clone()
        } else if (arg.starts_with("/P") || arg.starts_with("/p")) && arg.len() > 2 {
            "/P***".to_string()
        } else {
            arg.clone()
        };

        // Кавычки — если в аргументе есть пробелы (как и раньше).
        if masked.contains(' ') {
            parts.push(format!("\"{}\"", masked));
        } else {
            parts.push(masked);
        }
    }

    parts.join(" ")
}

/// Является ли аргумент ключом пароля (без учёта регистра).
fn is_secret_key(key: &str) -> bool {
    SECRET_KEYS.iter().any(|k| key.eq_ignore_ascii_case(k))
}

/// Результат выполнения внешней команды
pub struct CommandResult {
    pub success: bool,
    pub return_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Запуск внешних процессов (только IBCMD)
pub struct ProcessRunner;

impl ProcessRunner {
    /// Запуск ibcmd-команды со стримингом stdout/stderr в Logger.
    /// Каждая строка вывода ibcmd сразу попадает в GUI-канал/файловый лог,
    /// а не копится до завершения процесса. Это критично для долгих
    /// `ibcmd config export --sync`, где между запуском и выводом
    /// могут быть минуты тишины.
    pub fn run(cmd: &[String]) -> Result<CommandResult, ExportError> {
        // Логируем команду (в кавычках — если содержит пробелы),
        // скрывая значения паролей: журнал живёт в файле долго.
        let cmd_str = mask_command(cmd);
        Logger::log("Выполнение команды:");
        Logger::log(&format!("  {}", cmd_str));
        Logger::log("Команда запущена, ожидание вывода...");

        let started = std::time::Instant::now();

        // stdin закрываем: если ibcmd попытается запросить ввод (например,
        // пароль) — получит EOF и упадёт с ошибкой, а не зависнет.
        // CREATE_NO_WINDOW = 0x08000000: иначе родителю-GUI-приложению
        // Windows-loader создаст консоль для каждого ibcmd-вызова.
        #[cfg(windows)]
        use std::os::windows::process::CommandExt;
        let mut builder = Command::new(&cmd[0]);
        builder
            .args(&cmd[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        builder.creation_flags(0x08000000);
        let mut child = builder.spawn()?;

        let stdout = child.stdout.take().expect("piped stdout не доступен");
        let stderr = child.stderr.take().expect("piped stderr не доступен");

        // Канал для агрегации вывода: каждый поток шлёт (stream_kind, строка).
        // Stream kind: 0 = stdout, 1 = stderr — пригодится для возврата в CommandResult.
        let (tx, rx) = mpsc::channel::<(u8, String)>();

        let tx_out = tx.clone();
        let stdout_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            // Читаем по строкам в байтовом виде (не lines() с UTF-8) и декодируем
            // на месте — ibcmd на Windows может писать в CP866/CP1251.
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        let line = decode_console_output(&buf);
                        let line = line.trim_end_matches(['\r', '\n']);
                        if !line.is_empty() {
                            Logger::log(&format!("  {}", line));
                            let _ = tx_out.send((0, line.to_string()));
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let tx_err = tx;
        let stderr_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        let line = decode_console_output(&buf);
                        let line = line.trim_end_matches(['\r', '\n']);
                        if !line.is_empty() {
                            Logger::log(&format!("  [stderr] {}", line));
                            let _ = tx_err.send((1, line.to_string()));
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Дожидаемся завершения процесса
        let status = child.wait()?;
        let return_code = status.code().unwrap_or(-1);

        // Дочитываем оставшийся вывод (потоки уже на финише, но wait не гарантирует
        // что мы прочитали всё)
        let _ = stdout_thread.join();
        let _ = stderr_thread.join();

        // Собираем агрегированный stdout/stderr из канала для возврата вызывающему
        let mut stdout_acc = String::new();
        let mut stderr_acc = String::new();
        for (kind, line) in rx.iter() {
            match kind {
                0 => { stdout_acc.push_str(&line); stdout_acc.push('\n'); }
                _ => { stderr_acc.push_str(&line); stderr_acc.push('\n'); }
            }
        }

        let elapsed = started.elapsed();
        let success = return_code == 0;
        if success {
            Logger::log(&format!(
                "✓ Команда выполнена успешно за {:.1} сек",
                elapsed.as_secs_f64()
            ));
        } else {
            Logger::log(&format!(
                "✗ Ошибка выполнения (код возврата: {}, время: {:.1} сек)",
                return_code, elapsed.as_secs_f64()
            ));
        }

        Ok(CommandResult {
            success,
            return_code,
            stdout: stdout_acc,
            stderr: stderr_acc,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::mask_command;

    /// Удобный помощник: &[&str] → Vec<String>
    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn masks_password_with_equals() {
        let out = mask_command(&v(&["ibcmd.exe", "--user=exchanges", "--password=abc"]));
        assert_eq!(out, "ibcmd.exe --user=exchanges --password=***");
    }

    #[test]
    fn masks_password_as_separate_arg() {
        let out = mask_command(&v(&["ibcmd.exe", "--password", "abc", "output"]));
        assert_eq!(out, "ibcmd.exe --password *** output");
    }

    #[test]
    fn masks_db_pwd_both_forms() {
        assert_eq!(
            mask_command(&v(&["ibcmd.exe", "--db-pwd=abc"])),
            "ibcmd.exe --db-pwd=***"
        );
        assert_eq!(
            mask_command(&v(&["ibcmd.exe", "--db-pwd", "abc"])),
            "ibcmd.exe --db-pwd ***"
        );
    }

    #[test]
    fn masks_short_pwd_key() {
        assert_eq!(mask_command(&v(&["ibcmd.exe", "--pwd=abc"])), "ibcmd.exe --pwd=***");
    }

    #[test]
    fn masks_platform_slash_p() {
        let out = mask_command(&v(&["1cv8.exe", "ENTERPRISE", "/Nvasya", "/Pabc"]));
        assert_eq!(out, "1cv8.exe ENTERPRISE /Nvasya /P***");
    }

    #[test]
    fn empty_password_stays_as_is() {
        // Скрывать нечего — ключ остаётся читаемым.
        let empty_key = format!("--{}=", "password");
        assert_eq!(mask_command(&v(&["ibcmd.exe", &empty_key])), "ibcmd.exe --password=");
        assert_eq!(mask_command(&v(&["ibcmd.exe", "--password", ""])), "ibcmd.exe --password ");
        // Одинокий `/P` без значения — не пароль, не трогаем.
        assert_eq!(mask_command(&v(&["1cv8.exe", "/P"])), "1cv8.exe /P");
    }

    #[test]
    fn masks_password_with_special_chars_and_spaces() {
        let out = mask_command(&v(&["ibcmd.exe", "--password=p@ss w0rd!#$%"]));
        assert_eq!(out, "ibcmd.exe --password=***");
        let out = mask_command(&v(&["1cv8.exe", "/Pп@роль с пробелом"]));
        assert_eq!(out, "1cv8.exe /P***");
    }

    #[test]
    fn login_and_other_args_untouched() {
        let out = mask_command(&v(&[
            "C:/Program Files/1cv8/bin/ibcmd.exe",
            "infobase",
            "config",
            "export",
            "--extension=Расш",
            "--db-server=sql-01",
            "--dbms=MSSQLServer",
            "--db-name=demo",
            "--user=export_user",
            "E:/export/extensions/Расш",
        ]));
        assert_eq!(
            out,
            "\"C:/Program Files/1cv8/bin/ibcmd.exe\" infobase config export \
             --extension=Расш --db-server=sql-01 --dbms=MSSQLServer --db-name=demo \
             --user=export_user E:/export/extensions/Расш"
        );
    }

    #[test]
    fn full_real_command_from_defect_report() {
        let out = mask_command(&v(&[
            "C:/Program Files/1cv8/8.3.27.2214/bin/ibcmd.exe",
            "infobase",
            "config",
            "export",
            "--extension=Имя",
            "--dbms=MSSQLServer",
            "--user=export_user",
            "--password=НастоящийПароль",
            "E:/export\\extensions\\Имя",
        ]));
        assert!(!out.contains("НастоящийПароль"), "пароль остался в строке журнала: {}", out);
        assert!(out.contains("--password=***"));
        assert!(out.contains("--user=export_user"));
    }
}
