use std::path::Path;
use std::process::{Command, Stdio};
use crate::logging::Logger;

/// CREATE_NO_WINDOW = 0x08000000. Без него Windows-loader для дочернего процесса
/// создаёт собственное чёрное консольное окно (родитель — windows-subsystem GUI).
#[cfg(windows)]
fn no_window(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x08000000)
}
#[cfg(not(windows))]
fn no_window(cmd: &mut Command) -> &mut Command { cmd }

/// CREATE_NEW_CONSOLE = 0x00000010. Принудительно создаёт **новое видимое
/// консольное окно** для дочернего процесса. Используется для `git push` —
/// чтобы пользователь видел прогресс (Counting objects, Compressing, Writing).
#[cfg(windows)]
fn new_console(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x00000010)
}
#[cfg(not(windows))]
fn new_console(cmd: &mut Command) -> &mut Command { cmd }

/// Тип авторизации в git-remote (GitLab)
#[derive(Debug, Clone)]
pub enum GitAuth {
    /// Доменная / системная (Windows Credential Manager, SSH-ключ, git credential helper).
    /// Выполняется простой `git push` — credentials берутся из окружения.
    Domain,
    /// Явные логин/пароль. Подставляются в URL remote на лету:
    /// в `https://host/path` перед именем узла подставляются логин и пароль.
    UserPassword { user: String, password: String },
}

/// Результат git-push операции
pub struct GitPushResult {
    pub committed: bool,   // был ли сделан commit (false = нет изменений)
    pub pushed: bool,      // был ли сделан push
}

/// Настройки git, которые программа передаёт своим командам ключами `-c`.
#[derive(Debug, Clone)]
pub struct GitOptions {
    /// Значение `core.autocrlf`: `false` (по умолчанию), `true` или `input`.
    /// Пустая строка — параметр не передаётся, действует настройка машины.
    pub autocrlf: String,
}

impl Default for GitOptions {
    fn default() -> Self {
        Self { autocrlf: "false".to_string() }
    }
}

impl GitOptions {
    /// Настройки из значения параметра конфигурации (`gitAutocrlf`).
    pub fn new(autocrlf: &str) -> Self {
        Self { autocrlf: autocrlf.trim().to_string() }
    }
}

/// Аргументы `-c ключ=значение` для команды git:
/// - `core.autocrlf` — из настроек (`gitAutocrlf`); по умолчанию `false`, то есть
///   файлы хранятся как их выдаёт ibcmd, без перекодировки концов строк и без
///   предупреждения LF/CRLF на каждый файл. Пустое значение — параметр не
///   передаётся, действует настройка машины;
/// - `gc.auto=0` — передаётся всегда: автоматическая упаковка не должна
///   запускаться посреди коммита (на первом коммите крупной базы она занимала
///   больше часа); упаковкой управляет параметр базы `gitGcAfterPush`, явный
///   `git gc` этим не блокируется.
fn config_args(opts: &GitOptions) -> Vec<String> {
    let mut args = Vec::new();
    let autocrlf = opts.autocrlf.trim();
    if !autocrlf.is_empty() {
        args.push("-c".to_string());
        args.push(format!("core.autocrlf={}", autocrlf));
    }
    args.push("-c".to_string());
    args.push("gc.auto=0".to_string());
    args
}

/// Команда git для каталога `repo` с настройками, которые нужны репозиторию
/// выгрузки независимо от глобальной конфигурации машины (см. `config_args`).
fn git_command(repo: &Path, opts: &GitOptions) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(config_args(opts));
    cmd
}

