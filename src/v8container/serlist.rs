//! Парсер 1С-сериализованного списка («скобкофайл»).
//!
//! Формат, в котором 1С хранит файлы-описатели метаданных, формы, СКД, макеты:
//!
//! ```text
//! {value, value, {nested, "string""with""quotes", 42}, "text"}
//! ```
//!
//! Грамматика:
//! - `list   := '{' value (',' value)* '}'`
//! - `value  := list | string | raw`
//! - `string := '"' chars '"'` — внутри удвоение `""` экранирует `"`. Многострочные
//!   строки (с `\n` внутри) разрешены как есть.
//! - `raw    := любая последовательность до `,`, `}`, `{` (числа, UUID,
//!   идентификаторы — на phase 2 не различаем, оставляем как `String`).
//!
//! Эталонная семантика — `saby/v8unpack/json_container_decoder.py`. Реализация
//! полностью своя (char-by-char итератор без state-машины с режимами).
//!
//! Phase 3 будет извлекать конкретные поля по индексам / типам — здесь же только
//! универсальный AST.

use crate::v8container::error::{Result, V8ContainerError};

/// AST-узел сериализованного списка.
#[derive(Debug, Clone, PartialEq)]
pub enum V8Value {
    /// Список `{a, b, c}`. Может быть пустым.
    List(Vec<V8Value>),
    /// Строка в кавычках `"..."` (без самих кавычек). Удвоение `""` уже декодировано.
    Str(String),
    /// Произвольный «голый» токен — число, UUID, имя, булево. Trim'нут от пробелов.
    Raw(String),
}

impl V8Value {
    pub fn as_list(&self) -> Option<&[V8Value]> {
        match self {
            V8Value::List(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            V8Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_raw(&self) -> Option<&str> {
        match self {
            V8Value::Raw(s) => Some(s),
            _ => None,
        }
    }

    /// Если list — взять i-й элемент.
    pub fn get(&self, i: usize) -> Option<&V8Value> {
        self.as_list().and_then(|v| v.get(i))
    }

    /// Навигация по дереву через массив индексов: `value.path(&[0, 3, 1])`.
    pub fn path(&self, indices: &[usize]) -> Option<&V8Value> {
        let mut cur = self;
        for &i in indices {
            cur = cur.get(i)?;
        }
        Some(cur)
    }

    /// Если list, длина; иначе None.
    pub fn len(&self) -> Option<usize> {
        self.as_list().map(|v| v.len())
    }

    /// Преобразовать в `serde_json::Value` для pretty-print диф-френдли вывода.
    ///
    /// - `List` → `Array`
    /// - `Str` → `String` как есть
    /// - `Raw` распознаётся: пустая строка → `Null`, `Истина`/`Ложь` → `Bool`,
    ///   парсится как `i64`/`f64` → `Number`, иначе `String` (UUID, идентификаторы).
    pub fn to_json_value(&self) -> serde_json::Value {
        match self {
            V8Value::List(items) => {
                serde_json::Value::Array(items.iter().map(V8Value::to_json_value).collect())
            }
            V8Value::Str(s) => serde_json::Value::String(s.clone()),
            V8Value::Raw(s) => raw_to_json(s),
        }
    }
}

fn raw_to_json(s: &str) -> serde_json::Value {
    if s.is_empty() {
        return serde_json::Value::Null;
    }
    // Булевы значения 1С: "Истина"/"Ложь" — но в большинстве serialized_list'ов
    // они кодируются как 0/1 в Raw, а не как имена. Но если попадётся имя —
    // признаем.
    if s == "Истина" || s == "True" {
        return serde_json::Value::Bool(true);
    }
    if s == "Ложь" || s == "False" {
        return serde_json::Value::Bool(false);
    }
    // UUID-формат — оставим как String чтобы не путать с числами.
    if s.len() == 36 && s.matches('-').count() == 4 {
        return serde_json::Value::String(s.to_string());
    }
    // Целое — i64.
    if let Ok(n) = s.parse::<i64>() {
        return serde_json::Value::Number(n.into());
    }
    // Дробное — f64 (если влезает в JSON Number).
    if let Ok(n) = s.parse::<f64>() {
        if let Some(num) = serde_json::Number::from_f64(n) {
            return serde_json::Value::Number(num);
        }
    }
    serde_json::Value::String(s.to_string())
}

/// Распарсить строку как сериализованный список 1С. Возвращает корневой `V8Value`
/// (обычно — `List`).
pub fn parse(text: &str) -> Result<V8Value> {
    let mut p = Parser::new(text);
    p.skip_whitespace();
    let value = p.parse_value()?;
    p.skip_whitespace();
    if !p.eof() {
        return Err(V8ContainerError::BadBlockHeader {
            offset: p.byte_pos() as u64,
            message: format!(
                "trailing content after root value: {:?}",
                p.peek().unwrap_or('?')
            ),
        });
    }
    Ok(value)
}

/// Распарсить байты, автоматически определяя кодировку: BOM (UTF-8/UTF-16LE/UTF-16BE)
/// → fallback на windows-1251.
///
/// 1С сохраняет файлы-описатели либо в UTF-8 with BOM (`utf-8-sig`), либо в
/// windows-1251 — см. `saby/v8unpack/helper.py::detect_by_bom`.
pub fn parse_bytes_utf8_or_1251(bytes: &[u8]) -> Result<V8Value> {
    let text = decode_text(bytes)?;
    let text = normalize_universal_newlines(&text);
    parse(&text)
}

/// Нормализация переводов строк к каноническому `\n` (universal-newline).
/// Python-эталон (`saby`) читает файлы-описатели в текстовом режиме
/// (`open(path, 'r')`), который транслирует `\r\n`/одиночный `\r` → `\n` ещё
/// до парсинга — в т.ч. внутри строковых литералов (например, base64-блоб в
/// реквизитах формы, перенесённый на несколько строк). Наш байтовый парсер
/// иначе сохранил бы такой `\r` буквально — расхождение с golden.
fn normalize_universal_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Распознавание BOM и декодирование в `String`.
fn decode_text(bytes: &[u8]) -> Result<String> {
    // UTF-8 BOM
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return std::str::from_utf8(&bytes[3..])
            .map(str::to_string)
            .map_err(|_| V8ContainerError::BadFilename);
    }
    // UTF-16LE BOM
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let (cow, _, had_errors) = encoding_rs::UTF_16LE.decode(&bytes[2..]);
        if had_errors {
            return Err(V8ContainerError::BadFilename);
        }
        return Ok(cow.into_owned());
    }
    // UTF-16BE BOM
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let (cow, _, had_errors) = encoding_rs::UTF_16BE.decode(&bytes[2..]);
        if had_errors {
            return Err(V8ContainerError::BadFilename);
        }
        return Ok(cow.into_owned());
    }
    // Без BOM — пробуем UTF-8, иначе windows-1251.
    if let Ok(s) = std::str::from_utf8(bytes) {
        return Ok(s.to_string());
    }
    let (cow, _, had_errors) = encoding_rs::WINDOWS_1251.decode(bytes);
    if had_errors {
        return Err(V8ContainerError::BadFilename);
    }
    Ok(cow.into_owned())
}

