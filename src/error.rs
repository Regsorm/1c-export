use thiserror::Error;

/// Ошибки приложения выгрузки 1С
#[derive(Error, Debug)]
pub enum ExportError {
    #[error("Ошибка конфигурации: {0}")]
    Config(String),

    #[error("Файл не найден: {0}")]
    FileNotFound(String),

    #[error("Ошибка выполнения команды 1С (код {code}): {message}")]
    CommandFailed { code: i32, message: String },

    #[error("Ошибка ввода-вывода: {0}")]
    Io(#[from] std::io::Error),

    #[error("Ошибка JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Платформа 1С не найдена: {0}")]
    PlatformNotFound(String),

    #[error("IBCMD не найден: {0}")]
    IbcmdNotFound(String),

    /// Ошибки работы с MSSQL (tiberius) при выгрузке справочника
    /// ДополнительныеОтчетыИОбработки
    #[error("Ошибка MSSQL: {0}")]
    Sql(String),

    /// Ошибки распаковки поля ХранилищеОбработки (ValueStorage):
    /// неизвестный формат заголовка, битый DEFLATE, несовпадение MD5
    /// с реквизитом КонтрольнаяСумма БСП и т.п.
    #[error("Ошибка распаковки ХранилищаОбработки: {0}")]
    ValueStorage(String),
}