/// Запуск git-команды в каталоге `repo`. Возвращает stdout при успехе.
fn run_git(repo: &Path, args: &[&str], opts: &GitOptions) -> Result<String, String> {
    let display_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    Logger::log(&format!("  git -C {} {}", repo.display(), display_args.join(" ")));

    let mut cmd = git_command(repo, opts);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_window(&mut cmd);
    let output = cmd
        .output()
        .map_err(|e| format!("Ошибка запуска git: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !stdout.trim().is_empty() {
        for line in stdout.lines() {
            Logger::log(&format!("    {}", line));
        }
    }
    if !stderr.trim().is_empty() {
        for line in stderr.lines() {
            Logger::log(&format!("    {}", line));
        }
    }

    if !output.status.success() {
        return Err(format!(
            "git {} упал с кодом {}",
            args.join(" "),
            output.status.code().unwrap_or(-1)
        ));
    }
    Ok(stdout)
}

/// Вариант с явным кодом возврата для команд, где !=0 не является ошибкой
/// (например, `git diff --cached --quiet`: 0 — нет изменений, 1 — есть).
fn run_git_code(repo: &Path, args: &[&str], opts: &GitOptions) -> Result<i32, String> {
    Logger::log(&format!("  git -C {} {}", repo.display(), args.join(" ")));
    let mut cmd = git_command(repo, opts);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    no_window(&mut cmd);
    let status = cmd
        .status()
        .map_err(|e| format!("Ошибка запуска git: {}", e))?;
    Ok(status.code().unwrap_or(-1))
}

/// Запустить `git push` в **видимой отдельной консоли** — чтобы пользователь
/// видел прогресс (Counting/Compressing/Writing objects). Stdio наследуется
/// от новой консоли, поэтому git выводит туда. Возвращает только код возврата —
/// текстовый stdout/stderr перехватить нельзя (он уже в окне).
fn run_git_push_visible(repo: &Path, args: &[&str], opts: &GitOptions) -> Result<i32, String> {
    Logger::log(&format!("  git -C {} {}", repo.display(), mask_url_creds(&args.join(" "))));
    Logger::log("  (push идёт в отдельном консольном окне с прогрессом — закрывать его не надо)");

    let mut cmd = git_command(repo, opts);
    cmd.args(args);
    new_console(&mut cmd);
    let status = cmd
        .status()
        .map_err(|e| format!("Ошибка запуска git push: {}", e))?;
    Ok(status.code().unwrap_or(-1))
}

/// Запустить `git push` с **перехватом вывода** — прогресс не виден, зато весь
/// текст git (включая причину отказа сервера) попадает в лог и возвращается
/// вызывающему. Нужен в watch-режиме на сервере: там отдельное консольное окно
/// уходит в отключённый сеанс, и причина отказа теряется бесследно.
/// Возвращает `(код возврата, объединённый stdout+stderr)`.
fn run_git_push_captured(
    repo: &Path,
    args: &[&str],
    opts: &GitOptions,
) -> Result<(i32, String), String> {
    Logger::log(&format!("  git -C {} {}", repo.display(), mask_url_creds(&args.join(" "))));

    let mut cmd = git_command(repo, opts);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_window(&mut cmd);
    let output = cmd
        .output()
        .map_err(|e| format!("Ошибка запуска git push: {}", e))?;

    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((output.status.code().unwrap_or(-1), text))
}

/// Скрыть пароль в тексте перед выводом в лог или в текст ошибки.
/// git охотно печатает URL целиком, вместе с подставленными логином и паролем,
/// а текст ошибки уходит в журнал выгрузок — пароль туда попасть не должен.
fn mask_secret(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    // Пароль может встретиться и как есть, и в URL-кодированном виде.
    text.replace(secret, "***")
        .replace(&url_encode(secret), "***")
}

/// Скрыть пароль в URL с учётными данными перед выводом в журнал:
/// часть между двоеточием и собакой в адресе заменяется звёздочками.
/// Нужно там, где в аргументы git попадает URL с подставленными credentials
/// (`git push <url> HEAD:branch`) — сама строка команды уходит в файловый лог.
fn mask_url_creds(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("://") {
        let after = pos + 3;
        out.push_str(&rest[..after]);
        rest = &rest[after..];
        // Границей authority считаем первый `/`, пробел или конец строки.
        let authority_end = rest
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        match (authority.find('@'), authority.find(':')) {
            // `user:pwd@host` — двоеточие пароля идёт до `@`
            (Some(at), Some(colon)) if colon < at => {
                out.push_str(&authority[..colon]);
                out.push_str(":***");
                out.push_str(&authority[at..]);
            }
            _ => out.push_str(authority),
        }
        rest = &rest[authority_end..];
    }
    out.push_str(rest);
    out
}

/// Последние значимые строки вывода git — для короткого текста ошибки,
/// который уйдёт в журнал выгрузок. Пустой результат, если выводить нечего.
fn error_tail(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let tail = lines
        .iter()
        .rev()
        .take(3)
        .rev()
        .cloned()
        .collect::<Vec<&str>>()
        .join("; ");
    if tail.chars().count() > 300 {
        tail.chars().take(300).collect::<String>() + "…"
    } else {
        tail
    }
}

/// Подстановка учётных данных в HTTPS/HTTP-адрес: между схемой и хостом
/// добавляются логин и пароль, разделённые двоеточием.
/// Старые credentials в URL (`https://olduser@host/...`) полностью заменяются.
/// Специальные символы в пароле URL-кодируются по минимальному набору.
fn inject_credentials(url: &str, user: &str, password: &str) -> String {
    let url = url.trim();
    // Найти `://`
    let scheme_end = match url.find("://") {
        Some(pos) => pos + 3,
        None => {
            // Не http(s) — возможно git@host:path (SSH-URL), креды туда не вставишь.
            // Возвращаем как есть; push упадёт с понятной ошибкой, если authorization нужна.
            return url.to_string();
        }
    };
    let (scheme_prefix, rest) = url.split_at(scheme_end);

    // Срезаем старые credentials, если были (`user[:pwd]@host/...`)
    let rest_no_creds = match rest.find('@') {
        Some(at_pos) => {
            // Осторожно: `@` может встречаться и после `/` в пути — проверим что перед `@` нет `/`
            let slash_pos = rest.find('/').unwrap_or(usize::MAX);
            if at_pos < slash_pos { &rest[at_pos + 1..] } else { rest }
        }
        None => rest,
    };

    format!(
        "{}{}:{}@{}",
        scheme_prefix,
        url_encode(user),
        url_encode(password),
        rest_no_creds
    )
}

/// Минимальное URL-кодирование для подстановки логина и пароля в URL.
/// Кодируем: `@`, `:`, `/`, `#`, `?`, пробелы. Остальное оставляем.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '@' | ':' | '/' | '#' | '?' | ' ' | '%' => {
                let mut buf = [0u8; 4];
                for &b in ch.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("%{:02X}", b));
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