// ─── parser ─────────────────────────────────────────────────────────────────

struct Parser<'a> {
    text: &'a str,
    iter: std::str::CharIndices<'a>,
    next: Option<(usize, char)>,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        let mut iter = text.char_indices();
        let next = iter.next();
        Self { text, iter, next }
    }

    fn peek(&self) -> Option<char> {
        self.next.map(|(_, c)| c)
    }

    fn byte_pos(&self) -> usize {
        self.next.map(|(p, _)| p).unwrap_or(self.text.len())
    }

    fn advance(&mut self) {
        self.next = self.iter.next();
    }

    fn eof(&self) -> bool {
        self.next.is_none()
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn parse_value(&mut self) -> Result<V8Value> {
        self.skip_whitespace();
        match self.peek() {
            Some('{') => self.parse_list(),
            Some('"') => self.parse_string(),
            Some(_) => self.parse_raw(),
            None => Err(V8ContainerError::BadBlockHeader {
                offset: self.byte_pos() as u64,
                message: "unexpected EOF, expected value".into(),
            }),
        }
    }

    fn parse_list(&mut self) -> Result<V8Value> {
        debug_assert_eq!(self.peek(), Some('{'));
        self.advance(); // съесть '{'
        let mut items = Vec::new();
        loop {
            self.skip_whitespace();
            // Сразу '}' — тело списка пусто. saby `JsonContainerDecoder._end_value`:
            // при закрытии объекта значение добавляется ВСЕГДА, кроме случая, когда
            // непосредственно перед этим закрылся вложенный подобъект (защита от
            // фантомного пустого «хвоста» после вложенного списка). Для реально пустого
            // тела `{}` это условие не выполняется — значит добавляется ПУСТАЯ СТРОКА:
            // `{}` разбирается как список из ОДНОГО элемента `Raw("")`, а не как пустой
            // список (подтверждено эталонным выводом v8unpack.exe на обычных формах).
            if self.peek() == Some('}') {
                self.advance();
                if items.is_empty() {
                    return Ok(V8Value::List(vec![V8Value::Raw(String::new())]));
                }
                return Ok(V8Value::List(items));
            }
            // проверка на ',' в начале (например, при пустом значении в начале списка)
            // в 1С ',' между значениями. Пустое значение допустимо — парсится как Raw("").
            // Поэтому здесь сразу читаем value.
            let v = self.parse_value()?;
            items.push(v);
            self.skip_whitespace();
            match self.peek() {
                Some(',') => self.advance(),
                Some('}') => {
                    self.advance();
                    return Ok(V8Value::List(items));
                }
                Some(c) => {
                    return Err(V8ContainerError::BadBlockHeader {
                        offset: self.byte_pos() as u64,
                        message: format!("expected ',' or '}}', got {c:?}"),
                    });
                }
                None => {
                    return Err(V8ContainerError::BadBlockHeader {
                        offset: self.byte_pos() as u64,
                        message: "unterminated list, expected '}'".into(),
                    });
                }
            }
        }
    }

    fn parse_string(&mut self) -> Result<V8Value> {
        debug_assert_eq!(self.peek(), Some('"'));
        self.advance(); // съесть открывающую '"'
        let mut buf = String::new();
        loop {
            match self.peek() {
                Some('"') => {
                    self.advance();
                    // Удвоенная кавычка `""` = одна кавычка внутри строки.
                    if self.peek() == Some('"') {
                        buf.push('"');
                        self.advance();
                    } else {
                        return Ok(V8Value::Str(buf));
                    }
                }
                Some(c) => {
                    buf.push(c);
                    self.advance();
                }
                None => {
                    return Err(V8ContainerError::BadBlockHeader {
                        offset: self.byte_pos() as u64,
                        message: "unterminated string".into(),
                    });
                }
            }
        }
    }

    fn parse_raw(&mut self) -> Result<V8Value> {
        let start = self.byte_pos();
        while let Some(c) = self.peek() {
            if matches!(c, ',' | '}' | '{') {
                break;
            }
            self.advance();
        }
        let end = self.byte_pos();
        // Длинные raw-токены (например, `#base64:...` в реквизитах формы)
        // 1С переносит на несколько физических строк внутри самого файла —
        // это формальный перенос, а не значащий символ (в отличие от строк в
        // кавычках, где `\n` может быть частью текста, см. `parse_string`).
        // Убираем встроенные переводы строк целиком, не только по краям.
        let raw: String = self.text[start..end]
            .chars()
            .filter(|&c| c != '\r' && c != '\n')
            .collect::<String>()
            .trim()
            .to_string();
        Ok(V8Value::Raw(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: Vec<V8Value>) -> V8Value {
        V8Value::List(items)
    }
    fn raw(s: &str) -> V8Value {
        V8Value::Raw(s.to_string())
    }
    fn s(s: &str) -> V8Value {
        V8Value::Str(s.to_string())
    }

    #[test]
    fn empty_list() {
        // saby `JsonContainerDecoder`: пустое тело `{}` разбирается как список из ОДНОГО
        // элемента Raw("") (см. комментарий в `parse_list`), не как пустой список — так
        // ведёт себя реальный v8unpack.exe (проверено на обычных формах отчётов).
        assert_eq!(parse("{}").unwrap(), list(vec![raw("")]));
    }

    #[test]
    fn one_number() {
        assert_eq!(parse("{42}").unwrap(), list(vec![raw("42")]));
    }

    #[test]
    fn flat_list() {
        assert_eq!(
            parse("{1,2,3}").unwrap(),
            list(vec![raw("1"), raw("2"), raw("3")])
        );
    }

    #[test]
    fn nested_list() {
        assert_eq!(
            parse("{1,{2,3},4}").unwrap(),
            list(vec![raw("1"), list(vec![raw("2"), raw("3")]), raw("4")])
        );
    }

    #[test]
    fn simple_string() {
        assert_eq!(parse("{\"hello\"}").unwrap(), list(vec![s("hello")]));
    }

    #[test]
    fn empty_string() {
        assert_eq!(parse("{\"\"}").unwrap(), list(vec![s("")]));
    }

    #[test]
    fn string_with_escape() {
        // "a""b" → a"b
        assert_eq!(parse("{\"a\"\"b\"}").unwrap(), list(vec![s("a\"b")]));
    }

    #[test]
    fn string_with_newline() {
        // Многострочное содержимое — переводы строк часть строки.
        assert_eq!(
            parse("{\"line1\nline2\"}").unwrap(),
            list(vec![s("line1\nline2")])
        );
    }

    #[test]
    fn whitespace_tolerance() {
        assert_eq!(
            parse("{ 1 ,\n  2  ,\n  3\n}").unwrap(),
            list(vec![raw("1"), raw("2"), raw("3")])
        );
    }

    #[test]
    fn uuid_as_raw() {
        let uuid = "e41aff26-25cf-4bb6-b6c1-3f478a75f374";
        assert_eq!(
            parse(&format!("{{2,{uuid}}}")).unwrap(),
            list(vec![raw("2"), raw(uuid)])
        );
    }

    #[test]
    fn mixed_types() {
        let src = "{1,\"text\",{nested,\"x\"},42}";
        assert_eq!(
            parse(src).unwrap(),
            list(vec![
                raw("1"),
                s("text"),
                list(vec![raw("nested"), s("x")]),
                raw("42"),
            ])
        );
    }

    #[test]
    fn nav_path() {
        let v = parse("{1,{2,{3,{4}}}}").unwrap();
        assert_eq!(v.path(&[0]).unwrap(), &raw("1"));
        assert_eq!(v.path(&[1, 1, 1, 0]).unwrap(), &raw("4"));
        assert!(v.path(&[1, 1, 99]).is_none());
    }

    #[test]
    fn unterminated_string_errors() {
        assert!(parse("{\"hello").is_err());
    }

    #[test]
    fn unterminated_list_errors() {
        assert!(parse("{1,2,3").is_err());
    }

    #[test]
    fn missing_brace_errors() {
        assert!(parse("1,2,3}").is_err());
    }

    #[test]
    fn trailing_garbage_errors() {
        assert!(parse("{}garbage").is_err());
    }

    #[test]
    fn empty_value_in_list() {
        // Пустые значения парсятся как Raw("") — встречается в типизированных
        // полях метаданных (1С пишет `,,` при пропущенных параметрах).
        let v = parse("{1,,3}").unwrap();
        assert_eq!(v, list(vec![raw("1"), raw(""), raw("3")]));
    }

    #[test]
    fn cyrillic_via_utf8_bom() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("{\"Привет\"}".as_bytes());
        let v = parse_bytes_utf8_or_1251(&bytes).unwrap();
        assert_eq!(v, list(vec![s("Привет")]));
    }

    #[test]
    fn cyrillic_via_win1251() {
        // Закодируем "Привет" в windows-1251 без BOM.
        let (cow, _, _) = encoding_rs::WINDOWS_1251.encode("{\"Привет\"}");
        let v = parse_bytes_utf8_or_1251(&cow).unwrap();
        assert_eq!(v, list(vec![s("Привет")]));
    }

    #[test]
    fn to_json_simple() {
        let v = parse(r#"{1,"hello",2.5,e41aff26-25cf-4bb6-b6c1-3f478a75f374}"#).unwrap();
        let json = v.to_json_value();
        assert_eq!(
            json,
            serde_json::json!([1, "hello", 2.5, "e41aff26-25cf-4bb6-b6c1-3f478a75f374"])
        );
    }

    #[test]
    fn to_json_empty_raw_is_null() {
        let v = parse("{1,,3}").unwrap();
        let json = v.to_json_value();
        assert_eq!(json, serde_json::json!([1, null, 3]));
    }

    #[test]
    fn to_json_nested() {
        let v = parse(r#"{1,{2,"a"},{3,{4,5}}}"#).unwrap();
        let json = v.to_json_value();
        assert_eq!(json, serde_json::json!([1, [2, "a"], [3, [4, 5]]]));
    }

    #[test]
    fn list_of_lists_no_padding() {
        // Реальный паттерн из файлов-описателей метаданных: список UUID-ов.
        let src = r#"{
2,
e41aff26-25cf-4bb6-b6c1-3f478a75f374,
{1,"name"},
{0,0}
}"#;
        let v = parse(src).unwrap();
        let items = v.as_list().unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].as_raw(), Some("2"));
        assert_eq!(
            items[1].as_raw(),
            Some("e41aff26-25cf-4bb6-b6c1-3f478a75f374")
        );
        assert!(items[2].as_list().is_some());
        assert!(items[3].as_list().is_some());
    }
}
