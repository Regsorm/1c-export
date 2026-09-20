// Встраивание манифеста приложения в exe и гейт качества кода.
//
// Без манифеста импорт comctl32 связывается с версией 5.82, а nwg::init()
// (enable_visual_styles) подключает comctl32 6.0 контекстом активации уже на
// ходу — в процессе живут две версии, подклассы окон nwg зацикливаются
// в comctl32!DefSubclassProc и GUI виснет на старте.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    // Без этих строк cargo перезапускал бы build.rs только при правке .rc
    // и .manifest, и гейт не срабатывал бы на изменения исходников.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=rustfmt.toml");
    println!("cargo:rerun-if-env-changed=ONEC_EXPORT_SKIP_QUALITY_GATE");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=resources/1c-export.rc");
        println!("cargo:rerun-if-changed=resources/1c-export.manifest");
        embed_resource::compile("resources/1c-export.rc", embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }

    quality_gate();
}

/// Гейт качества: при каждой сборке прогоняет `cargo fmt --all -- --check`
/// и `cargo clippy --all-targets --all-features -- -D warnings`.
/// Нарушение роняет сборку; вывод обеих команд идёт в stderr build-скрипта,
/// и cargo печатает его целиком.
///
/// Пропустить разово — переменная окружения `ONEC_EXPORT_SKIP_QUALITY_GATE=1`.
fn quality_gate() {
    // clippy, запущенный отсюда, снова выполняет этот же build.rs —
    // страж обрывает рекурсию.
    if std::env::var_os("ONEC_EXPORT_QUALITY_GATE").is_some() {
        return;
    }
    if std::env::var_os("ONEC_EXPORT_SKIP_QUALITY_GATE").is_some() {
        println!(
            "cargo:warning=Проверка формата и clippy пропущена: задана ONEC_EXPORT_SKIP_QUALITY_GATE"
        );
        return;
    }

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR задаёт cargo"),
    );

    // Отдельный каталог артефактов: target-dir родителя занят его же
    // процессом, и общий каталог дал бы взаимную блокировку cargo.
    let gate_target_dir = gate_target_dir();

    run_gate(
        &cargo,
        &manifest_dir,
        "fmt",
        &["fmt", "--all", "--", "--check"],
        None,
        "форматирование (cargo fmt --all -- --check)",
    );
    run_gate(
        &cargo,
        &manifest_dir,
        "clippy",
        &[
            "clippy",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ],
        Some(&gate_target_dir),
        "clippy (-D warnings)",
    );
}

/// Каталог артефактов для clippy: `<target-dir>/quality-gate`.
///
/// OUT_DIR имеет вид `<target-dir>/<профиль>/build/<пакет>-<хеш>/out`,
/// поэтому корень — четыре уровня вверх.
fn gate_target_dir() -> PathBuf {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR задаёт cargo"));
    out_dir
        .ancestors()
        .nth(4)
        .map(|root| root.join("quality-gate"))
        .unwrap_or_else(|| out_dir.join("quality-gate"))
}

/// Запускает одну проверку. `subcommand` проверяется отдельно на наличие:
/// не установленный компонент — предупреждение, а не падение сборки.
fn run_gate(
    cargo: &OsStr,
    dir: &Path,
    subcommand: &str,
    args: &[&str],
    target_dir: Option<&Path>,
    what: &str,
) {
    if !subcommand_available(cargo, dir, subcommand) {
        println!(
            "cargo:warning=Проверка «{what}» пропущена: подкоманда cargo {subcommand} недоступна. \
             Установите компонент: rustup component add rustfmt clippy"
        );
        return;
    }

    let mut cmd = base_command(cargo, dir);
    cmd.args(args);
    if let Some(target_dir) = target_dir {
        cmd.env("CARGO_TARGET_DIR", target_dir);
    }

    match cmd.status() {
        Ok(status) if status.success() => {}
        Ok(status) => panic!(
            "Гейт качества не пройден: {what}, код возврата {:?}. \
             Замечания напечатаны выше. Исправьте их (для формата — cargo fmt --all) \
             либо соберите с ONEC_EXPORT_SKIP_QUALITY_GATE=1.",
            status.code()
        ),
        Err(err) => println!("cargo:warning=Проверку «{what}» запустить не удалось: {err}"),
    }
}

fn subcommand_available(cargo: &OsStr, dir: &Path, subcommand: &str) -> bool {
    base_command(cargo, dir)
        .args([subcommand, "--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Дочерний cargo запускается без обёрток родителя: RUSTC_WORKSPACE_WRAPPER
/// родителя указывает на clippy-driver, когда сборку начали через clippy,
/// и в дочернем процессе он лишний.
fn base_command(cargo: &OsStr, dir: &Path) -> Command {
    let mut cmd = Command::new(cargo);
    cmd.current_dir(dir)
        .env("ONEC_EXPORT_QUALITY_GATE", "1")
        .env_remove("RUSTC")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS");
    cmd
}