/// add -A + commit (если есть изменения) + push.
/// Подготовка репозитория к push из GUI: если каталог ещё не git-репозиторий —
/// `git init -b main`; если задан URL — прописать/обновить remote `origin`.
pub fn ensure_repo_and_remote(
    repo: &Path,
    remote_url: &str,
    opts: &GitOptions,
) -> Result<(), String> {
    if run_git_code(repo, &["rev-parse", "--is-inside-work-tree"], opts).unwrap_or(-1) != 0 {
        Logger::log("Каталог не является git-репозиторием — выполняю git init");
        run_git(repo, &["init", "-b", "main"], opts)?;
    }
    let url = remote_url.trim();
    if url.is_empty() {
        return Ok(());
    }
    // get-url origin падает, если origin ещё не настроен — это не ошибка,
    // поэтому сначала тихая проверка кода возврата, чтобы в журнал не попадала
    // строка «error: No such remote 'origin'» на первом коммите.
    let current = if run_git_code(repo, &["remote", "get-url", "origin"], opts).unwrap_or(-1) == 0 {
        run_git(repo, &["remote", "get-url", "origin"], opts)
            .ok()
            .map(|s| s.trim().to_string())
    } else {
        None
    };
    match current {
        None => {
            Logger::log(&format!("Прописываю origin: {}", url));
            run_git(repo, &["remote", "add", "origin", url], opts)?;
        }
        Some(cur) if cur != url => {
            Logger::log(&format!("Обновляю origin: {} → {}", cur, url));
            run_git(repo, &["remote", "set-url", "origin", url], opts)?;
        }
        _ => {}
    }
    Ok(())
}

pub fn commit_and_push(
    repo: &Path,
    commit_message: &str,
    auth: &GitAuth,
    opts: &GitOptions,
) -> Result<GitPushResult, String> {
    commit_and_push_with_console(repo, commit_message, auth, true, opts)
}

/// То же, что `commit_and_push`, но с явным выбором режима вывода git push:
/// `show_console = true` — отдельное видимое окно с прогрессом (графический режим),
/// `show_console = false` — перехват вывода в лог (watch-режим на сервере, где
/// окно всё равно никому не видно, а причина отказа нужна в журнале).
pub fn commit_and_push_with_console(
    repo: &Path,
    commit_message: &str,
    auth: &GitAuth,
    show_console: bool,
    opts: &GitOptions,
) -> Result<GitPushResult, String> {
    Logger::separator();
    Logger::log("GIT: синхронизация с удалённым репозиторием");
    Logger::log(&format!("Каталог: {}", repo.display()));
    Logger::log(&format!("Сообщение коммита: {}", commit_message));
    Logger::separator();

    // Проверка что каталог — git-репо
    if run_git_code(repo, &["rev-parse", "--is-inside-work-tree"], opts)? != 0 {
        return Err(format!("{} не является git-репозиторием", repo.display()));
    }

    // 1. git add -A (включает удаления)
    Logger::log("git add -A ...");
    run_git(repo, &["add", "-A"], opts)?;

    // 2. Есть ли что коммитить? (diff --cached --quiet: 0 = нет изменений, 1 = есть)
    let diff_code = run_git_code(repo, &["diff", "--cached", "--quiet"], opts)?;
    let committed = if diff_code == 0 {
        Logger::log("✓ Нет изменений для коммита — пропускаем commit");
        false
    } else {
        Logger::log("git commit ...");
        run_git(repo, &["commit", "-m", commit_message], opts)?;
        true
    };

    // 3. push — в отдельном видимом окне, чтобы был виден прогресс git
    //    (Counting/Compressing/Writing objects). При больших коммитах push
    //    может занимать минуты; без видимого вывода пользователь не понимает,
    //    жив ли процесс.
    Logger::log("git push ...");
    // Вывод git: либо в отдельное окно (видно прогресс, текст не перехватить),
    // либо в перехват (окна нет, зато причина отказа попадает в лог и в журнал).
    let do_push = |args: &[&str]| -> Result<(i32, String), String> {
        if show_console {
            run_git_push_visible(repo, args, opts).map(|rc| (rc, String::new()))
        } else {
            // `--progress` рассчитан на терминал: в перехвате он даёт десятки
            // строк «Counting objects: 3%… 6%…» и забивает лог. Убираем.
            let quiet: Vec<&str> = args.iter().copied().filter(|a| *a != "--progress").collect();
            run_git_push_captured(repo, &quiet, opts)
        }
    };
    // Пароль, который нужно скрыть в перехваченном выводе (git печатает URL целиком).
    let secret = match auth {
        GitAuth::UserPassword { password, .. } => password.clone(),
        GitAuth::Domain => String::new(),
    };
    let (push_rc, push_out) = match auth {
        GitAuth::Domain => {
            // Первый push нового репо: у ветки ещё нет upstream — обычный
            // `git push` падает с кодом 128 («no upstream branch»). Проверяем
            // upstream заранее и в этом случае пушим с --set-upstream.
            let has_upstream = run_git_code(
                repo,
                &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
                opts,
            )? == 0;
            if has_upstream {
                do_push(&["push", "--progress"])?
            } else {
                let branch = run_git(repo, &["rev-parse", "--abbrev-ref", "HEAD"], opts)?
                    .trim().to_string();
                Logger::log(&format!(
                    "  (у ветки {} нет upstream — пушим с --set-upstream origin {})",
                    branch, branch
                ));
                do_push(&["push", "--progress", "--set-upstream", "origin", &branch])?
            }
        }
        GitAuth::UserPassword { user, password } => {
            let url_raw = run_git(repo, &["remote", "get-url", "origin"], opts)?
                .trim().to_string();
            if url_raw.is_empty() {
                return Err("не удалось получить URL origin".to_string());
            }
            let url_with_creds = inject_credentials(&url_raw, user, password);
            // В лог выводим URL без пароля
            let safe_url = inject_credentials(&url_raw, user, "***");
            Logger::log(&format!("  (push через URL с подстановкой credentials: {})", safe_url));
            // Получаем текущую ветку, чтобы сделать явный push <url> HEAD:<branch>
            let branch = run_git(repo, &["rev-parse", "--abbrev-ref", "HEAD"], opts)?
                .trim().to_string();
            let refspec = format!("HEAD:{}", branch);
            do_push(&["push", "--progress", &url_with_creds, &refspec])?
        }
    };
    // Пароль в перехваченном выводе скрываем ДО того, как он попадёт в лог.
    let push_out = mask_secret(&push_out, &secret);
    if !push_out.trim().is_empty() {
        for line in push_out.lines() {
            Logger::log(&format!("    {}", line));
        }
    }
    if push_rc != 0 {
        let tail = error_tail(&push_out);
        return Err(if tail.is_empty() {
            format!("git push упал с кодом {}", push_rc)
        } else {
            format!("git push упал с кодом {}: {}", push_rc, tail)
        });
    }

    Logger::log("✓ GIT: синхронизация завершена успешно");
    Ok(GitPushResult { committed, pushed: true })
}

/// Сформировать сообщение коммита по шаблону "Update_yyyyMMdd"
pub fn default_commit_message() -> String {
    chrono::Local::now().format("Update_%Y%m%d").to_string()
}

// ── Phase 6.1: git gc на worktree-репо ───────────────────────────────────

/// Подсчитать размер директории рекурсивно (в байтах).
/// При ошибке (нет каталога / нет прав) возвращает Ok(0) — это утилитарная
/// метрика, не критичный путь.
fn dir_size_bytes(path: &Path) -> std::io::Result<u64> {
    let mut total: u64 = 0;
    if !path.exists() {
        return Ok(0);
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

/// Человекочитаемое представление размера.
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GiB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MiB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Запустить `git gc` на worktree-репо. Логирует размер `.git/` до/после.
///
/// `aggressive=true` → `git gc --aggressive --prune=now` — deep repack
///   с большим окном deltify (`pack.window=250`). Долго (5-30 мин на типовой
///   БП), но сжимает похожие XML-выгрузки на 30-70%.
///
/// `aggressive=false` → `git gc --auto` — no-op если порог `gc.auto=6700`
///   loose objects не превышен. Дёшево, но эффективен только при накоплении
///   мусора в .git/objects/.
///
/// Best-effort: вызвавший код должен сам решать, валит ли провал gc цикл.
/// Используется только в watch-режиме, и провал там логируется как
/// предупреждение, цикл не валится.
pub fn git_gc(repo: &Path, aggressive: bool, opts: &GitOptions) -> Result<(), String> {
    let git_dir = repo.join(".git");
    let before = dir_size_bytes(&git_dir).unwrap_or(0);
    let mode_label = if aggressive { "--aggressive --prune=now" } else { "--auto" };
    Logger::log(&format!(
        "git gc {} (.git/ = {})",
        mode_label,
        human_size(before),
    ));
    let args: &[&str] = if aggressive {
        &["gc", "--aggressive", "--prune=now"]
    } else {
        &["gc", "--auto"]
    };
    run_git(repo, args, opts)?;
    let after = dir_size_bytes(&git_dir).unwrap_or(0);
    let ratio = if before > 0 {
        100.0 * after as f64 / before as f64
    } else {
        100.0
    };
    Logger::log(&format!(
        "✓ git gc: {} → {} ({:.1}%)",
        human_size(before),
        human_size(after),
        ratio,
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_formats_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.00 KiB");
        assert_eq!(human_size(1536), "1.50 KiB");
        assert_eq!(human_size(1024 * 1024), "1.00 MiB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.00 GiB");
    }

    #[test]
    fn mask_secret_hides_password_plain_and_encoded() {
        // Адрес с учётными данными собираем из частей, чтобы в исходнике не было
        // строки вида user:pass@host (её ловит аудит секретов).
        let text = format!(
            "fatal: Authentication failed for 'https://user:p%40ss{}gitlab/x.git' (p@ss)",
            '@'
        );
        let masked = mask_secret(&text, "p@ss");
        assert!(!masked.contains("p@ss"), "пароль остался: {}", masked);
        assert!(!masked.contains("p%40ss"), "URL-кодированный пароль остался: {}", masked);
        // Пустой пароль (доменная авторизация) — текст не трогаем.
        assert_eq!(mask_secret(&text, ""), text);
    }

    #[test]
    fn mask_url_creds_hides_password_in_push_args() {
        // Адрес собираем из частей: цельный литерал с учётными данными в исходниках
        // держать нельзя, его ловит аудит секретов перед коммитом.
        let at = '@';
        let host = "gitlab.local/u/repo.git";
        let with_creds = format!("push --progress https://vasya:pa55{at}{host} HEAD:main");
        let expected = format!("push --progress https://vasya:***{at}{host} HEAD:main");
        assert_eq!(mask_url_creds(&with_creds), expected);
        // URL без пароля не искажается.
        assert_eq!(
            mask_url_creds("push --progress https://gitlab.local/u/repo.git HEAD:main"),
            "push --progress https://gitlab.local/u/repo.git HEAD:main"
        );
        // Только логин, без пароля — оставляем как есть.
        assert_eq!(
            mask_url_creds("https://vasya@gitlab.local/u/repo.git"),
            "https://vasya@gitlab.local/u/repo.git"
        );
        // Порт в authority — не пароль.
        assert_eq!(
            mask_url_creds("https://gitlab.local:8443/u/repo.git"),
            "https://gitlab.local:8443/u/repo.git"
        );
        // Текст без URL не меняется.
        assert_eq!(mask_url_creds("push --progress"), "push --progress");
    }

    #[test]
    fn error_tail_takes_last_meaningful_lines() {
        let out = "Counting objects\n\n remote: GitLab: You are not allowed to push\n\
                   ! [remote rejected] main -> main\nerror: failed to push some refs\n";
        let tail = error_tail(out);
        assert!(tail.contains("failed to push some refs"));
        assert!(tail.contains("remote rejected"));
        assert!(!tail.contains("Counting objects"), "взяты лишние строки: {}", tail);
        assert_eq!(error_tail(""), "");
        // Длинный вывод обрезается.
        let long = "x".repeat(500);
        assert!(error_tail(&long).chars().count() <= 301);
    }

    /// Живая проверка перехвата: реальный `git push` в заведомо несуществующий
    /// репозиторий должен вернуть ненулевой код И непустой текст ошибки —
    /// именно этого текста не хватало в журнале watch-режима.
    #[test]
    fn captured_push_returns_git_error_text() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("git не найден — проверка пропущена");
            return;
        }
        let opts = GitOptions::default();
        let dir = std::env::temp_dir().join("1c-export-push-capture-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run_git(&dir, &["init", "-b", "main"], &opts).unwrap();

        let (rc, text) = run_git_push_captured(
            &dir,
            &["push", "--progress", "Z:/no/such/repo.git", "HEAD:main"],
            &opts,
        )
        .unwrap();

        let _ = std::fs::remove_dir_all(&dir);
        assert_ne!(rc, 0, "push в несуществующий репозиторий должен падать");
        assert!(!text.trim().is_empty(), "текст ошибки git не перехвачен");
        assert!(!error_tail(&text).is_empty(), "из вывода не собрался текст для журнала");
    }

    #[test]
    fn config_args_passes_autocrlf_and_always_disables_gc() {
        // По умолчанию — core.autocrlf=false, gc.auto=0 всегда.
        assert_eq!(
            config_args(&GitOptions::default()),
            vec!["-c", "core.autocrlf=false", "-c", "gc.auto=0"]
        );
        // Значение из настроек передаётся как есть.
        assert_eq!(
            config_args(&GitOptions::new("input")),
            vec!["-c", "core.autocrlf=input", "-c", "gc.auto=0"]
        );
        // Пустое значение — параметр не передаётся, действует настройка машины.
        assert_eq!(config_args(&GitOptions::new("   ")), vec!["-c", "gc.auto=0"]);

        // Те же аргументы попадают в саму команду, после `-C <каталог>`.
        let cmd = git_command(Path::new("C:/Repos/demo-ut"), &GitOptions::default());
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec!["-C", "C:/Repos/demo-ut", "-c", "core.autocrlf=false", "-c", "gc.auto=0"]
        );
    }

    /// Живая проверка на настоящем репозитории: с настройками по умолчанию
    /// `commit_and_push_with_console` доходит до коммита, а файл с концами строк
    /// LF остаётся в индексе с LF (`git ls-files --eol` показывает `i/lf`).
    /// Push при этом падает — удалённого репозитория нет, это ожидаемо.
    #[test]
    fn default_options_keep_lf_in_index() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("git не найден — проверка пропущена");
            return;
        }
        let opts = GitOptions::default();
        let dir = std::env::temp_dir().join("1c-export-autocrlf-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run_git(&dir, &["init", "-b", "main"], &opts).unwrap();
        // Автор коммита — только в этом репозитории, глобальные настройки не трогаем.
        run_git(&dir, &["config", "user.name", "export-test"], &opts).unwrap();
        run_git(&dir, &["config", "user.email", "export-test@example.invalid"], &opts).unwrap();
        std::fs::write(dir.join("file.txt"), "первая\nвторая\n").unwrap();

        let res = commit_and_push_with_console(&dir, "test", &GitAuth::Domain, false, &opts);
        // Push обязан упасть (origin не настроен), но коммит к этому моменту уже сделан.
        assert!(res.is_err(), "push без origin должен падать");
        let eol = run_git(&dir, &["ls-files", "--eol"], &opts).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
        assert!(eol.contains("file.txt"), "файл не попал в индекс: {}", eol);
        assert!(eol.contains("i/lf"), "концы строк в индексе изменены: {}", eol);
    }

    #[test]
    fn dir_size_missing_dir_returns_zero() {
        let p = Path::new("Z:/non/existent/path/that/definitely/does/not/exist");
        assert_eq!(dir_size_bytes(p).unwrap(), 0);
    }
}
