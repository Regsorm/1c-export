//! Нативная распаковка внешней обработки (.epf) и внешнего отчёта (.erf) в
//! saby-совместимый формат вывода (JSON-описатели + `.obj.bsl` + макеты).
//!
//! Разбираются модуль объекта, формы (управляемые и обычные) с модулями и
//! элементами, макеты. Всё, что выходит за поддержанные рамки (другой тип
//! метаданных, старая версия платформы, неизвестный вложенный объект),
//! возвращается как [`UnpackOutcome::Unsupported`] с причиной — вызывающий код
//! оставляет файл бинарём и фиксирует провал; внешнего распаковщика нет.
//!
//! Эталонная семантика (байт-в-байт формат вывода) — saby/v8unpack 1.2.6,
//! файлы `MetaObject/ExternalDataProcessor.py`, `MetaObject/__init__.py`,
//! `MetaDataObject/__init__.py`, `json_container_decoder.py`. Реализация
//! полностью своя.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use crate::v8container::error::Result;
use crate::v8container::inflate::try_inflate;
use crate::v8container::reader::{unpack, V8File};
use crate::v8container::serlist::{parse_bytes_utf8_or_1251, V8Value};
use std::path::Path;

/// UUID типа метаданных «Внешняя обработка» (ExternalDataProcessor) в
/// заголовке скобкофайла. Якорь: saby `MetaObject/ExternalDataProcessor.py`.
const EXTERNAL_DATA_PROCESSOR_TYPE_UUID: &str = "c3831ec8-d8d5-4f93-8a22-f9bfae07327f";

/// UUID типа метаданных «Внешний отчёт» (ExternalReport) в заголовке
/// скобкофайла. Якорь: saby `MetaObject/ExternalReport.py`.
const EXTERNAL_REPORT_TYPE_UUID: &str = "e41aff26-25cf-4bb6-b6c1-3f478a75f374";

/// UUID типа вложенного объекта «Макет» (Template) в списке includes заголовка.
const TEMPLATE_TYPE_UUID: &str = "3daea016-69b7-4ed4-9453-127911372fe6";

/// UUID типа вложенного объекта «Форма» (управляемая форма) в списке includes
/// заголовка.
const FORM_TYPE_UUID: &str = "d5b0e5ed-256d-401c-9c36-f630cafd8a62";

/// UUID типа вложенного объекта «Обычная форма» (ReportForm, 1С 8.1-совместимый
/// формат FormElements26/27 — в отличие от управляемой формы `FORM_TYPE_UUID`) в
/// списке includes заголовка. Материализуется как content-entry `{obj_uuid}`.
const REPORT_FORM_TYPE_UUID: &str = "a3b368c0-29e2-11d6-a3c7-0050bae0a776";

/// UUID типа элемента формы «Поле» (saby `FormItemTypes.Field`).
const FORM_ITEM_FIELD_UUID: &str = "77ffcc29-7f2d-4223-b22f-19666e7250ba";

/// UUID типа элемента формы «Кнопка» (saby `FormItemTypes.Button`).
const FORM_ITEM_BUTTON_UUID: &str = "a9f3b1ac-f51b-431e-b102-55a69acdecad";

/// UUID типа элемента формы «Группа» (saby `FormItemTypes.Group`).
const FORM_ITEM_GROUP_UUID: &str = "cd5394d0-7dda-4b56-8927-93ccbe967a01";

/// UUID типа элемента формы «Таблица» (saby `FormItemTypes.Table`).
const FORM_ITEM_TABLE_UUID: &str = "143c00f7-a42d-4cd7-9189-88e4467dc768";

/// UUID типа элемента формы «Надпись» (saby `FormItemTypes.Decoration`).
const FORM_ITEM_DECORATION_UUID: &str = "3d3cb80c-508b-41fa-8a18-680cdf5f1712";

/// UUID типа элемента формы «Доп. элемент» (saby `FormItemTypes.ItemAddition`).
const FORM_ITEM_ITEM_ADDITION_UUID: &str = "c5259a1d-518a-4afd-b98d-0176027e4feb";

/// Версия v8unpack, под которую подогнан формат вывода (пишется в JSON как
/// есть, для совместимости с существующими потребителями вывода).
const V8UNPACK_VERSION: &str = "1.2.6";

/// Минимальная поддерживаемая версия платформы (major) в заголовке `version`.
const MIN_PLATFORM_VERSION: i64 = 216;

/// Результат попытки нативной распаковки.
#[derive(Debug)]
pub enum UnpackOutcome {
    /// Распаковано полностью, файлы записаны в `dest_dir`.
    Done,
    /// Контейнер не поддержан (другой тип метаданных, старая версия платформы,
    /// неизвестный вложенный объект и т.п.). Строка — причина, для
    /// диагностики/логов. Вызывающий код оставляет файл бинарём.
    Unsupported(String),
}

/// Извлечь Option<&V8Value> из пути, иначе — ранний выход из функции с
/// `Ok(UnpackOutcome::Unsupported(...))`. Отклонение реальной структуры от
/// ожидаемого скелета — это не ошибка ввода-вывода/парсинга, а сигнал
/// «не тот случай», поэтому не `Err`, а `Unsupported`.
macro_rules! req {
    ($opt:expr, $ctx:expr) => {
        match $opt {
            Some(v) => v,
            None => return Ok(UnpackOutcome::Unsupported($ctx.to_string())),
        }
    };
}

/// Достать текстовое содержимое узла (`Str` или `Raw`) как принадлежащую
/// строку. Для `List` возвращает пустую строку — на корректных путях скелета
/// такого не должно происходить.
fn text_of(v: &V8Value) -> String {
    match v {
        V8Value::Str(s) => s.clone(),
        V8Value::Raw(s) => s.clone(),
        V8Value::List(_) => String::new(),
    }
}

/// Нормализация переводов строк к каноническому `\n` (universal-newline,
/// как при чтении текста в Python). `\r\n` → `\n`, одиночный `\r` → `\n`.
fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Заменить узел дерева `V8Value` по пути индексов на `new`. Возвращает
/// `false`, если путь не ведёт к существующему элементу (мутация не сделана).
/// Нужен для saby-подстановки `header[1][2] = "в отдельном файле"` в заголовке
/// макета (см. `helper.decode_header`, `id_in_separate_file=True`).
fn set_at_path(root: &mut V8Value, path: &[usize], new: V8Value) -> bool {
    let (last, prefix) = match path.split_last() {
        Some(x) => x,
        None => return false,
    };
    let mut cur = root;
    for &i in prefix {
        cur = match cur {
            V8Value::List(items) => match items.get_mut(i) {
                Some(v) => v,
                None => return false,
            },
            _ => return false,
        };
    }
    match cur {
        V8Value::List(items) => match items.get_mut(*last) {
            Some(slot) => {
                *slot = new;
                true
            }
            None => false,
        },
        _ => false,
    }
}

/// Взять список за узлом `v` как изменяемый `Vec`, если это `List`. Нужен для
/// удаления распакованных элементов из середины списка (реквизиты формы,
/// см. `decode_form`) — `set_at_path` умеет только заменять узел, не удалять.
fn as_list_mut(v: &mut V8Value) -> Option<&mut Vec<V8Value>> {
    match v {
        V8Value::List(items) => Some(items),
        _ => None,
    }
}

/// Прочитать элемент списка `list[i]` как `i64` через `text_of`. Используется
/// для навигации по числовым полям-счётчикам в дереве формы (`decode_form`).
fn int_at(list: &[V8Value], i: usize) -> Option<i64> {
    list.get(i).map(text_of).and_then(|s| s.parse::<i64>().ok())
}

/// saby `helper.calc_offset`: пройти по парам `(смещение, размер)`. `size != 0`
/// означает, что по адресу `index` лежит счётчик, а за ним — `int(list[index]) *
/// size` дополнительных записей, которые надо перепрыгнуть. Возвращает `None`,
/// если счётчик по пути не парсится в число (структура не та). Используется для
/// вычисления позиций имени/ссылок в raw элемента формы (Field/Button).
fn calc_offset(counters: &[(usize, i64)], list: &[V8Value]) -> Option<usize> {
    let mut index: i64 = 0;
    for &(counter_index, size) in counters {
        index += counter_index as i64;
        if size != 0 {
            let value = int_at(list, index as usize)?;
            index += value * size;
        }
    }
    Some(index as usize)
}

/// saby `FormElement.check_count_element`: длина `list` за вычетом переменных
/// хвостов, на которые указывают счётчики (size!=0). Нужна для size-guard
/// группы/таблицы. `None` — если счётчик по пути не парсится в число.
fn check_count_element(counters: &[(usize, i64)], list: &[V8Value]) -> Option<i64> {
    let mut index: i64 = 0;
    let mut var_len: i64 = 0;
    for &(counter_index, size) in counters {
        index += counter_index as i64;
        if size != 0 {
            let value = int_at(list, index as usize)?;
            index += value * size;
            var_len += value * size;
        }
    }
    Some(list.len() as i64 - var_len)
}

/// Узел индекса реквизитов формы (saby `create_prop_index_by_id`): имя реквизита
/// + карта дочерних реквизитов (`id → PropNode`). Нужен для разрешения
/// многоуровневого «ПутьКДанным» (`Родитель.Ребёнок`). saby индексирует ровно
/// один уровень детей (`decode_child` не рекурсивна), поэтому у детей `child`
/// всегда пуст — путь глубже 2 уровней у saby тоже обрывается в `None`.
struct PropNode {
    name: String,
    child: std::collections::HashMap<String, PropNode>,
}

/// Разрешить «ПутьКДанным» реквизита по `prop_link` (saby `FormElement.decode`).
/// Многоуровневый путь (count>1) разворачивается по вложенному `props_index`:
/// на уровне `i` берём id из `prop_link[i+1][0]`, читаем имя узла, спускаемся в
/// его `child`, склеиваем имена через точку. Любой не найденный уровень (saby
/// `KeyError`) → `None`. Возвращаемый `Result` сохранён для совместимости с
/// вызовом (фолбэк по `Err` больше не нужен — saby на нерешённом пути даёт None).
fn resolve_prop_path(
    list: &[V8Value],
    prop_offset: usize,
    props_index: &std::collections::HashMap<String, PropNode>,
) -> std::result::Result<Option<String>, String> {
    let prop_link = match list.get(prop_offset).and_then(|v| v.as_list()) {
        Some(l) => l,
        None => return Ok(None),
    };
    let cnt = match int_at(prop_link, 0) {
        Some(n) => n,
        None => return Ok(None),
    };
    if cnt <= 0 {
        return Ok(None);
    }
    let mut names: Vec<String> = Vec::with_capacity(cnt as usize);
    let mut src = props_index;
    for i in 0..cnt as usize {
        let pid = prop_link
            .get(i + 1)
            .and_then(|v| v.as_list())
            .and_then(|l| l.first())
            .map(text_of);
        match pid.and_then(|p| src.get(&p)) {
            Some(node) => {
                names.push(node.name.clone());
                src = &node.child;
            }
            None => return Ok(None),
        }
    }
    if names.is_empty() {
        Ok(None)
    } else {
        Ok(Some(names.join(".")))
    }
}

/// Разрешить «ИмяКоманды» кнопки по `command_link` (saby `FormElement.decode`).
fn resolve_command(
    list: &[V8Value],
    cmd_offset: usize,
    commands_index: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let cmd_link = list.get(cmd_offset).and_then(|v| v.as_list())?;
    let cid = cmd_link.get(0).map(text_of)?;
    let non_zero = cid.parse::<i64>().map(|n| n != 0).unwrap_or(false);
    if !non_zero {
        return None;
    }
    commands_index.get(&cid).cloned()
}

/// Результат разбора списка элементов формы: дерево (`None` при пустом
/// контейнере — saby `decode_list` при count=0 выходит рано, БЕЗ маркера)
/// либо сигнал фолбэка на v8unpack.
enum ElemResult {
    Tree(Option<Vec<J>>),
    Fallback(String),
}

/// Результат разбора одного элемента: узел дерева либо сигнал фолбэка.
enum ElemOne {
    Node(J),
    Fallback(String),
}

/// Рекурсивно разобрать список элементов формы (saby `FormElement.decode_list`).
/// `items` — контейнер с элементами (root_data формы или raw группы),
/// `index_element_count` — позиция счётчика элементов. Заполняет `data`
/// (path-ключ → JSON записи элемента), возвращает дерево узлов. МУТИРУЕТ `items`:
/// при count>0 ставит маркер «Дочерние элементы отдельно» + удаляет пары
/// (uuid, raw) разобранных элементов.
fn decode_form_elements(
    items: &mut Vec<V8Value>,
    index_element_count: usize,
    path: &str,
    props_index: &std::collections::HashMap<String, PropNode>,
    commands_index: &std::collections::HashMap<String, String>,
    data: &mut Vec<(String, J)>,
) -> Result<ElemResult> {
    let count = match int_at(items.as_slice(), index_element_count) {
        Some(n) if n >= 0 => n as usize,
        _ => {
            return Ok(ElemResult::Fallback(format!(
                "форма: счётчик элементов не число (path=\"{path}\")"
            )))
        }
    };
    if count == 0 {
        return Ok(ElemResult::Tree(None));
    }
    if items.len() < index_element_count + 1 + count * 2 {
        return Ok(ElemResult::Fallback(format!(
            "форма: контейнер элементов короче ожидаемого (path=\"{path}\")"
        )));
    }

    let mut tree: Vec<J> = Vec::with_capacity(count);
    for i in 0..count {
        let uuid = match items.get(index_element_count + i * 2 + 1).map(text_of) {
            Some(u) => u,
            None => {
                return Ok(ElemResult::Fallback(format!(
                    "форма: нет uuid элемента (path=\"{path}\", i={i})"
                )))
            }
        };
        // Клонируем raw элемента: группа мутирует свой raw (drain детей), а сам
        // `items` дренируется в конце — клон снимает конфликт заимствований.
        let mut elem_val = match items.get(index_element_count + i * 2 + 2) {
            Some(v) => v.clone(),
            None => {
                return Ok(ElemResult::Fallback(format!(
                    "форма: нет raw элемента (path=\"{path}\", i={i})"
                )))
            }
        };
        match decode_one_element(&uuid, &mut elem_val, path, props_index, commands_index, data)? {
            ElemOne::Node(node) => tree.push(node),
            ElemOne::Fallback(r) => return Ok(ElemResult::Fallback(r)),
        }
    }

    items[index_element_count] = V8Value::Raw("Дочерние элементы отдельно".to_string());
    items.drain(index_element_count + 1..index_element_count + 1 + count * 2);
    Ok(ElemResult::Tree(Some(tree)))
}

/// Разобрать один элемент формы по типу (saby `FormElement.decode` +
/// переопределения Field/Button/Decoration/ItemAddition/Group/Table). Пишет запись
/// элемента в `data` (path-ключ → {raw, ver[, ПутьКДанным|ИмяКоманды]}); для контейнеров
/// (Group/Table) рекурсивно разбирает детей (мутируя `elem_val`) и добавляет
/// `child` в узел дерева.
fn decode_one_element(
    uuid: &str,
    elem_val: &mut V8Value,
    path: &str,
    props_index: &std::collections::HashMap<String, PropNode>,
    commands_index: &std::collections::HashMap<String, String>,
    data: &mut Vec<(String, J)>,
) -> Result<ElemOne> {
    // ─── фаза 1: иммутабельные вычисления (тип, имя, ссылки, контейнер) ─────
    // `container` = Some((child_index, path_replace)) для контейнеров
    // (Group/Table); None для листовых (Field/Button). path_replace — заменять
    // ли `includr_→include_` в пути детей (saby делает это для Group, не Table).
    #[allow(clippy::type_complexity)]
    let (type_name, name, prop_path, command_name, container): (
        &str,
        String,
        Option<String>,
        Option<String>,
        Option<(usize, bool)>,
    ) = {
        let list = match elem_val.as_list() {
            Some(l) => l,
            None => {
                return Ok(ElemOne::Fallback(format!(
                    "форма: элемент не список (path=\"{path}\")"
                )))
            }
        };
        // Тип → смещения имени/ссылок + (для контейнеров) size-guard и индекс
        // детей. Guard: saby при «плохой» скобочной структуре уходит в спец-
        // разбор (FuckingBrackets), который мы не воспроизводим → фолбэк.
        let (type_name, name_off, prop_off, cmd_off, container): (
            &str,
            Option<usize>,
            Option<usize>,
            Option<usize>,
            Option<(usize, bool)>,
        ) = match uuid {
            FORM_ITEM_FIELD_UUID => (
                "Field",
                calc_offset(&[(3, 1), (1, 1), (2, 0)], list),
                calc_offset(&[(3, 1), (1, 1), (7, 0)], list),
                None,
                None,
            ),
            FORM_ITEM_BUTTON_UUID => (
                "Button",
                calc_offset(&[(3, 1), (2, 0)], list),
                None,
                calc_offset(&[(3, 1), (5, 0)], list),
                None,
            ),
            FORM_ITEM_GROUP_UUID => {
                // saby Group.decode: guard raw[0]=='22' и остаток < 20.
                if list.get(0).map(text_of).as_deref() == Some("22")
                    && check_count_element(&[(3, 1), (1, 1), (17, 2)], list)
                        .map_or(true, |s| s < 20)
                {
                    return Ok(ElemOne::Fallback(format!(
                        "форма: группа требует спец-разбора (path=\"{path}\")"
                    )));
                }
                let ci = match calc_offset(&[(3, 1), (1, 1), (17, 0)], list) {
                    Some(idx) => idx,
                    None => {
                        return Ok(ElemOne::Fallback(format!(
                            "форма: нет смещения детей группы (path=\"{path}\")"
                        )))
                    }
                };
                (
                    "Group",
                    calc_offset(&[(3, 1), (1, 1), (2, 0)], list),
                    None,
                    None,
                    Some((ci, true)),
                )
            }
            FORM_ITEM_TABLE_UUID => {
                // saby Table.decode: guard raw[0]=='55' и остаток != 99.
                if list.get(0).map(text_of).as_deref() == Some("55")
                    && check_count_element(&[(4, 1), (50, 2), (7, 2)], list)
                        .map_or(true, |s| s != 99)
                {
                    return Ok(ElemOne::Fallback(format!(
                        "форма: таблица требует спец-разбора (path=\"{path}\")"
                    )));
                }
                let ci = match calc_offset(&[(4, 1), (50, 2), (7, 0)], list) {
                    Some(idx) => idx,
                    None => {
                        return Ok(ElemOne::Fallback(format!(
                            "форма: нет смещения колонок таблицы (path=\"{path}\")"
                        )))
                    }
                };
                (
                    "Table",
                    calc_offset(&[(4, 1), (1, 0)], list),
                    calc_offset(&[(4, 1), (7, 0)], list),
                    None,
                    Some((ci, false)),
                )
            }
            FORM_ITEM_DECORATION_UUID => (
                // saby Decoration: тот же offset имени, что у Field, но БЕЗ
                // get_prop_link_offset/get_command_link_offset (обе остаются None
                // у базового FormElement — ни «ПутьКДанным», ни «ИмяКоманды»).
                "Decoration",
                calc_offset(&[(3, 1), (1, 1), (2, 0)], list),
                None,
                None,
                None,
            ),
            FORM_ITEM_ITEM_ADDITION_UUID => (
                // saby ItemAddition: свой offset имени, тоже без prop/command link.
                "ItemAddition",
                calc_offset(&[(3, 1), (3, 0)], list),
                None,
                None,
                None,
            ),
            other => {
                return Ok(ElemOne::Fallback(format!(
                    "форма: тип элемента {other} не поддержан (path=\"{path}\")"
                )))
            }
        };
        let name_off = match name_off {
            Some(n) => n,
            None => {
                return Ok(ElemOne::Fallback(format!(
                    "форма: нет смещения имени {type_name} (path=\"{path}\")"
                )))
            }
        };
        let name = match list.get(name_off).map(text_of) {
            Some(n) => n,
            None => {
                return Ok(ElemOne::Fallback(format!(
                    "форма: нет имени {type_name} по смещению {name_off} (path=\"{path}\")"
                )))
            }
        };

        let prop_path = if let Some(po) = prop_off {
            match resolve_prop_path(list, po, props_index) {
                Ok(p) => p,
                Err(r) => return Ok(ElemOne::Fallback(r)),
            }
        } else {
            None
        };
        let command_name = cmd_off.and_then(|co| resolve_command(list, co, commands_index));

        (type_name, name, prop_path, command_name, container)
    };

    // ─── фаза 2: рекурсия детей контейнера (Group/Table, мутирует elem_val) ─
    let is_container = container.is_some();
    let mut child_tree: Option<Vec<J>> = None;
    if let Some((child_index, path_replace)) = container {
        // saby: new_path = f"{path}/{name}"; для Group ещё .replace(includr_→include_)
        let new_path = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}/{name}")
        };
        let new_path = if path_replace {
            new_path.replace("includr_", "include_")
        } else {
            new_path
        };
        let gitems = match as_list_mut(elem_val) {
            Some(l) => l,
            None => {
                return Ok(ElemOne::Fallback(format!(
                    "форма: контейнер {name} не список (path=\"{path}\")"
                )))
            }
        };
        match decode_form_elements(gitems, child_index, &new_path, props_index, commands_index, data)?
        {
            ElemResult::Tree(t) => child_tree = t,
            ElemResult::Fallback(r) => return Ok(ElemOne::Fallback(r)),
        }
    }

    // ─── фаза 3: запись элемента в data + узел дерева ──────────────────────
    // Для группы raw сериализуется ПОСЛЕ рекурсии — с детьми, заменёнными
    // маркером (saby хранит ссылку на тот же raw, мутируемый decode_list).
    let key = if path.is_empty() {
        name.clone()
    } else {
        format!("{path}/{name}")
    };
    let mut data_obj: Vec<(String, J)> = vec![
        ("raw".to_string(), saby_json(elem_val)),
        ("ver".to_string(), J::Num(4)),
    ];
    if let Some(p) = prop_path {
        data_obj.push(("ПутьКДанным".to_string(), J::Str(p)));
    }
    if let Some(c) = command_name {
        data_obj.push(("ИмяКоманды".to_string(), J::Str(c)));
    }
    data.push((key, J::Obj(data_obj)));

    let mut node: Vec<(String, J)> = vec![
        ("name".to_string(), J::Str(name)),
        ("type".to_string(), J::Str(type_name.to_string())),
    ];
    // Узел контейнера (Group/Table) всегда несёт ключ `child` (null при пустом
    // контейнере — saby `data['child'] = decode_list(...)` кладёт None).
    if is_container {
        node.push((
            "child".to_string(),
            match child_tree {
                Some(ct) => J::Arr(ct),
                None => J::Null,
            },
        ));
    }
    Ok(ElemOne::Node(J::Obj(node)))
}

// ─── собственный JSON pretty-эмиттер (НЕ serde_json) ───────────────────────
//
// Байт-в-байт совместим с Python `json.dump(data, ensure_ascii=False, indent=2)`:
// отступ 2 пробела, `": "` после ключа, `,\n` между элементами, без
// экранирования не-ASCII, без завершающего перевода строки в конце.

/// Внутреннее JSON-дерево для pretty-эмиттера.
enum J {
    Null,
    Bool(bool),
    Num(i64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    fn write(&self, out: &mut String, indent: usize) {
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Num(n) => out.push_str(&n.to_string()),
            J::Str(s) => {
                out.push('"');
                escape_json_string(s, out);
                out.push('"');
            }
            J::Arr(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push_str("[\n");
                let inner = indent + 1;
                for (i, item) in items.iter().enumerate() {
                    push_indent(out, inner);
                    item.write(out, inner);
                    if i + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                push_indent(out, indent);
                out.push(']');
            }
            J::Obj(entries) => {
                if entries.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push_str("{\n");
                let inner = indent + 1;
                for (i, (key, value)) in entries.iter().enumerate() {
                    push_indent(out, inner);
                    out.push('"');
                    escape_json_string(key, out);
                    out.push_str("\": ");
                    value.write(out, inner);
                    if i + 1 < entries.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                push_indent(out, indent);
                out.push('}');
            }
        }
    }

    fn to_pretty_string(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out
    }
}

fn push_indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

/// Экранирование строки как в Python `json.dumps(..., ensure_ascii=False)`:
/// не-ASCII (кириллица) остаётся сырым UTF-8, экранируются только
/// управляющие символы и сама кавычка/бэкслеш.
fn escape_json_string(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

/// Преобразовать дерево `V8Value` (скобкофайл) в `J` так, как это делает
/// saby `json_container_decoder.py`: каждый лист становится JSON-строкой.
/// `Str` восстанавливает кавычки, которые снял наш парсер (с удвоением
/// внутренних), `Raw` идёт как есть (числа, UUID, пустые значения — всё
/// строками).
fn saby_json(v: &V8Value) -> J {
    match v {
        V8Value::List(items) => J::Arr(items.iter().map(saby_json).collect()),
        V8Value::Raw(s) => J::Str(s.clone()),
        V8Value::Str(s) => J::Str(format!("\"{}\"", s.replace('"', "\"\""))),
    }
}

/// Определить «Тип формы» дёшево, БЕЗ полного разбора — только по дескриптору
/// (`{form_uuid}`), не трогая содержимое `{form_uuid}.0`. Нужен для выбора между
/// `decode_form` (Тип=1, управляемая, FormElements4) и `decode_regular_form` (Тип=0,
/// обычная, FormElements26/27) ДО того, как одна из них начнёт разбор — saby определяет
/// это полностью аналогично (`FormCore.decode_data`: `_header_obj[1][3]`, `IndexError` → `"0"`).
///
/// При любой неопределённости (нет дескриптора, неизвестный disc-узел `[0,1,0]`) возвращает
/// `"0"` — решение по умолчанию «не управляемая»: если это неверно, точную причину
/// `Unsupported` всё равно вернёт `decode_regular_form` при полном разборе.
fn peek_form_kind(content: &V8File, form_uuid: &str) -> Result<String> {
    let fh = match content.find(form_uuid) {
        Some(entry) => V8Value::List(vec![parse_bytes_utf8_or_1251(&try_inflate(&entry.data))?]),
        None => return Ok("0".to_string()),
    };
    let base: Vec<usize> = match fh.path(&[0, 1, 0]).map(text_of).as_deref() {
        Some("0") => vec![0, 1],
        Some("1") => vec![0, 1, 1],
        _ => return Ok("0".to_string()),
    };
    let type_path: Vec<usize> = base.iter().copied().chain([1, 3]).collect();
    Ok(fh.path(&type_path).map(text_of).unwrap_or_else(|| "0".to_string()))
}

// ─── публичный API ──────────────────────────────────────────────────────────

/// Распаковать «скелет» ExternalDataProcessor из `.epf` в `dest_dir`.
///
/// Пишет `ExternalDataProcessor.json` (всегда) и `ExternalDataProcessor.obj.bsl`
/// (если модуль объекта непустой; содержимое проходит `strip_include_areas` —
/// см. её докстринг про области `#Область include_.../includr_...`). При любом
/// отклонении от ожидаемой формы (не тот тип метаданных, старая версия платформы,
/// есть формы/шаблоны, нестандартная раскладка контейнера) — возвращает
/// `Unsupported`, не пишет ничего и не считается ошибкой ввода-вывода.
pub fn unpack_epf_skeleton(epf_bytes: &[u8], dest_dir: &Path) -> Result<UnpackOutcome> {
    let root_v8 = unpack(epf_bytes)?;

    // Ш1: развернуть контейнер до «content» — того, что содержит entry "root".
    let content: V8File = if root_v8.find("root").is_some() {
        root_v8
    } else {
        let mut found: Option<V8File> = None;
        for entry in &root_v8.entries {
            // Entry может быть сжат raw DEFLATE — пробуем развернуть перед
            // попыткой распаковать как вложенный контейнер (см. inflate.rs).
            let inflated = try_inflate(&entry.data);
            if let Ok(inner) = unpack(&inflated) {
                if inner.find("root").is_some() {
                    found = Some(inner);
                }
            }
        }
        match found {
            Some(v) => v,
            None => {
                return Ok(UnpackOutcome::Unsupported(
                    "не найден контейнер с entry \"root\"".to_string(),
                ))
            }
        }
    };

    // Ш2: прочитать «скобкофайлы». Каждый entry внутри контейнера может быть
    // сжат raw DEFLATE без явного маркера — разворачиваем перед парсингом
    // (try_inflate возвращает данные как есть, если это не valid DEFLATE).
    let root_entry = req!(content.find("root"), "нет entry \"root\" в content-контейнере");
    let root_val = parse_bytes_utf8_or_1251(&try_inflate(&root_entry.data))?;
    let file_uuid = text_of(req!(root_val.get(1), "root: нет [1] (file_uuid)"));

    // header/version/copyinfo/info — каждый скобкофайл на диске хранит СВОЙ
    // корневой список плоско (без обёртки), но в выводе v8unpack каждый из
    // них представлен как одноэлементный список, содержащий этот корневой
    // список целиком (`[<весь распарсенный файл>]`). Оборачиваем сразу после
    // парсинга — дальше по коду навигация (`path([0, ...])`) и финальная
    // сериализация в JSON работают с уже обёрнутым значением единообразно.
    let header_entry = req!(
        content.find(&file_uuid),
        format!("нет entry \"{file_uuid}\" (header) в content-контейнере")
    );
    let header_val = V8Value::List(vec![parse_bytes_utf8_or_1251(&try_inflate(
        &header_entry.data,
    ))?]);

    let version_entry = req!(content.find("version"), "нет entry \"version\"");
    let version_val = V8Value::List(vec![parse_bytes_utf8_or_1251(&try_inflate(
        &version_entry.data,
    ))?]);

    let copyinfo_entry = req!(content.find("copyinfo"), "нет entry \"copyinfo\"");
    let copyinfo_val = V8Value::List(vec![parse_bytes_utf8_or_1251(&try_inflate(
        &copyinfo_entry.data,
    ))?]);

    // Ш3: детект типа/версии — порт saby decoder.get_handler_by_version_file.
    // obj_version и класс метаданных зависят от версии платформы v0[0]:
    //   >=216 — obj_type берётся из header, obj_version по длине version-узла;
    //   ==106 — легаси, ВСЕГДА ExternalDataProcessor, obj_version='801'.
    let v0 = req!(version_val.path(&[0, 0]), "version: нет пути [0,0]");
    let ver_major_text = text_of(req!(v0.get(0), "version: нет [0,0,0] (ver_major)"));
    let ver_major: i64 = match ver_major_text.parse() {
        Ok(n) => n,
        Err(_) => {
            return Ok(UnpackOutcome::Unsupported(format!(
                "версия платформы не число: {ver_major_text:?}"
            )))
        }
    };

    // saby-конвенция: JSON-описатель называется по классу метаданных
    // (ExternalReport.json / ExternalDataProcessor.json), а модуль объекта —
    // ВСЕГДА ExternalDataProcessor.obj.bsl (проверено golden отчёта).
    let (report_class_name, obj_version): (&str, String) = if ver_major >= MIN_PLATFORM_VERSION {
        let obj_type_uuid = text_of(req!(
            header_val.path(&[0, 3, 0]),
            "header: нет пути [0,3,0] (obj_type_uuid)"
        ));
        let is_report = obj_type_uuid == EXTERNAL_REPORT_TYPE_UUID;
        if !is_report && obj_type_uuid != EXTERNAL_DATA_PROCESSOR_TYPE_UUID {
            return Ok(UnpackOutcome::Unsupported(format!(
                "тип {obj_type_uuid} не ExternalDataProcessor/ExternalReport"
            )));
        }
        // obj_version по длине version[0][0]:
        //   len==2 → '802';
        //   len==3 → первые 3 символа version[0][0][2][0]; если та короче
        //            и это '1'/'2' → '802', иначе не поддержано.
        let obj_version = match v0.len() {
            Some(2) => "802".to_string(),
            Some(3) => {
                let full = text_of(req!(
                    v0.path(&[2, 0]),
                    "version: нет пути [0,0,2,0] (obj_version)"
                ));
                match full.get(0..3) {
                    Some(s) if s.len() == 3 => s.to_string(),
                    _ if full == "1" || full == "2" => "802".to_string(),
                    _ => {
                        return Ok(UnpackOutcome::Unsupported(format!(
                            "obj_version не поддержана: {full:?}"
                        )))
                    }
                }
            }
            other => {
                return Ok(UnpackOutcome::Unsupported(format!(
                    "version[0][0]: неподдержанная длина {other:?}"
                )))
            }
        };
        let class = if is_report {
            "ExternalReport"
        } else {
            "ExternalDataProcessor"
        };
        (class, obj_version)
    } else if ver_major == 106 {
        // легаси старый формат: obj_type из header не читается, всегда обработка.
        ("ExternalDataProcessor", "801".to_string())
    } else {
        return Ok(UnpackOutcome::Unsupported(format!(
            "версия платформы {ver_major} не поддерживается"
        )));
    };

    // Ш4: разобрать список вложенных объектов (includes). Поддержаны только
    // макеты (Template) с типом scheme(6) — их UUID собираем для последующей
    // распаковки. Любой другой вложенный объект (формы/команды) → Unsupported
    // (фолбэк на внешний v8unpack.exe).
    let inc = req!(
        header_val.path(&[0, 3, 1]),
        "header: нет пути [0,3,1] (includes)"
    );
    let count_types_text = text_of(req!(inc.get(2), "header includes: нет count_types"));
    let count_types: usize = match count_types_text.parse() {
        Ok(n) => n,
        Err(_) => {
            return Ok(UnpackOutcome::Unsupported(format!(
                "count_types не число: {count_types_text:?}"
            )))
        }
    };
    let mut template_uuids: Vec<String> = Vec::new();
    let mut form_uuids: Vec<String> = Vec::new();
    let mut report_form_uuids: Vec<String> = Vec::new();
    for i in 0..count_types {
        let meta = req!(
            inc.get(i + 3),
            format!("header includes: нет meta[{i}]")
        );
        let type_uuid = text_of(req!(
            meta.get(0),
            format!("header includes meta[{i}]: нет type_uuid")
        ));
        let count_obj: i64 = text_of(req!(
            meta.get(1),
            format!("header includes meta[{i}]: нет count_obj")
        ))
        .parse()
        .unwrap_or(0);
        if count_obj == 0 {
            continue;
        }
        if type_uuid == TEMPLATE_TYPE_UUID {
            for j in 0..count_obj as usize {
                let tmpl_uuid = text_of(req!(
                    meta.get(2 + j),
                    format!("header includes meta[{i}]: нет tmpl_uuid[{j}]")
                ));
                template_uuids.push(tmpl_uuid);
            }
        } else if type_uuid == FORM_TYPE_UUID {
            for j in 0..count_obj as usize {
                let form_uuid = text_of(req!(
                    meta.get(2 + j),
                    format!("header includes meta[{i}]: нет form_uuid[{j}]")
                ));
                form_uuids.push(form_uuid);
            }
        } else if type_uuid == REPORT_FORM_TYPE_UUID {
            for j in 0..count_obj as usize {
                let rf_uuid = text_of(req!(
                    meta.get(2 + j),
                    format!("header includes meta[{i}]: нет report_form_uuid[{j}]")
                ));
                report_form_uuids.push(rf_uuid);
            }
        } else {
            // Прочие типы включений. Большинство (реквизиты, табличные части,
            // параметры) инлайнятся в header — saby не создаёт для них
            // отдельных артефактов. НО материализующиеся типы (обычные формы
            // ReportForm, команды и т.п.) имеют собственный content-entry
            // `{obj_uuid}` и выгружаются saby в отдельный каталог. Такие типы
            // мы пока не разбираем нативно: если у объекта есть content-entry —
            // возвращаем Unsupported, чтобы export откатился на полный
            // v8unpack.exe (иначе выгрузим НЕПОЛНЫЙ скелет без этого объекта).
            for j in 0..count_obj as usize {
                let obj_uuid = text_of(req!(
                    meta.get(2 + j),
                    format!("header includes meta[{i}]: нет obj_uuid[{j}]")
                ));
                if content.find(&obj_uuid).is_some() {
                    return Ok(UnpackOutcome::Unsupported(format!(
                        "вложенный объект типа {type_uuid} (uuid {obj_uuid}) \
                         материализуется, но нативно не поддержан"
                    )));
                }
            }
            continue;
        }
    }

    // Ш4.5: контейнер метаданных объекта для saby-подстановки «Родитель» в
    // реквизитах формы (см. `decode_form`). Кандидат — DHC[1] (второй элемент
    // «большого списка» под includes, header_val.path([0,3,1,1])); подтверждено
    // эмпирически по golden-фикстуре «Регистратор» — совпадает с raw
    // pattern-uuid реквизита «Объект» до saby-подстановки.
    let dhc = req!(
        header_val.path(&[0, 3, 1, 1]),
        "header: нет пути [0,3,1,1] (DHC)"
    );
    let parent_container_uuid = text_of(req!(dhc.get(1), "DHC: нет [1] (container candidate)"));

    // Ш5: decode_header — uuid/name/name2/comment.
    let hdr = req!(
        header_val.path(&[0, 3, 1, 1, 3, 1]),
        "header: нет пути [0,3,1,1,3,1] (hdr)"
    );
    let uuid = text_of(req!(hdr.path(&[1, 2]), "hdr: нет пути [1,2] (uuid)"));
    let name = text_of(req!(hdr.get(2), "hdr: нет [2] (name)"));

    let n2 = req!(hdr.get(3), "hdr: нет [3] (name2)");
    let name2_count: usize = text_of(req!(n2.get(0), "name2: нет count"))
        .parse()
        .unwrap_or(0);
    let mut name2: Vec<(String, String)> = Vec::with_capacity(name2_count);
    for i in 0..name2_count {
        let key = text_of(req!(n2.get(1 + 2 * i), format!("name2[{i}]: нет key")));
        let val = text_of(req!(n2.get(2 + 2 * i), format!("name2[{i}]: нет val")));
        name2.push((key, val));
    }

    let comment = text_of(req!(hdr.get(4), "hdr: нет [4] (comment)"));

    // Ш6: контейнер кода `{uuid}.0` → info + text. НЕОБЯЗАТЕЛЕН: у объекта с
    // пустым модулем такого контейнера нет — тогда saby не пишет
    // `code_info_obj`/`code_encoding_obj` в JSON и не создаёт `.obj.bsl`.
    // `code_module` = Some((info_val, code_encoding_obj, code_obj)) при наличии.
    let code_module: Option<(V8Value, &str, String)> =
        match content.find(&format!("{uuid}.0")) {
            None => None,
            Some(code_entry) => {
                let code_v8 = match unpack(&try_inflate(&code_entry.data)) {
                    Ok(v) => v,
                    Err(_) => {
                        return Ok(UnpackOutcome::Unsupported(format!(
                            "{uuid}.0 не является вложенным контейнером — не dir-скелет"
                        )))
                    }
                };
                let info_entry = req!(
                    code_v8.find("info"),
                    format!("{uuid}.0: нет entry \"info\"")
                );
                let info_val = V8Value::List(vec![parse_bytes_utf8_or_1251(&try_inflate(
                    &info_entry.data,
                ))?]);

                let text_entry = req!(
                    code_v8.find("text"),
                    format!("{uuid}.0: нет entry \"text\"")
                );
                let text_data = try_inflate(&text_entry.data);
                let (code_encoding_obj, text_bytes_no_bom): (&str, &[u8]) =
                    if text_data.starts_with(&[0xEF, 0xBB, 0xBF]) {
                        ("utf-8-sig", &text_data[3..])
                    } else {
                        ("utf-8", &text_data[..])
                    };
                let code_text_raw = match std::str::from_utf8(text_bytes_no_bom) {
                    Ok(s) => s.to_string(),
                    Err(_) => {
                        return Ok(UnpackOutcome::Unsupported(format!(
                            "{uuid}.0/text: не валидный UTF-8"
                        )))
                    }
                };
                Some((info_val, code_encoding_obj, normalize_newlines(&code_text_raw)))
            }
        };

    // Ш7: form1 — необязательный.
    let form1: Option<V8Value> = match content.find(&format!("{uuid}.1")) {
        Some(e) => Some(V8Value::List(vec![parse_bytes_utf8_or_1251(&try_inflate(
            &e.data,
        ))?])),
        None => None,
    };

    // ─── сборка ExternalDataProcessor.json (порядок ключей фиксирован) ─────
    let name2_json: Vec<(String, J)> = name2
        .into_iter()
        .map(|(k, v)| (k, J::Str(v)))
        .collect();

    let mut root_entries: Vec<(String, J)> = vec![
        ("root".to_string(), J::Bool(true)),
        ("file_uuid".to_string(), J::Str(file_uuid)),
        ("uuid".to_string(), J::Str(uuid.clone())),
        ("name".to_string(), J::Str(name)),
        ("name2".to_string(), J::Obj(name2_json)),
        ("comment".to_string(), J::Str(comment)),
        ("header".to_string(), saby_json(&header_val)),
        ("v8unpack".to_string(), J::Str(V8UNPACK_VERSION.to_string())),
        ("version".to_string(), saby_json(&version_val)),
        ("copyinfo".to_string(), saby_json(&copyinfo_val)),
        (
            "form1".to_string(),
            match &form1 {
                Some(v) => saby_json(v),
                None => J::Null,
            },
        ),
    ];
    // code_info_obj/code_encoding_obj — только при непустом модуле объекта
    // (наличии контейнера `{uuid}.0`); saby при пустом модуле их опускает.
    if let Some((info_val, code_encoding_obj, _)) = &code_module {
        root_entries.push(("code_info_obj".to_string(), saby_json(info_val)));
        root_entries.push((
            "code_encoding_obj".to_string(),
            J::Str(code_encoding_obj.to_string()),
        ));
    }
    root_entries.push(("obj_version".to_string(), J::Str(obj_version)));
    let root_obj = J::Obj(root_entries);

    let json_text = root_obj.to_pretty_string().replace('\n', "\r\n");

    std::fs::create_dir_all(dest_dir)?;
    std::fs::write(
        dest_dir.join(format!("{report_class_name}.json")),
        json_text.as_bytes(),
    )?;

    if let Some((_, _, code_obj)) = &code_module {
        if !code_obj.is_empty() {
            let bsl_text = strip_include_areas(code_obj).replace('\n', "\r\n");
            std::fs::write(dest_dir.join("ExternalDataProcessor.obj.bsl"), bsl_text.as_bytes())?;
        }
    }

    // Вложенные макеты (СКД scheme) — каждый в dest_dir/Template/{имя объекта}/.
    for tmpl_uuid in &template_uuids {
        match decode_template(&content, tmpl_uuid, dest_dir)? {
            UnpackOutcome::Done => {}
            other => return Ok(other),
        }
    }

    // Вложенные формы обработки (include-тип d5b0e5ed/Form) — каждая в
    // dest_dir/Form/{имя формы}/. Управляемая (Тип=1) или обычная (Тип=0) — решает
    // peek_form_kind дёшево, до полного разбора (см. его докстринг).
    for form_uuid in &form_uuids {
        let outcome = if peek_form_kind(&content, form_uuid)? == "1" {
            decode_form(&content, form_uuid, &parent_container_uuid, "Form", dest_dir)?
        } else {
            decode_regular_form(&content, form_uuid, "Form", dest_dir)?
        };
        match outcome {
            UnpackOutcome::Done => {}
            other => return Ok(other),
        }
    }

    // Вложенные формы отчёта (include-тип a3b368c0/ReportForm) — каждая в
    // dest_dir/ReportForm/{имя формы}/. Та же дихотомия Тип формы, что и у Form выше.
    for rf_uuid in &report_form_uuids {
        let outcome = if peek_form_kind(&content, rf_uuid)? == "1" {
            decode_form(&content, rf_uuid, &parent_container_uuid, "ReportForm", dest_dir)?
        } else {
            decode_regular_form(&content, rf_uuid, "ReportForm", dest_dir)?
        };
        match outcome {
            UnpackOutcome::Done => {}
            other => return Ok(other),
        }
    }

    Ok(UnpackOutcome::Done)
}

/// Распаковать вложенный макет (Template) в `dest_dir/Template/{имя объекта}/`:
/// `Template.{bin|mxl|c1b64|txt|html}` (данные, если есть), `Template.json` (описатель),
/// `Template.id.json` (UUID). Поддержаны типы (saby `TmplType`): scheme(6) → `.bin`
/// (контейнер СКД), table(0, табличный документ) → `.mxl`, base64(1) → `.c1b64`
/// (скобко-файл `{#base64…}`) — данные копируются как есть (saby
/// `decode_scheme_data`/`decode_base64_data` в ветке BigBase64); active_doc(2),
/// geographic(5), design(7), graphic_scheme(8) — саby делегирует их на тот же
/// `decode_scheme_data`, что и scheme(6) (`decode_active_doc_data`/`decode_geographic_data`/
/// `decode_design_data`/`decode_graphic_scheme_data` — каждая однострочная обёртка), поэтому
/// данные копируются как есть в `.bin`, различается только имя типа в JSON; extension(9) —
/// саby делегирует на `decode_base64_data` (`decode_extension_data`), т.е. как base64(1) —
/// копия в `.c1b64`; text(4) — текстовый файл с нормализацией переводов строк и сохранением
/// BOM (saby `decode_text_data`); html(3) → `.html` — base64-полезная нагрузка ИЗВЛЕКАЕТСЯ из
/// скобко-структуры (не копия как есть, см. отдельный комментарий у `html_field`/`html_out`
/// ниже, saby `MetaObject._decode_html_data`), при этом мутированная структура идёт доп.
/// JSON-ключом "html". Контейнер `{uuid}.0` НЕОБЯЗАТЕЛЕН: у макета-заглушки без сохранённых
/// данных его нет → пишутся только `Template.json` + `Template.id.json` (saby:
/// `decode_scheme_data`/`decode_text_data`/`_decode_html_data` ловят FileNotFound и
/// возвращаются). Прочие типы макета → `Unsupported`.
///
/// Ограничение: base64/extension трактуем как BigBase64 (копия скобко-файла в `.c1b64`);
/// малый инлайн-base64 (декодирование в бинарь по расширению из комментария) в
/// реальном корпусе не встречается и не реализован.
///
/// Дескриптор и данные макета — сиблинг-entries в том же content-контейнере,
/// где лежат `root`/`version` (не во вложенном контейнере).
fn decode_template(content: &V8File, tmpl_uuid: &str, dest_dir: &Path) -> Result<UnpackOutcome> {
    // Дескриптор — та же обёртка List(vec![...]) + try_inflate, что и header.
    let tmpl_desc_entry = req!(
        content.find(tmpl_uuid),
        format!("нет дескриптора макета \"{tmpl_uuid}\"")
    );
    let mut tmpl_header = V8Value::List(vec![parse_bytes_utf8_or_1251(&try_inflate(
        &tmpl_desc_entry.data,
    ))?]);

    // Тип макета → (имя типа JSON, расширение файла данных). scheme(6)/table(0)/base64(1) —
    // копия сырого контейнера {uuid}.0 как есть; active_doc(2)/geographic(5)/design(7)/
    // graphic_scheme(8) — саby-обёртки над тем же decode_scheme_data, что и scheme(6)
    // (отличается только имя типа в JSON); extension(9) — саby-обёртка над decode_base64_data,
    // т.е. как base64(1); text(4) — текстовый файл с нормализацией переводов строк и
    // сохранением BOM (см. отдельную ветку записи данных ниже, saby
    // `Template2.decode_text_data`); html(3) — вложенный base64 внутри скобко-структуры
    // (saby `MetaObject._decode_html_data`), разбирается отдельной веткой ниже.
    let tmpl_type = text_of(req!(
        tmpl_header.path(&[0, 1, 1]),
        "макет: нет пути [0,1,1] (tmpl_type)"
    ));
    let (type_name, data_ext) = match tmpl_type.as_str() {
        "6" => ("scheme", "bin"),
        "0" => ("table", "mxl"),
        "1" => ("base64", "c1b64"),
        "4" => ("text", "txt"),
        "3" => ("html", "html"),
        "2" => ("active_doc", "bin"),
        "5" => ("geographic", "bin"),
        "7" => ("design", "bin"),
        "8" => ("graphic_scheme", "bin"),
        "9" => ("extension", "c1b64"),
        other => {
            return Ok(UnpackOutcome::Unsupported(format!(
                "макет tmpl_type={other} — поддержаны scheme(6), table(0), base64(1), text(4), html(3), \
                 active_doc(2), geographic(5), design(7), graphic_scheme(8), extension(9)"
            )))
        }
    };
    let is_text = tmpl_type == "4";
    let is_html = tmpl_type == "3";

    // decode-header макета: name, name2, comment. Читаем ДО подстановки ниже
    // (все значения — принадлежащие String, иммутабельные заимствования тут
    // же заканчиваются).
    let thdr = req!(
        tmpl_header.path(&[0, 1, 2]),
        "макет: нет пути [0,1,2] (thdr)"
    );
    let name = text_of(req!(thdr.get(2), "макет thdr: нет [2] (name)"));

    let n2 = req!(thdr.get(3), "макет thdr: нет [3] (name2)");
    let name2_count: usize = text_of(req!(n2.get(0), "макет name2: нет count"))
        .parse()
        .unwrap_or(0);
    let mut name2: Vec<(String, String)> = Vec::with_capacity(name2_count);
    for i in 0..name2_count {
        let key = text_of(req!(n2.get(1 + 2 * i), format!("макет name2[{i}]: нет key")));
        let val = text_of(req!(n2.get(2 + 2 * i), format!("макет name2[{i}]: нет val")));
        name2.push((key, val));
    }

    let comment = text_of(req!(thdr.get(4), "макет thdr: нет [4] (comment)"));

    // saby-подстановка: id макета хранится в отдельном файле (Template.id.json),
    // поэтому в заголовке thdr[1][2] (== [0,1,2,1,2] от корня tmpl_header)
    // реальный uuid заменяется литералом-маркером. Якорь: helper.decode_header
    // (`id_in_separate_file=True`). Для верхнего объекта такой подстановки нет.
    if !set_at_path(
        &mut tmpl_header,
        &[0, 1, 2, 1, 2],
        V8Value::Raw("в отдельном файле".to_string()),
    ) {
        return Ok(UnpackOutcome::Unsupported(
            "макет: не удалось подставить маркер id по пути [0,1,2,1,2]".to_string(),
        ));
    }

    // Файл данных {uuid}.0 — НЕОБЯЗАТЕЛЕН: у макета-заглушки (например, table
    // без сохранённого табличного документа) контейнера нет → saby пишет только
    // Template.json + .id.json. При наличии — для scheme/table/base64 пишем как есть
    // (без нормализации): scheme → .bin (контейнер СКД), table → .mxl (MOXCEL),
    // base64 → .c1b64 (скобко-файл {#base64…}). Для text(4) — saby
    // `helper.txt_read_detect_encoding`/`txt_write`: BOM определяется и СНИМАЕТСЯ на
    // чтении, переводы строк нормализуются к `\n`, при записи `\n` → `\r\n` и BOM
    // возвращается обратно, если был обнаружен (в отличие от кода модуля — там BOM на
    // выходе не пишется никогда).
    let data_bin: Option<Vec<u8>> = if is_text || is_html {
        None
    } else {
        content.find(&format!("{tmpl_uuid}.0")).map(|e| try_inflate(&e.data))
    };
    let data_text: Option<Vec<u8>> = if is_text {
        match content.find(&format!("{tmpl_uuid}.0")) {
            None => None,
            Some(e) => {
                let raw = try_inflate(&e.data);
                let (had_bom, no_bom): (bool, &[u8]) = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
                    (true, &raw[3..])
                } else {
                    (false, &raw[..])
                };
                let text = match std::str::from_utf8(no_bom) {
                    Ok(s) => s,
                    Err(_) => {
                        return Ok(UnpackOutcome::Unsupported(format!(
                            "{tmpl_uuid}.0: не валидный UTF-8 (макет text)"
                        )))
                    }
                };
                let mut out = normalize_newlines(text).replace('\n', "\r\n").into_bytes();
                if had_bom {
                    let mut with_bom = vec![0xEF, 0xBB, 0xBF];
                    with_bom.append(&mut out);
                    out = with_bom;
                }
                Some(out)
            }
        }
    } else {
        None
    };

    // html(3) (saby `MetaObject._decode_html_data`, вызывается как `Template2.decode_html_data`):
    // в отличие от scheme/table/base64 (сырая копия {uuid}.0 как есть), тут {uuid}.0 —
    // скобко-структура, полезная нагрузка (base64) лежит на листе [0,3,0]. Файл > 1 МБ (граница
    // саby) → сырая копия как `Template.bin`, БЕЗ ключа "html" в JSON (saby выходит раньше, чем
    // `self.header['html'] = data`). Иначе — извлекаем base64 (`_extract_b64_data`: префикс
    // `##base64:`/`#base64:`/`#data:`), заменяем payload коротким маркером-префиксом в структуре
    // (мутация, как у кода формы), декодированный бинарь пишем как `Template.html`, а
    // (возможно мутированную) структуру — как JSON-поле "html" (после "comment", перед
    // "obj_version" — порядок вставки ключей в `self.header` у saby).
    let (html_field, html_out): (Option<V8Value>, Option<(Vec<u8>, &str)>) = if !is_html {
        (None, None)
    } else {
        match content.find(&format!("{tmpl_uuid}.0")) {
            None => (None, None),
            Some(e) => {
                let raw = try_inflate(&e.data);
                if raw.len() > 1_000_000 {
                    (None, Some((raw, "bin")))
                } else {
                    let mut html_data = V8Value::List(vec![parse_bytes_utf8_or_1251(&raw)?]);
                    let leaf = html_data.path(&[0, 3, 0]).map(text_of);
                    let bin = match leaf.as_deref() {
                        Some(s) if !s.is_empty() => {
                            let (prefix, payload): (&str, &str) =
                                if let Some(p) = s.strip_prefix("##base64:") {
                                    ("##base64:", p)
                                } else if let Some(p) = s.strip_prefix("#base64:") {
                                    ("#base64:", p)
                                } else if let Some(p) = s.strip_prefix("#data:") {
                                    ("#data:", p)
                                } else {
                                    return Ok(UnpackOutcome::Unsupported(format!(
                                        "макет html: неизвестный префикс base64-данных ({s:?})"
                                    )));
                                };
                            let decoded = match BASE64.decode(payload) {
                                Ok(d) => d,
                                Err(_) => {
                                    return Ok(UnpackOutcome::Unsupported(
                                        "макет html: невалидный base64".to_string(),
                                    ))
                                }
                            };
                            if !set_at_path(
                                &mut html_data,
                                &[0, 3, 0],
                                V8Value::Raw(prefix.to_string()),
                            ) {
                                return Ok(UnpackOutcome::Unsupported(
                                    "макет html: не удалось подставить маркер по пути [0,3,0]"
                                        .to_string(),
                                ));
                            }
                            Some(decoded)
                        }
                        _ => None,
                    };
                    (Some(html_data), bin.map(|b| (b, "html")))
                }
            }
        }
    };

    // Template.json (порядок ключей фиксирован; "html" — только для tmpl_type=3, вставляется
    // между "comment" и "obj_version", см. комментарий выше).
    let name2_json: Vec<(String, J)> = name2.into_iter().map(|(k, v)| (k, J::Str(v))).collect();
    let mut template_entries: Vec<(String, J)> = vec![
        ("type".to_string(), J::Str(type_name.to_string())),
        ("header".to_string(), saby_json(&tmpl_header)),
        ("name".to_string(), J::Str(name.clone())),
        ("name2".to_string(), J::Obj(name2_json)),
        ("comment".to_string(), J::Str(comment)),
    ];
    if let Some(h) = &html_field {
        template_entries.push(("html".to_string(), saby_json(h)));
    }
    template_entries.push(("obj_version".to_string(), J::Str("2".to_string())));
    let template_obj = J::Obj(template_entries);
    let template_json = template_obj.to_pretty_string().replace('\n', "\r\n");

    // Template.id.json.
    let id_obj = J::Obj(vec![("uuid".to_string(), J::Str(tmpl_uuid.to_string()))]);
    let id_json = id_obj.to_pretty_string().replace('\n', "\r\n");

    // Каталог: dest_dir/Template/{имя объекта}/. «Template» — имя класса
    // метаданных, имя конкретного макета — подкаталог.
    let target = dest_dir.join("Template").join(&name);
    std::fs::create_dir_all(&target)?;
    if let Some(bin) = &data_bin {
        std::fs::write(target.join(format!("Template.{data_ext}")), bin)?;
    }
    if let Some(text) = &data_text {
        std::fs::write(target.join(format!("Template.{data_ext}")), text)?;
    }
    if let Some((bytes, ext)) = &html_out {
        std::fs::write(target.join(format!("Template.{ext}")), bytes)?;
    }
    std::fs::write(target.join("Template.json"), template_json.as_bytes())?;
    std::fs::write(target.join("Template.id.json"), id_json.as_bytes())?;

    Ok(UnpackOutcome::Done)
}

/// Версии Form9 (FormElements4), которые распаковщик умеет разбирать.
const SUPPORTED_FORM_VERSIONS: &[&str] = &["9", "12", "13", "14"];

/// saby `FormCore.decode_form1`: если у формы есть сиблинг-entry `{form_uuid}.1`,
/// прочитать её и вернуть как ВТОРОЙ элемент списка `form` (вызывается БЕЗУСЛОВНО, после
/// `decode_form0`/`decode_form0_from_file`, независимо от Тип формы — общее и для
/// управляемой, и для обычной формы). Обёрнут в один уровень списка так же, как `fh`/`fc`
/// (см. их комментарии) — `helper.brace_file_read` возвращает «список групп верхнего
/// уровня», а не голое содержимое. `None`, если сиблинга нет — тогда `form` остаётся
/// списком из одного элемента (`[form0]`), как было до этого исправления.
fn read_form1(content: &V8File, form_uuid: &str) -> Result<Option<V8Value>> {
    match content.find(&format!("{form_uuid}.1")) {
        Some(entry) => Ok(Some(V8Value::List(vec![parse_bytes_utf8_or_1251(
            &try_inflate(&entry.data),
        )?]))),
        None => Ok(None),
    }
}

/// Распаковать вложенную управляемую форму (Тип формы=1; include-тип может быть как
/// `d5b0e5ed`/Form, так и `a3b368c0`/ReportForm — у saby это один и тот же
/// `FormCore`/`Form9`, разница только в `meta_obj_class.__name__`, см. `class_name`) в
/// `dest_dir/{class_name}/{имя формы}/`: `{class_name}.json` (описатель),
/// `{class_name}.elem.json` (params/props/commands/tree/data), `{class_name}.id.json`
/// (UUID), `{class_name}.obj.bsl` (код формы). Поддержана Form9/FormElements4 с
/// параметрами, реквизитами, командами и деревом элементов типов Field/Button/
/// Decoration/ItemAddition/Group/Table (Group/Table — рекурсивно, с разрешением
/// «ПутьКДанным»/«ИмяКоманды»). Прочие типы элементов и многоуровневый путь к данным →
/// `Unsupported`.
///
/// Дескриптор формы (`{form_uuid}`) и её содержимое (`{form_uuid}.0`) — те же
/// сиблинг-entries в content-контейнере, что и у макета (`decode_template`).
///
/// `parent_container_uuid` — UUID контейнера метаданных объекта (см.
/// `unpack_epf_skeleton`, DHC[1]), на который в реквизитах формы («Pattern»)
/// ссылается «родительский» тип — при совпадении подставляется маркер
/// `"Родитель"` (аналог saby `ExternalDataProcessor.get_container_uuid`).
fn decode_form(
    content: &V8File,
    form_uuid: &str,
    parent_container_uuid: &str,
    class_name: &str,
    dest_dir: &Path,
) -> Result<UnpackOutcome> {
    // ─── заголовок формы (дескриптор — сиблинг-entry, как у макета) ────────
    let fh_entry = req!(
        content.find(form_uuid),
        format!("нет дескриптора формы \"{form_uuid}\"")
    );
    let mut fh = V8Value::List(vec![parse_bytes_utf8_or_1251(&try_inflate(&fh_entry.data))?]);

    if text_of(req!(fh.path(&[0, 1, 0]), "форма: нет пути [0,1,0]")) != "1" {
        return Ok(UnpackOutcome::Unsupported(
            "форма: неподдержанный obj_version node".to_string(),
        ));
    }

    let version = text_of(req!(
        fh.path(&[0, 1, 1, 1, 0]),
        "форма: нет пути [0,1,1,1,0] (version)"
    ));
    if !SUPPORTED_FORM_VERSIONS.contains(&version.as_str()) {
        return Ok(UnpackOutcome::Unsupported(format!(
            "форма: версия {version} не поддержана (ожидался Form9 {SUPPORTED_FORM_VERSIONS:?})"
        )));
    }

    let form_kind = text_of(req!(
        fh.path(&[0, 1, 1, 1, 3]),
        "форма: нет пути [0,1,1,1,3] (Тип формы)"
    ));
    if form_kind != "1" {
        return Ok(UnpackOutcome::Unsupported(format!(
            "форма: тип формы {form_kind} не поддержан (ожидался FormElements4=1)"
        )));
    }

    let uuid = text_of(req!(
        fh.path(&[0, 1, 1, 1, 1, 1, 2]),
        "форма: нет пути [0,1,1,1,1,1,2] (uuid)"
    ));
    let name = text_of(req!(
        fh.path(&[0, 1, 1, 1, 1, 2]),
        "форма: нет пути [0,1,1,1,1,2] (name)"
    ));

    let n2 = req!(
        fh.path(&[0, 1, 1, 1, 1, 3]),
        "форма: нет пути [0,1,1,1,1,3] (name2)"
    );
    let name2_count: usize = text_of(req!(n2.get(0), "форма name2: нет count"))
        .parse()
        .unwrap_or(0);
    let mut name2: Vec<(String, String)> = Vec::with_capacity(name2_count);
    for i in 0..name2_count {
        let key = text_of(req!(n2.get(1 + 2 * i), format!("форма name2[{i}]: нет key")));
        let val = text_of(req!(n2.get(2 + 2 * i), format!("форма name2[{i}]: нет val")));
        name2.push((key, val));
    }

    let comment = text_of(req!(
        fh.path(&[0, 1, 1, 1, 1, 4]),
        "форма: нет пути [0,1,1,1,1,4] (comment)"
    ));

    // saby-подстановка: id формы хранится в отдельном файле (Form.id.json).
    if !set_at_path(
        &mut fh,
        &[0, 1, 1, 1, 1, 1, 2],
        V8Value::Raw("в отдельном файле".to_string()),
    ) {
        return Ok(UnpackOutcome::Unsupported(
            "форма: не удалось подставить маркер id по пути [0,1,1,1,1,1,2]".to_string(),
        ));
    }

    // ─── содержимое формы ({form_uuid}.0): код, реквизиты, дерево ──────────
    let fc_entry = req!(
        content.find(&format!("{form_uuid}.0")),
        format!("нет содержимого формы \"{form_uuid}.0\"")
    );
    let mut fc = V8Value::List(vec![parse_bytes_utf8_or_1251(&try_inflate(&fc_entry.data))?]);

    // Код — строковый литерал внутри C[2]; парсер уже вернул чистый текст
    // (внешние кавычки сняты, `""` раскрыты — см. `serlist::Parser::parse_string`).
    let code_text_raw = text_of(req!(fc.path(&[0, 2]), "форма: нет C[2] (код)"));
    let code_obj = normalize_newlines(&code_text_raw);
    if !set_at_path(
        &mut fc,
        &[0, 2],
        V8Value::Raw("Код в отдельном файле".to_string()),
    ) {
        return Ok(UnpackOutcome::Unsupported(
            "форма: не удалось подставить маркер кода по пути [0,2]".to_string(),
        ));
    }

    // Реквизиты формы (FormProps, C[3]): count в C[3][1], сами реквизиты —
    // C[3][2..2+count]. Подстановка «Родитель» — в узле raw[5][1][1]
    // (Pattern), если он совпадает с parent_container_uuid.
    let props_count_text = text_of(req!(
        fc.path(&[0, 3, 1]),
        "форма: нет C[3][1] (props count)"
    ));
    let props_count: usize = match props_count_text.parse() {
        Ok(n) => n,
        Err(_) => {
            return Ok(UnpackOutcome::Unsupported(format!(
                "форма: props count не число: {props_count_text:?}"
            )))
        }
    };

    let mut props_json: Vec<J> = Vec::with_capacity(props_count);
    // Индекс id → имя реквизита — для разрешения «ПутьКДанным» элементов формы
    // (saby `FormElements4.create_prop_index_by_id`). Дочерние реквизиты не
    // индексируются: этот инкремент поддерживает только одноуровневый путь.
    let mut props_index: std::collections::HashMap<String, PropNode> =
        std::collections::HashMap::new();
    for i in 0..props_count {
        let pattern_tag_path = [0usize, 3, 2 + i, 5, 1, 0];
        let pattern_uuid_path = [0usize, 3, 2 + i, 5, 1, 1];
        let is_parent_pattern = fc.path(&pattern_tag_path).map(text_of).as_deref() == Some("#")
            && fc.path(&pattern_uuid_path).map(text_of).as_deref() == Some(parent_container_uuid);
        if is_parent_pattern {
            set_at_path(
                &mut fc,
                &pattern_uuid_path,
                V8Value::Raw("Родитель".to_string()),
            );
        }

        // Дочерние реквизиты (колонки реквизита-таблицы и т.п.): saby
        // `FormProps.decode` — child_count в raw[13] (child_offset), сами дети в
        // raw[14..14+cc]. Извлекаем их (id/имя/raw) ДО мутации, затем в raw
        // родителя ставим маркер «отдельно» и удаляем детей (как saby); дети
        // уходят в отдельный ключ `child`. id ребёнка = raw[1] (скаляр), тогда
        // как id родителя = raw[1][0]. Нужно для многоуровневого «ПутьКДанным».
        let child_count: usize = fc
            .path(&[0, 3, 2 + i, 13])
            .map(text_of)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let mut child_json: Vec<J> = Vec::with_capacity(child_count);
        let mut child_map: std::collections::HashMap<String, PropNode> =
            std::collections::HashMap::new();
        for j in 0..child_count {
            let craw = req!(
                fc.path(&[0, 3, 2 + i, 14 + j]),
                format!("форма: props[{i}] нет child[{j}]")
            );
            let cname = text_of(req!(
                craw.get(3),
                format!("форма: props[{i}] child[{j}] нет name")
            ));
            let cid = text_of(req!(
                craw.get(1),
                format!("форма: props[{i}] child[{j}] нет id")
            ));
            child_json.push(J::Obj(vec![
                ("name".to_string(), J::Str(cname.clone())),
                ("id".to_string(), J::Str(cid.clone())),
                ("raw".to_string(), saby_json(craw)),
            ]));
            child_map.insert(
                cid,
                PropNode {
                    name: cname,
                    child: std::collections::HashMap::new(),
                },
            );
        }
        if child_count > 0 {
            let fc_outer = req!(as_list_mut(&mut fc), "форма: fc не список (props child)");
            let c_items = req!(
                fc_outer.get_mut(0).and_then(as_list_mut),
                "форма: C не список (props child)"
            );
            let props_items = req!(
                c_items.get_mut(3).and_then(as_list_mut),
                "форма: C[3] не список (props child)"
            );
            let raw_list = req!(
                props_items.get_mut(2 + i).and_then(as_list_mut),
                format!("форма: props[{i}] не список (child drain)")
            );
            if raw_list.len() < 14 + child_count {
                return Ok(UnpackOutcome::Unsupported(format!(
                    "форма: props[{i}] короче ожидаемого для {child_count} детей"
                )));
            }
            raw_list[13] = V8Value::Raw("отдельно".to_string());
            raw_list.drain(14..14 + child_count);
        }

        let raw_node = req!(fc.path(&[0, 3, 2 + i]), format!("форма: нет props[{i}]"));
        let prop_name = text_of(req!(raw_node.get(3), format!("форма: props[{i}] нет name")));
        let prop_id = text_of(req!(
            raw_node.path(&[1, 0]),
            format!("форма: props[{i}] нет id")
        ));
        props_index.insert(
            prop_id.clone(),
            PropNode {
                name: prop_name.clone(),
                child: child_map,
            },
        );
        let mut entry = vec![
            ("name".to_string(), J::Str(prop_name)),
            ("id".to_string(), J::Str(prop_id)),
            ("raw".to_string(), saby_json(raw_node)),
        ];
        if !child_json.is_empty() {
            entry.push(("child".to_string(), J::Arr(child_json)));
        }
        props_json.push(J::Obj(entry));
    }

    // saby-подстановка: маркер + удаление распакованных реквизитов из C[3].
    {
        let fc_outer = req!(as_list_mut(&mut fc), "форма: fc не список");
        let c_items = req!(
            fc_outer.get_mut(0).and_then(as_list_mut),
            "форма: C не список"
        );
        let props_items = req!(
            c_items.get_mut(3).and_then(as_list_mut),
            "форма: C[3] не список (props)"
        );
        if props_items.len() < 2 + props_count {
            return Ok(UnpackOutcome::Unsupported(
                "форма: props_container короче ожидаемого".to_string(),
            ));
        }
        props_items[1] = V8Value::Raw("Дочерние элементы отдельно".to_string());
        props_items.drain(2..2 + props_count);
    }

    // Параметры формы (FormParams, C[4]): count в C[4][1], сами параметры —
    // C[4][2..2+count]. У параметра name = str_decode(raw[1]) (saby `_FormRoot.decode`,
    // `FormParams.index_name=1`) — БЕЗ id/child, в отличие от реквизитов (FormProps).
    let params_count_text = text_of(req!(
        fc.path(&[0, 4, 1]),
        "форма: нет C[4][1] (params count)"
    ));
    let params_count: usize = match params_count_text.parse() {
        Ok(n) => n,
        Err(_) => {
            return Ok(UnpackOutcome::Unsupported(format!(
                "форма: params count не число: {params_count_text:?}"
            )))
        }
    };
    let mut params_json_items: Vec<J> = Vec::with_capacity(params_count);
    for i in 0..params_count {
        let raw_node = req!(fc.path(&[0, 4, 2 + i]), format!("форма: нет params[{i}]"));
        let param_name = text_of(req!(
            raw_node.get(1),
            format!("форма: params[{i}] нет name")
        ));
        params_json_items.push(J::Obj(vec![
            ("name".to_string(), J::Str(param_name)),
            ("raw".to_string(), saby_json(raw_node)),
        ]));
    }
    // saby-подстановка: маркер + удаление распакованных параметров из C[4]. При пустом
    // контейнере (count=0) saby `_FormRoot.decode_list` выходит рано и маркер НЕ ставит —
    // сохраняем C[4][1]="0" как есть (симметрично уже реализованному для команд, C[5]).
    if params_count > 0 {
        let fc_outer = req!(as_list_mut(&mut fc), "форма: fc не список (params)");
        let c_items = req!(
            fc_outer.get_mut(0).and_then(as_list_mut),
            "форма: C не список (params)"
        );
        let params_items = req!(
            c_items.get_mut(4).and_then(as_list_mut),
            "форма: C[4] не список (params)"
        );
        if params_items.len() < 2 + params_count {
            return Ok(UnpackOutcome::Unsupported(
                "форма: params_container короче ожидаемого".to_string(),
            ));
        }
        params_items[1] = V8Value::Raw("Дочерние элементы отдельно".to_string());
        params_items.drain(2..2 + params_count);
    }
    let params_json = if params_count == 0 {
        J::Null
    } else {
        J::Arr(params_json_items)
    };

    // Команды формы (FormCommands, C[5]): count в C[5][1], сами команды —
    // C[5][2..2+count]. У команды name = str_decode(raw[2]), id = raw[1][0]
    // (saby `FormCommands.decode`). Индекс id → имя нужен для «ИмяКоманды»
    // элементов-кнопок (saby `create_commands_index_by_id`).
    let commands_count_text = text_of(req!(
        fc.path(&[0, 5, 1]),
        "форма: нет C[5][1] (commands count)"
    ));
    let commands_count: usize = match commands_count_text.parse() {
        Ok(n) => n,
        Err(_) => {
            return Ok(UnpackOutcome::Unsupported(format!(
                "форма: commands count не число: {commands_count_text:?}"
            )))
        }
    };
    let mut commands_json: Vec<J> = Vec::with_capacity(commands_count);
    let mut commands_index: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for i in 0..commands_count {
        let raw_node = req!(fc.path(&[0, 5, 2 + i]), format!("форма: нет commands[{i}]"));
        let cmd_name = text_of(req!(
            raw_node.get(2),
            format!("форма: commands[{i}] нет name")
        ));
        let cmd_id = text_of(req!(
            raw_node.path(&[1, 0]),
            format!("форма: commands[{i}] нет id")
        ));
        commands_index.insert(cmd_id.clone(), cmd_name.clone());
        commands_json.push(J::Obj(vec![
            ("name".to_string(), J::Str(cmd_name)),
            ("id".to_string(), J::Str(cmd_id)),
            ("raw".to_string(), saby_json(raw_node)),
        ]));
    }
    // saby-подстановка: маркер + удаление распакованных команд из C[5]. При
    // пустом контейнере (count=0) saby `_FormRoot.decode_list` выходит рано и
    // маркер НЕ ставит — сохраняем C[5][1]="0" как есть.
    if commands_count > 0 {
        let fc_outer = req!(as_list_mut(&mut fc), "форма: fc не список");
        let c_items = req!(
            fc_outer.get_mut(0).and_then(as_list_mut),
            "форма: C не список"
        );
        let cmd_items = req!(
            c_items.get_mut(5).and_then(as_list_mut),
            "форма: C[5] не список (commands)"
        );
        if cmd_items.len() < 2 + commands_count {
            return Ok(UnpackOutcome::Unsupported(
                "форма: commands_container короче ожидаемого".to_string(),
            ));
        }
        cmd_items[1] = V8Value::Raw("Дочерние элементы отдельно".to_string());
        cmd_items.drain(2..2 + commands_count);
    }
    let commands_field = if commands_count == 0 {
        J::Null
    } else {
        J::Arr(commands_json)
    };

    // Дерево/данные формы — по root_data (C[1]). Индекс командной панели —
    // saby `calc_offset([(18,2),(3,0)], root_data)` (FormElements4.py:86) =
    // 18 + int(root_data[18]) * 2 + 3; корневые элементы дерева — пары
    // (uuid типа, raw) начиная с index_root_elem+1. Разбор рекурсивный
    // (`decode_form_elements`): группы вкладываются через `child`, ключи data
    // — по пути «Группа/Элемент». Поддержаны Field/Button/Group; прочее →
    // Unsupported → фолбэк.
    let index_root_elem = {
        let root_data = req!(fc.path(&[0, 1]), "форма: нет C[1] (root_data)");
        let root_items = req!(root_data.as_list(), "форма: C[1] не список (root_data)");
        let count18 = req!(int_at(root_items, 18), "форма: root_data[18] не число");
        let index_command_panel = (18 + count18 * 2 + 3) as usize;
        let panel_count = req!(
            int_at(root_items, index_command_panel),
            "форма: нет command_panel_count"
        );
        index_command_panel + panel_count as usize + 1
    };

    let mut data_entries: Vec<(String, J)> = Vec::new();
    let tree_json = {
        let fc_outer = req!(as_list_mut(&mut fc), "форма: fc не список");
        let c_items = req!(
            fc_outer.get_mut(0).and_then(as_list_mut),
            "форма: C не список"
        );
        let root_items = req!(
            c_items.get_mut(1).and_then(as_list_mut),
            "форма: C[1] не список (root_data)"
        );
        match decode_form_elements(
            root_items,
            index_root_elem,
            "",
            &props_index,
            &commands_index,
            &mut data_entries,
        )? {
            ElemResult::Tree(t) => t.unwrap_or_default(),
            ElemResult::Fallback(reason) => return Ok(UnpackOutcome::Unsupported(reason)),
        }
    };

    // data — dict(sorted(...)) по ключу (path-имени элемента), saby-порядок.
    data_entries.sort_by(|a, b| a.0.cmp(&b.0));
    let tree_field = J::Arr(tree_json);
    let data_field = J::Obj(data_entries);

    // ─── сборка {class_name}.json / .elem.json / .id.json / .obj.bsl ──────
    let name2_json: Vec<(String, J)> = name2.into_iter().map(|(k, v)| (k, J::Str(v))).collect();

    let mut form_items = vec![fc];
    if let Some(f1) = read_form1(content, form_uuid)? {
        form_items.push(f1);
    }
    let form_wrapped = V8Value::List(form_items);
    let form_field = saby_json(&form_wrapped);

    let form_obj = J::Obj(vec![
        ("name".to_string(), J::Str(name.clone())),
        ("name2".to_string(), J::Obj(name2_json)),
        ("comment".to_string(), J::Str(comment)),
        ("header".to_string(), saby_json(&fh)),
        ("Тип формы".to_string(), J::Str(form_kind)),
        (
            "code_info_obj".to_string(),
            J::Str("Код в отдельном файле".to_string()),
        ),
        ("form".to_string(), form_field),
        (
            "Версия элементов формы".to_string(),
            J::Str("1".to_string()),
        ),
        ("obj_version".to_string(), J::Str(version)),
    ]);
    let form_json_text = form_obj.to_pretty_string().replace('\n', "\r\n");

    let props_field = if props_count == 0 {
        J::Null
    } else {
        J::Arr(props_json)
    };
    let elem_obj = J::Obj(vec![
        ("params".to_string(), params_json),
        ("props".to_string(), props_field),
        ("commands".to_string(), commands_field),
        ("tree".to_string(), tree_field),
        ("data".to_string(), data_field),
    ]);
    let elem_json_text = elem_obj.to_pretty_string().replace('\n', "\r\n");

    let id_obj = J::Obj(vec![("uuid".to_string(), J::Str(uuid))]);
    let id_json_text = id_obj.to_pretty_string().replace('\n', "\r\n");

    let bsl_text = strip_include_areas(&code_obj).replace('\n', "\r\n");

    let target = dest_dir.join(class_name).join(&name);
    std::fs::create_dir_all(&target)?;
    std::fs::write(target.join(format!("{class_name}.json")), form_json_text.as_bytes())?;
    std::fs::write(target.join(format!("{class_name}.elem.json")), elem_json_text.as_bytes())?;
    std::fs::write(target.join(format!("{class_name}.id.json")), id_json_text.as_bytes())?;
    std::fs::write(target.join(format!("{class_name}.obj.bsl")), bsl_text.as_bytes())?;

    Ok(UnpackOutcome::Done)
}

// ─── обычная форма (ReportForm, FormElements26/27) ─────────────────────────

/// UUID типа элемента формы «Панель» (FormElements26/27, `FormItemTypes.Panel`). Корневой узел
/// дерева формы и вложенные группы (панели с собственным набором страниц) используют этот тип —
/// для него нужен рекурсивный разбор; для остальных типов действует общая («листовая») схема.
const RF_PANEL_TYPE_UUID: &str = "09ccdc77-ea1a-4a6d-ab1c-3435eada2433";

/// Версии обычной формы (`form_root[1][0]`, saby `Form.versions`), которые распаковщик умеет
/// разбирать. `Form5`/`Form9` в saby различаются только выбором класса-обработчика — сама логика
/// `FormCore`, которую мы портируем, у них идентична (`Form5`/`Form9` не переопределяют
/// `decode_data`/`decode_includes`).
const RF_SUPPORTED_VERSIONS: &[&str] = &["5", "7", "9", "12", "13", "14"];

/// Имя типа элемента обычной формы (`FormItemTypes`, FormElements26/27) по UUID. `None` —
/// неизвестный тип (saby `ValueError` → `ExtException`, у нас сигнал фолбэка).
fn rf_item_type_name(uuid: &str) -> Option<&'static str> {
    Some(match uuid {
        "381ed624-9217-4e63-85db-c4c3cb87daae" => "Field",
        "35af3d93-d7c7-4a2e-a8eb-bac87a1a3f26" => "CheckBox",
        "782e569a-79a7-4a4f-a936-b48d013936ec" => "RadioBtn",
        "64483e7f-3833-48e2-8c75-2c31aac49f6e" => "SelectField",
        "e69bf21d-97b2-4f37-86db-675aea9ec2cb" => "CommandPanel",
        "6ff79819-710e-4145-97cd-1618da79e3e2" => "Button",
        "151ef23e-6bb2-4681-83d0-35bc2217230c" => "Image",
        "90db814a-c75f-4b54-bc96-df62e554d67d" => "Group",
        "ea83fe3a-ac3c-4cce-8045-3dddf35b28b1" => "Table",
        "236a17b3-7f44-46d9-a907-75f9cdc61ab5" => "TableField",
        "09ccdc77-ea1a-4a6d-ab1c-3435eada2433" => "Panel",
        "0fc7e20d-f241-460c-bdf4-5ad88e5474a5" => "Label",
        "19f8b798-314e-4b4e-8121-905b2a7a03f5" => "ListField",
        "36e52348-5d60-4770-8e89-a16ed50a2006" => "Separator",
        "d92a805c-98ae-4750-9158-d9ce7cec2f20" => "FieldHtml",
        "b1db1f86-abbb-4cf0-8852-fe6ae21650c2" => "Indicator",
        "e3c063d8-ef92-41be-9c89-b70290b5368b" => "CalendarBox",
        "6c06cd5d-8481-4b6f-a90a-7a97a8bb8bef" => "TrackBar",
        "14c4a229-bfc3-42fe-9ce1-2da049fd0109" => "TextDocumentField",
        "42248403-7748-49da-b782-e4438fd7bff3" => "GraphicalSchemaField",
        "ad37194e-555e-4305-b718-5dca84baf145" => "GeographicalSchemaField",
        "a8b97779-1a4b-4059-b09c-807f86d2a461" => "Chart",
        "e5fdc112-5c84-4a16-9728-72b85692b6e2" => "GanttChart",
        "a26da99e-184a-4823-b0d6-62816d38dc4e" => "PivotChart",
        "984981b1-622d-4ebc-94f7-885f0cdfb59a" => "Dendrogram",
        _ => return None,
    })
}

/// Пошагово войти в дерево `V8Value::List` по `path` (индексы) и вернуть `&mut Vec<V8Value>`
/// последнего уровня — сокращение для цепочки `get_mut(i).and_then(as_list_mut)` на несколько
/// уровней подряд (нужно для глубоких путей внутри разбора обычной формы: `fc[0][2][2]` и т.п.).
fn list_mut_path<'a>(list: &'a mut Vec<V8Value>, path: &[usize]) -> Option<&'a mut Vec<V8Value>> {
    let mut cur = list;
    for &i in path {
        cur = cur.get_mut(i).and_then(as_list_mut)?;
    }
    Some(cur)
}

/// saby `Panel.calc_id`: путь-ключ из непустых частей, соединённых `/` (Python truthy — пустая
/// строка в часть не идёт).
fn calc_id(path: &str, page: Option<&str>, name: Option<&str>) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !path.is_empty() {
        parts.push(path);
    }
    if let Some(p) = page {
        if !p.is_empty() {
            parts.push(p);
        }
    }
    if let Some(n) = name {
        if !n.is_empty() {
            parts.push(n);
        }
    }
    parts.join("/")
}

/// Результат шага разбора обычной формы (FormElements26/27): полезная нагрузка либо сигнал
/// фолбэка на v8unpack.exe (структура не соответствует ожидаемому скелету — не воспроизводим).
enum RfResult<T> {
    Value(T),
    Fallback(String),
}

/// Аналог `req!` для функций с сигнатурой `Result<RfResult<T>>` (не `Result<UnpackOutcome>`).
macro_rules! rf_req {
    ($opt:expr, $ctx:expr) => {
        match $opt {
            Some(v) => v,
            None => return Ok(RfResult::Fallback($ctx.to_string())),
        }
    };
}

/// Общий контекст разбора элементов обычной формы: различия FormElements26 (0-26, платформа
/// 801) и FormElements27 (0-27) у saby сведены ровно к двум значениям (`FormProps.name_index`,
/// `Panel.ver`) — весь остальной код `FormElements27`/`Panel` идентичен, `FormElements26` лишь
/// переопределяет их.
struct RfCtx<'a> {
    /// Версия элементов (26 или 27) — пишется в `elements_data` как `"ver"`.
    ver: i64,
    /// Индекс «id элемента формы → имя реквизита» (saby `create_prop_index_by_elem_id`), общий
    /// на все уровни вложенности (панели/группы разделяют один и тот же индекс).
    props_by_elem_id: &'a std::collections::HashMap<String, String>,
}

/// saby `FormProps.decode_list` (FormElements26/27): разобрать реквизиты формы. `container` —
/// `raw_data[2][2]` формы (список `[count, prop0, prop1, ...]`). `name_index` — смещение имени
/// в `raw` реквизита (4 для FormElements27, 3 для FormElements26). Возвращает JSON-список
/// `[{name,id,raw}, ...]` (`None` при пустом контейнере — saby `if not element_count: return`) и
/// карту `id реквизита → имя` (для построения индекса элемент→реквизит). Мутирует `container`:
/// `[0]` → маркер «Дочерние элементы отдельно», реквизиты удаляются.
fn rf_decode_props(
    container: &mut Vec<V8Value>,
    name_index: usize,
) -> Result<RfResult<(Option<Vec<J>>, std::collections::HashMap<String, String>)>> {
    let count = match int_at(container.as_slice(), 0) {
        Some(n) if n >= 0 => n as usize,
        _ => return Ok(RfResult::Fallback("форма: счётчик реквизитов не число".to_string())),
    };
    if count == 0 {
        return Ok(RfResult::Value((None, std::collections::HashMap::new())));
    }
    if container.len() < 1 + count {
        return Ok(RfResult::Fallback(
            "форма: контейнер реквизитов короче ожидаемого".to_string(),
        ));
    }
    let mut props_json: Vec<J> = Vec::with_capacity(count);
    let mut by_id: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for i in 0..count {
        let raw_node = &container[1 + i];
        let list = rf_req!(raw_node.as_list(), format!("форма: реквизит[{i}] не список"));
        let name = rf_req!(
            list.get(name_index).map(text_of),
            format!("форма: реквизит[{i}] нет имени по смещению {name_index}")
        );
        let id = rf_req!(
            list.first()
                .and_then(|v| v.as_list())
                .and_then(|l| l.first())
                .map(text_of),
            format!("форма: реквизит[{i}] нет id")
        );
        by_id.insert(id.clone(), name.clone());
        props_json.push(J::Obj(vec![
            ("name".to_string(), J::Str(name)),
            ("id".to_string(), J::Str(id)),
            ("raw".to_string(), saby_json(raw_node)),
        ]));
    }
    container[0] = V8Value::Raw("Дочерние элементы отдельно".to_string());
    container.drain(1..1 + count);
    Ok(RfResult::Value((Some(props_json), by_id)))
}

/// saby `FormElements27.create_prop_index_by_elem_id`: индекс «id элемента формы → имя
/// реквизита» по `form_data[2][3]` (список `[count, [elem_id, [_, [prop_id]]], ...]`,
/// заполняется saby при СБОРКЕ (`fill_datasource`) — при разборе только читается, не мутируется).
/// Отсутствие реквизита с нужным id в `by_id` — не ошибка, запись просто пропускается (saby
/// `except KeyError: pass`).
fn rf_build_datasource_index(
    datasource: &V8Value,
    by_id: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    let mut result = std::collections::HashMap::new();
    let list = match datasource.as_list() {
        Some(l) => l,
        None => return result,
    };
    let count = match int_at(list, 0) {
        Some(n) if n >= 0 => n as usize,
        _ => return result,
    };
    for i in 0..count {
        let entry = match list.get(1 + i).and_then(|v| v.as_list()) {
            Some(l) => l,
            None => continue,
        };
        let elem_id = match entry.first().map(text_of) {
            Some(s) => s,
            None => continue,
        };
        let prop_id = match entry
            .get(1)
            .and_then(|v| v.as_list())
            .and_then(|l| l.get(1))
            .and_then(|v| v.as_list())
            .and_then(|l| l.first())
            .map(text_of)
        {
            Some(s) => s,
            None => continue,
        };
        if let Some(name) = by_id.get(&prop_id) {
            result.insert(elem_id, name.clone());
        }
    }
    result
}

/// saby `Panel.decode_pages` (FormElements26/27): разобрать список страниц панели.
/// `pages_parent` — контейнер, в котором лежит pages_raw_data (`elem_raw_data[1+is_child][1]`
/// вызывающего). Возвращает имена страниц по порядку (`self.pages`) и пишет по одной записи в
/// `data` на страницу + маркер `-pages-`. Мутирует `pages_parent`: контейнер страниц схлопывается
/// до `[версия_формата]` (Python прощает срез за границами списка — итоговая длина ВСЕГДА 1,
/// независимо от количества страниц), запись счётчика page_info заменяется маркером «в отдельном
/// файле» с удалением самих page_info записей.
fn rf_decode_pages(
    pages_parent: &mut Vec<V8Value>,
    path: &str,
    ver: i64,
    data: &mut Vec<(String, J)>,
) -> Result<RfResult<Vec<String>>> {
    let pages_offset = match calc_offset(
        &[(2, 1), (1, 1), (1, 1), (1, 1), (1, 1), (1, 1), (4, 0)],
        pages_parent.as_slice(),
    ) {
        Some(o) => o,
        None => {
            return Ok(RfResult::Fallback(
                "форма: не нашли описание страниц элементов формы".to_string(),
            ))
        }
    };

    let mut page_names: Vec<String> = Vec::new();
    {
        let pages_raw_list = rf_req!(
            pages_parent.get_mut(pages_offset).and_then(as_list_mut),
            "форма: контейнер страниц не список"
        );
        let page_count = match int_at(pages_raw_list.as_slice(), 1) {
            Some(n) if n >= 0 => n as usize,
            _ => return Ok(RfResult::Fallback("форма: счётчик страниц не число".to_string())),
        };
        if pages_raw_list.len() < 2 + page_count {
            return Ok(RfResult::Fallback(
                "форма: контейнер страниц короче ожидаемого".to_string(),
            ));
        }
        for i in 0..page_count {
            let raw_page = &pages_raw_list[2 + i];
            let raw_page_list = rf_req!(raw_page.as_list(), format!("форма: страница[{i}] не список"));
            let page_format_version = rf_req!(
                raw_page_list.first().map(text_of),
                format!("форма: страница[{i}] нет версии формата")
            );
            let page_name = rf_req!(
                raw_page_list.get(6).map(text_of),
                format!("форма: страница[{i}] нет имени")
            );
            let elem_id = calc_id(path, Some(&page_name), None);
            data.push((
                elem_id,
                J::Obj(vec![
                    ("ver".to_string(), J::Num(ver)),
                    ("page_format_version".to_string(), J::Str(page_format_version)),
                    ("raw".to_string(), saby_json(raw_page)),
                ]),
            ));
            page_names.push(page_name);
        }
        // saby: `del pages_raw_data[1:1+2+page_count]` — срез всегда клипуется до конца списка
        // (1+2+page_count >= len(pages_raw_data)), итог — ровно [версия_формата].
        let end = (1 + 2 + page_count).min(pages_raw_list.len());
        pages_raw_list.drain(1..end);
    }
    let page_count = page_names.len();

    // decode_info: page_info записи — в РОДИТЕЛЕ (pages_parent), 4 позиции спустя контейнер
    // страниц (pages_offset+4).
    let pages_info_offset = pages_offset + 4;
    let pages_info_count = match int_at(pages_parent.as_slice(), pages_info_offset) {
        Some(n) if n >= 0 => n as usize,
        _ => return Ok(RfResult::Fallback("форма: счётчик page_info не число".to_string())),
    };
    let extra: i64 = pages_info_count as i64 - (page_count as i64) * 4;
    for (i, page_name) in page_names.iter().enumerate() {
        let (offset_i, take_i): (i64, i64) = if i == 0 {
            (1 + pages_info_offset as i64, 4 + extra)
        } else {
            (i as i64 * 4 + 1 + pages_info_offset as i64 + extra, 4)
        };
        if offset_i < 0 || take_i < 0 {
            return Ok(RfResult::Fallback(
                "форма: некорректные смещения page_info".to_string(),
            ));
        }
        let (offset, take) = (offset_i as usize, take_i as usize);
        if offset + take > pages_parent.len() {
            return Ok(RfResult::Fallback(
                "форма: контейнер page_info короче ожидаемого".to_string(),
            ));
        }
        let page_info: Vec<J> = pages_parent[offset..offset + take].iter().map(saby_json).collect();
        let key = calc_id(path, Some(page_name), None);
        if let Some((_, J::Obj(entries))) = data.iter_mut().find(|(k, _)| k == &key) {
            entries.push(("info".to_string(), J::Arr(page_info)));
        }
    }
    let del_start = pages_info_offset + 1;
    let del_end = (pages_info_offset + 1 + pages_info_count).min(pages_parent.len());
    pages_parent.drain(del_start..del_end);
    pages_parent[pages_info_offset] = V8Value::Raw("в отдельном файле".to_string());

    data.push((
        calc_id(path, Some("-pages-"), None),
        J::Arr(page_names.iter().cloned().map(J::Str).collect()),
    ));

    Ok(RfResult::Value(page_names))
}

/// saby `Panel.add_elem::get_page_name`: разрешить номер страницы (`elem_raw_data[-3][-5]`,
/// строка) в имя страницы текущей панели. Внешний `None` — некорректный формат номера страницы
/// (saby `ValueError`/несовпадение `str(int(page))` → фолбэк). `Some(None)` — элемент вне страниц
/// (page пустой ИЛИ у панели нет страниц вовсе, saby `if page and self.pages`). `Some(Some(name))`
/// — разрешённое имя страницы.
fn rf_resolve_page(page_raw: &str, pages: &[String]) -> Option<Option<String>> {
    if page_raw.is_empty() || pages.is_empty() {
        return Some(None);
    }
    let idx: i64 = page_raw.parse().ok()?;
    if idx.to_string() != page_raw {
        return None;
    }
    let real_idx = if idx < 0 { pages.len() as i64 + idx } else { idx };
    if real_idx < 0 {
        return None;
    }
    pages.get(real_idx as usize).cloned().map(Some)
}

/// saby `FormElement.decode`: имя / номер страницы (строка) / id элемента по фиксированным
/// смещениям ОТ КОНЦА raw-массива (`elem_raw_data[-2][1]`, `elem_raw_data[-3][-5]`,
/// `elem_raw_data[1]`) — единая схема для ВСЕХ листовых типов FormElements26/27 и для дочерней
/// панели-группы (`Panel.decode` при `is_child` вызывает этот же метод через `super().decode`).
fn rf_leaf_tail(list: &[V8Value]) -> Option<(String, String, String)> {
    let name_idx = list.len().checked_sub(2)?;
    let name_container = list.get(name_idx)?.as_list()?;
    let name = text_of(name_container.get(1)?);

    let page_idx = list.len().checked_sub(3)?;
    let page_container = list.get(page_idx)?.as_list()?;
    let page_rel = page_container.len().checked_sub(5)?;
    let page_raw = text_of(page_container.get(page_rel)?);

    let elem_id = text_of(list.get(1)?);

    Some((name, page_raw, elem_id))
}

/// saby `Panel.decode_elements` (для панели — корневой либо вложенной): разобрать список
/// дочерних элементов панели. `items` — контейнер элементов (`elem_raw_data[2]` у корня,
/// `elem_raw_data[-1]` у вложенной панели). `path`/`pages` — префикс ключей и имена страниц
/// ТЕКУЩЕЙ панели. Возвращает дерево дочерних узлов. Мутирует `items`: `[0]` → маркер «Дочерние
/// элементы отдельно», элементы удаляются.
fn rf_decode_elements(
    items: &mut Vec<V8Value>,
    path: &str,
    pages: &[String],
    ctx: &RfCtx,
    data: &mut Vec<(String, J)>,
) -> Result<RfResult<Vec<J>>> {
    let count = match int_at(items.as_slice(), 0) {
        Some(n) if n >= 0 => n as usize,
        _ => {
            return Ok(RfResult::Fallback(format!(
                "форма: счётчик элементов не число (path=\"{path}\")"
            )))
        }
    };
    if count == 0 {
        return Ok(RfResult::Value(Vec::new()));
    }
    if items.len() < 1 + count {
        return Ok(RfResult::Fallback(format!(
            "форма: контейнер элементов короче ожидаемого (path=\"{path}\")"
        )));
    }
    let mut tree: Vec<J> = Vec::with_capacity(count);
    for i in 0..count {
        // Клонируем raw элемента: вложенная панель мутирует свой raw (drain страниц/детей), а
        // сам `items` дренируется в конце — клон снимает конфликт заимствований (тот же приём,
        // что в `decode_form_elements` для управляемых форм).
        let mut elem = items[1 + i].clone();
        match rf_decode_elem(&mut elem, path, pages, ctx, data)? {
            RfResult::Value(node) => tree.push(node),
            RfResult::Fallback(r) => return Ok(RfResult::Fallback(r)),
        }
    }
    items[0] = V8Value::Raw("Дочерние элементы отдельно".to_string());
    items.drain(1..1 + count);
    Ok(RfResult::Value(tree))
}

/// saby `FormElement.decode`/`Panel.decode`: разобрать один элемент формы. Панель (тип `Panel`,
/// `elem[1]` — НЕ список, т.е. дочерняя, привязанная к реквизиту группа) — рекурсивно (страницы +
/// дочерние элементы, узел дерева с полем `child`); прочие типы — «лист» (единая логика для ВСЕХ
/// нелистовых типов FormElements26/27 — в отличие от FormElements4 нет разных смещений на тип).
/// `elem` — уже клонированный raw (мутируется на месте при рекурсии в панель).
fn rf_decode_elem(
    elem: &mut V8Value,
    path: &str,
    pages: &[String],
    ctx: &RfCtx,
    data: &mut Vec<(String, J)>,
) -> Result<RfResult<J>> {
    let type_uuid = {
        let list = rf_req!(elem.as_list(), "форма: элемент не список");
        rf_req!(list.first().map(text_of), "форма: у элемента нет типа")
    };
    let type_name = rf_req!(
        rf_item_type_name(&type_uuid),
        format!("форма: неизвестный тип элемента {type_uuid}")
    );

    if type_uuid == RF_PANEL_TYPE_UUID {
        let is_child = {
            let list = rf_req!(elem.as_list(), "форма: панель не список");
            !matches!(list.get(1), Some(V8Value::List(_)))
        };
        if !is_child {
            return Ok(RfResult::Fallback(
                "форма: вложенная панель без ссылки на реквизит (не группа) не поддержана"
                    .to_string(),
            ));
        }

        let (name, page_raw, elem_id) = {
            let list = rf_req!(elem.as_list(), "форма: панель не список");
            rf_req!(rf_leaf_tail(list), "форма: панель — не хватает хвостовых полей")
        };
        let page_name: Option<String> = rf_req!(
            rf_resolve_page(&page_raw, pages),
            format!("форма: не удалось определить страницу панели \"{name}\"")
        );
        // saby: new_path = elem_id.replace('includr_', 'include_'), где elem_id тут —
        // `calc_id(path, page_name, name)` (результат `add_elem`).
        let new_path = calc_id(path, page_name.as_deref(), Some(&name)).replace("includr_", "include_");

        // Пункт данных группы вставляется в data ДО рекурсии (saby: `add_elem` вызывается
        // раньше `decode_pages`/`decode_elements` — это определяет ПОЗИЦИЮ ключа в итоговом
        // `data`, дочерние записи вставляются уже ПОСЛЕ неё через `form.elements_data.update(...)`).
        // Но "raw" в saby — ссылка на мутируемый список: значение видно уже post-мутации, т.к.
        // сериализация происходит в конце. У нас `J`-снимок неизменяем — кладём плейсхолдер
        // сейчас, перезаписываем на настоящий снимок (после рекурсии) по ключу.
        let elem_key = calc_id(path, page_name.as_deref(), Some(&name));
        let page_id = calc_id(path, page_name.as_deref(), None);
        let mut data_obj: Vec<(String, J)> = vec![
            ("id".to_string(), J::Str(elem_id.clone())),
            ("ver".to_string(), J::Num(ctx.ver)),
            ("page".to_string(), J::Str(page_id)),
            ("raw".to_string(), J::Null),
        ];
        if let Some(prop_name) = ctx.props_by_elem_id.get(&elem_id) {
            data_obj.push(("prop".to_string(), J::Str(prop_name.clone())));
        }
        data.push((elem_key.clone(), J::Obj(data_obj)));

        let child_pages = {
            let list = rf_req!(as_list_mut(elem), "форма: панель не список");
            let pages_container = rf_req!(
                list_mut_path(list, &[2, 1]),
                "форма: у дочерней панели нет контейнера страниц"
            );
            match rf_decode_pages(pages_container, &new_path, ctx.ver, data)? {
                RfResult::Value(p) => p,
                RfResult::Fallback(r) => return Ok(RfResult::Fallback(r)),
            }
        };
        let child_tree = {
            let list = rf_req!(as_list_mut(elem), "форма: панель не список");
            let last_idx = rf_req!(list.len().checked_sub(1), "форма: у панели пустой raw");
            let items = rf_req!(
                list.get_mut(last_idx).and_then(as_list_mut),
                "форма: контейнер элементов панели не список"
            );
            match rf_decode_elements(items, &new_path, &child_pages, ctx, data)? {
                RfResult::Value(t) => t,
                RfResult::Fallback(r) => return Ok(RfResult::Fallback(r)),
            }
        };

        // Теперь `elem` полностью мутирован (страницы схлопнуты, дети заменены маркером) —
        // заполняем настоящее значение "raw" в уже вставленной записи `data`.
        if let Some((_, J::Obj(entries))) = data.iter_mut().find(|(k, _)| k == &elem_key) {
            if let Some((_, v)) = entries.iter_mut().find(|(k, _)| k == "raw") {
                *v = saby_json(elem);
            }
        }

        return Ok(RfResult::Value(J::Obj(vec![
            ("name".to_string(), J::Str(name)),
            ("type".to_string(), J::Str(type_name.to_string())),
            (
                "page".to_string(),
                match &page_name {
                    Some(p) => J::Str(p.clone()),
                    None => J::Null,
                },
            ),
            ("child".to_string(), J::Arr(child_tree)),
        ])));
    }

    // ─── лист: имя/страница/id — общая схема, без рекурсии ─────────────────────────────────
    let (name, page_raw, elem_id) = {
        let list = rf_req!(elem.as_list(), "форма: элемент не список");
        rf_req!(
            rf_leaf_tail(list),
            format!("форма: элемент {type_name} — не хватает хвостовых полей")
        )
    };
    let page_name: Option<String> = rf_req!(
        rf_resolve_page(&page_raw, pages),
        format!("форма: не удалось определить страницу элемента \"{name}\"")
    );
    let elem_key = calc_id(path, page_name.as_deref(), Some(&name));
    let page_id = calc_id(path, page_name.as_deref(), None);
    let mut data_obj: Vec<(String, J)> = vec![
        ("id".to_string(), J::Str(elem_id.clone())),
        ("ver".to_string(), J::Num(ctx.ver)),
        ("page".to_string(), J::Str(page_id)),
        ("raw".to_string(), saby_json(elem)),
    ];
    if let Some(prop_name) = ctx.props_by_elem_id.get(&elem_id) {
        data_obj.push(("prop".to_string(), J::Str(prop_name.clone())));
    }
    data.push((elem_key, J::Obj(data_obj)));

    Ok(RfResult::Value(J::Obj(vec![
        ("name".to_string(), J::Str(name)),
        ("type".to_string(), J::Str(type_name.to_string())),
        (
            "page".to_string(),
            match &page_name {
                Some(p) => J::Str(p.clone()),
                None => J::Null,
            },
        ),
    ])))
}

/// saby `Panel.decode` для КОРНЕВОЙ панели формы (`is_child=False`, не создаёт собственного узла
/// дерева — только отдаёт вложенные страницы/элементы напрямую как дерево формы). `root_panel` —
/// `form_data[1][2]` (список `[тип, [_, страницы], элементы]`).
fn rf_decode_root_panel(
    root_panel: &mut Vec<V8Value>,
    ctx: &RfCtx,
    data: &mut Vec<(String, J)>,
) -> Result<RfResult<Vec<J>>> {
    let is_root = matches!(root_panel.get(1), Some(V8Value::List(_)));
    if !is_root {
        return Ok(RfResult::Fallback(
            "форма: неожиданный формат корневой панели (elem[1] не список)".to_string(),
        ));
    }
    let pages = {
        let pages_container = rf_req!(
            list_mut_path(root_panel, &[1, 1]),
            "форма: у корневой панели нет контейнера страниц"
        );
        match rf_decode_pages(pages_container, "", ctx.ver, data)? {
            RfResult::Value(p) => p,
            RfResult::Fallback(r) => return Ok(RfResult::Fallback(r)),
        }
    };
    let items = rf_req!(
        root_panel.get_mut(2).and_then(as_list_mut),
        "форма: у корневой панели нет контейнера элементов"
    );
    match rf_decode_elements(items, "", &pages, ctx, data)? {
        RfResult::Value(tree) => Ok(RfResult::Value(tree)),
        RfResult::Fallback(r) => Ok(RfResult::Fallback(r)),
    }
}

/// saby `read_raw_code(uncomment_directive=True)`: снять маркер-комментарий v8unpack перед
/// директивами компиляции (`\n// v8unpack #Область` → `\n#Область`; символьный класс исходного
/// regex `[#|&]` включает буквально `#`, `|` и `&`). В реальном корпусе фикстур маркер не
/// встречается — функция становится no-op.
fn rf_uncomment_directives(code: &str) -> String {
    const MARKER: &str = "\n// v8unpack ";
    if !code.contains(MARKER) {
        return code.to_string();
    }
    let mut out = String::with_capacity(code.len());
    let mut rest = code;
    while let Some(pos) = rest.find(MARKER) {
        out.push_str(&rest[..pos]);
        let after_marker = &rest[pos + MARKER.len()..];
        let mut chars = after_marker.chars();
        match chars.next() {
            Some(c) if c == '#' || c == '&' || c == '|' => {
                out.push('\n');
                out.push(c);
                rest = chars.as_str();
            }
            _ => {
                out.push_str(&rest[pos..pos + MARKER.len()]);
                rest = after_marker;
            }
        }
    }
    out.push_str(rest);
    out
}

/// saby `OrganizerCode.unpack` (пост-обработка кода модуля на этапе организации
/// вывода): область `#Область include_ИМЯ` / `#Область includr_ИМЯ` ... `#КонецОбласти`
/// содержит ОБЩИЙ код, который v8unpack выносит в ОТДЕЛЬНЫЙ файл ВНЕ дерева
/// распаковки конкретного объекта (путь строится из `ИМЯ`, разбитого по `_`, и лежит
/// на уровень выше `dest_dir` — `OrganizerCode.parse_include_path`). Мы такие файлы не
/// пишем (эталон v8unpack.exe их тоже не сохраняет в тестовом дереве — они уходят
/// «мимо» результата), но ОБЯЗАНЫ вырезать их содержимое из родительского кода, оставляя
/// только сами строки `#Область .../#КонецОбласти` (иначе получим ПОЛНЫЙ, неразрезанный
/// текст вместо «пустой оболочки», как в эталоне). Применяется одинаково к коду любого
/// объекта (ExternalDataProcessor/Form/ReportForm) — саby гоняет этот проход по ЛЮБОМУ
/// `.bsl`-файлу вне зависимости от того, как он был извлечён.
///
/// Обычные (не-include) `#Область`/`#КонецОбласти` — сквозные: их строки и содержимое
/// остаются в родительской области как есть (стек `is_include_level` нужен только чтобы
/// `#КонецОбласти` знал, закрывает ли он include-уровень, — саby `_path`/`_include_path`).
fn strip_include_areas(code: &str) -> String {
    let mut root = String::with_capacity(code.len());
    let mut is_include_level: Vec<bool> = Vec::new();
    let mut include_depth: usize = 0;

    for line in code.split_inclusive('\n') {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('#') {
            if rest.starts_with("Область") {
                let is_area = trimmed.starts_with("#Область include_")
                    || trimmed.starts_with("#Область includr_");
                // Сама строка "#Область ..." — ещё в РОДИТЕЛЬСКОЙ (текущей) области,
                // переключение на новую происходит СО СЛЕДУЮЩЕЙ строки.
                if include_depth == 0 {
                    root.push_str(line);
                }
                is_include_level.push(is_area);
                if is_area {
                    include_depth += 1;
                }
                continue;
            } else if rest.starts_with("КонецОбласти") {
                let was_include = is_include_level.pop().unwrap_or(false);
                if was_include {
                    include_depth = include_depth.saturating_sub(1);
                }
                if include_depth == 0 {
                    root.push_str(line);
                }
                continue;
            }
        }
        if include_depth == 0 {
            root.push_str(line);
        }
    }
    root
}

/// Распаковать вложенную обычную форму (Тип формы=0, FormElements26/27 — 1С
/// 8.1-совместимый формат, в отличие от управляемых форм Тип=1/FormElements4) в
/// `dest_dir/{class_name}/{имя формы}/`: `{class_name}.json` (описатель),
/// `{class_name}.elem.json` (params/props/commands/tree/data), `{class_name}.id.json` (UUID),
/// `{class_name}.obj.bsl` (код модуля формы). Эталон — saby `MetaDataObject/Form/FormCore.py`
/// (ветка `OF`, `decode_form0_from_dir`) + `FormElements26/27`/`Panel` (дерево элементов).
///
/// Общая для обоих include-типов обычной формы (`d5b0e5ed`/Form и `a3b368c0`/ReportForm —
/// у saby это один и тот же `FormCore`/`Form5`/`Form9`, разница только в
/// `meta_obj_class.__name__`, отсюда параметр `class_name`). Дескриптор формы
/// (`{form_uuid}`) — сиблинг-entry в content-контейнере, как и у управляемой формы, но на
/// один уровень вложенности мельче (`obj_version` node = `"0"`, не `"1"`).
/// Содержимое (`{form_uuid}.0`) здесь — ВЛОЖЕННЫЙ контейнер (а не плоский скобкофайл, как у
/// управляемой формы): внутри entries `form` (дерево формы) и `module` (код модуля).
fn decode_regular_form(
    content: &V8File,
    form_uuid: &str,
    class_name: &str,
    dest_dir: &Path,
) -> Result<UnpackOutcome> {
    // ─── заголовок формы ─────────────────────────────────────────────────────────────────────
    let fh_entry = req!(
        content.find(form_uuid),
        format!("нет дескриптора обычной формы \"{form_uuid}\"")
    );
    let mut fh = V8Value::List(vec![parse_bytes_utf8_or_1251(&try_inflate(&fh_entry.data))?]);

    // saby `Form.get_form_root`: obj_version-дискриминатор — это НЕ признак «обычная/управляемая
    // форма» (тот определяется отдельно, полем «Тип формы» ниже), а версия РАСКЛАДКИ заголовка:
    // "0" — плоская (старые платформы, Form5), "1" — с ещё одним уровнем вложенности (Form9,
    // ТА ЖЕ раскладка, что и у управляемой формы в `decode_form`). base — путь до form_root.
    let disc = text_of(req!(fh.path(&[0, 1, 0]), "форма: нет пути [0,1,0]"));
    let base: Vec<usize> = match disc.as_str() {
        "0" => vec![0, 1],
        "1" => vec![0, 1, 1],
        other => {
            return Ok(UnpackOutcome::Unsupported(format!(
                "форма: неизвестный obj_version node {other}"
            )))
        }
    };
    let b_path: Vec<usize> = base.iter().copied().chain([1]).collect();
    let hdr_path: Vec<usize> = b_path.iter().copied().chain([1]).collect();
    let type_path: Vec<usize> = b_path.iter().copied().chain([3]).collect();
    let version_path: Vec<usize> = b_path.iter().copied().chain([0]).collect();

    // «Тип формы» — B[3] (saby `FormCore.decode_data`, `_header_obj[1][3]`, IndexError → OF='0').
    // Для обычной формы обязано быть "0" (симметрично `decode_form`, где обязано быть "1").
    let type_of_form = fh
        .path(&type_path)
        .map(text_of)
        .unwrap_or_else(|| "0".to_string());
    if type_of_form != "0" {
        return Ok(UnpackOutcome::Unsupported(format!(
            "форма: Тип формы {type_of_form} не поддержан (ожидалась обычная форма, \"0\")"
        )));
    }

    let version = text_of(req!(fh.path(&version_path), "форма: нет пути version"));
    if !RF_SUPPORTED_VERSIONS.contains(&version.as_str()) {
        return Ok(UnpackOutcome::Unsupported(format!(
            "форма: версия {version} не поддержана (ожидалась одна из {RF_SUPPORTED_VERSIONS:?})"
        )));
    }

    let uuid_path: Vec<usize> = hdr_path.iter().copied().chain([1, 2]).collect();
    let name_path: Vec<usize> = hdr_path.iter().copied().chain([2]).collect();
    let name2_path: Vec<usize> = hdr_path.iter().copied().chain([3]).collect();
    let comment_path: Vec<usize> = hdr_path.iter().copied().chain([4]).collect();

    let uuid = text_of(req!(fh.path(&uuid_path), "форма: нет пути uuid"));
    let name = text_of(req!(fh.path(&name_path), "форма: нет пути name"));

    let n2 = req!(fh.path(&name2_path), "форма: нет пути name2");
    let name2_count: usize = text_of(req!(n2.get(0), "форма name2: нет count"))
        .parse()
        .unwrap_or(0);
    let mut name2: Vec<(String, String)> = Vec::with_capacity(name2_count);
    for i in 0..name2_count {
        let key = text_of(req!(n2.get(1 + 2 * i), format!("форма name2[{i}]: нет key")));
        let val = text_of(req!(n2.get(2 + 2 * i), format!("форма name2[{i}]: нет val")));
        name2.push((key, val));
    }

    let comment = text_of(req!(fh.path(&comment_path), "форма: нет пути comment"));

    if !set_at_path(&mut fh, &uuid_path, V8Value::Raw("в отдельном файле".to_string())) {
        return Ok(UnpackOutcome::Unsupported(
            "форма: не удалось подставить маркер id по пути uuid".to_string(),
        ));
    }

    // ─── содержимое ({form_uuid}.0): вложенный контейнер с entries "form"+"module" ──────────
    let fc_container_entry = req!(
        content.find(&format!("{form_uuid}.0")),
        format!("нет содержимого обычной формы \"{form_uuid}.0\"")
    );
    let fc_container = match unpack(&try_inflate(&fc_container_entry.data)) {
        Ok(v) => v,
        Err(_) => {
            return Ok(UnpackOutcome::Unsupported(format!(
                "{form_uuid}.0 не является вложенным контейнером — не dir-скелет обычной формы"
            )))
        }
    };

    let form_subentry = req!(
        fc_container.find("form"),
        format!("{form_uuid}.0: нет entry \"form\"")
    );
    // fc = List([form_big]) — та же схема «на диске плоско → в выводе список из одного
    // элемента», что и у управляемой формы; form_big — «большой список» формы (тип "26"/"27"
    // первым элементом).
    let mut fc = V8Value::List(vec![parse_bytes_utf8_or_1251(&try_inflate(&form_subentry.data))?]);

    let module_entry = req!(
        fc_container.find("module"),
        format!("{form_uuid}.0: нет entry \"module\"")
    );
    let module_data = try_inflate(&module_entry.data);
    let (code_encoding_obj, module_no_bom): (&str, &[u8]) = if module_data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        ("utf-8-sig", &module_data[3..])
    } else {
        ("utf-8", &module_data[..])
    };
    let code_text_raw = match std::str::from_utf8(module_no_bom) {
        Ok(s) => s.to_string(),
        Err(_) => {
            return Ok(UnpackOutcome::Unsupported(format!(
                "{form_uuid}.0/module: не валидный UTF-8"
            )))
        }
    };
    let code_obj = rf_uncomment_directives(&normalize_newlines(&code_text_raw));

    // ─── form_big = fc[0] («большой список» формы) ──────────────────────────────────────────
    let form_ver = text_of(req!(fc.path(&[0, 0]), "форма: нет пути [0,0] (версия элементов)"));
    let (elements_ver, name_index): (i64, usize) = match form_ver.as_str() {
        "26" => (26, 3),
        "27" => (27, 4),
        other => {
            return Ok(UnpackOutcome::Unsupported(format!(
                "форма: версия элементов {other} не поддержана (ожидалась 26 или 27)"
            )))
        }
    };

    let panel_type_uuid = text_of(req!(
        fc.path(&[0, 1, 2, 0]),
        "форма: нет пути [0,1,2,0] (тип корневой панели)"
    ));
    if panel_type_uuid != RF_PANEL_TYPE_UUID {
        return Ok(UnpackOutcome::Unsupported(format!(
            "форма: корневой элемент {panel_type_uuid} не панель"
        )));
    }

    let datasource = fc.path(&[0, 2, 3]).cloned();

    let (props_json, props_by_id): (Option<Vec<J>>, std::collections::HashMap<String, String>) = {
        let fc_top = req!(as_list_mut(&mut fc), "форма: fc не список");
        let props_container = req!(
            list_mut_path(fc_top, &[0, 2, 2]),
            "форма: нет пути [0,2,2] (реквизиты)"
        );
        match rf_decode_props(props_container, name_index)? {
            RfResult::Value(v) => v,
            RfResult::Fallback(r) => return Ok(UnpackOutcome::Unsupported(r)),
        }
    };

    let props_by_elem_id: std::collections::HashMap<String, String> = match &datasource {
        Some(ds) => rf_build_datasource_index(ds, &props_by_id),
        None => std::collections::HashMap::new(),
    };

    let ctx = RfCtx {
        ver: elements_ver,
        props_by_elem_id: &props_by_elem_id,
    };

    let mut data_entries: Vec<(String, J)> = Vec::new();
    let tree_json: Vec<J> = {
        let fc_top = req!(as_list_mut(&mut fc), "форма: fc не список");
        let root_panel = req!(
            list_mut_path(fc_top, &[0, 1, 2]),
            "форма: нет пути [0,1,2] (корневая панель)"
        );
        match rf_decode_root_panel(root_panel, &ctx, &mut data_entries)? {
            RfResult::Value(t) => t,
            RfResult::Fallback(r) => return Ok(UnpackOutcome::Unsupported(r)),
        }
    };

    // ─── сборка {class_name}.json / .elem.json / .id.json / .obj.bsl ────────────────────────
    let name2_json: Vec<(String, J)> = name2.into_iter().map(|(k, v)| (k, J::Str(v))).collect();

    // form_wrapped = List([fc[, form1]]) — ВТОРАЯ обёртка для самого поля "form" (как у
    // управляемой формы: `form_wrapped = V8Value::List(vec![fc, ...])`), плюс опциональный
    // form1 (saby `decode_form1` — вызывается безусловно, независимо от Тип формы).
    let mut form_items = vec![fc];
    if let Some(f1) = read_form1(content, form_uuid)? {
        form_items.push(f1);
    }
    let form_field = saby_json(&V8Value::List(form_items));

    let root_obj = J::Obj(vec![
        ("name".to_string(), J::Str(name.clone())),
        ("name2".to_string(), J::Obj(name2_json)),
        ("comment".to_string(), J::Str(comment)),
        ("header".to_string(), saby_json(&fh)),
        ("Тип формы".to_string(), J::Str("0".to_string())),
        ("form".to_string(), form_field),
        ("code_encoding_obj".to_string(), J::Str(code_encoding_obj.to_string())),
        ("code_info_obj".to_string(), J::Num(1)),
        (
            "Версия элементов формы".to_string(),
            J::Str(format!("0-{elements_ver}")),
        ),
        ("obj_version".to_string(), J::Str(version)),
    ]);
    let json_text = root_obj.to_pretty_string().replace('\n', "\r\n");

    let target = dest_dir.join(class_name).join(&name);
    std::fs::create_dir_all(&target)?;
    std::fs::write(target.join(format!("{class_name}.json")), json_text.as_bytes())?;

    // Код модуля формы (OF): в отличие от кода объекта (decode_code — пишется только при
    // непустом тексте), decode_form0_from_dir всегда ставит code_info_obj=1, и файл .obj.bsl
    // пишется безусловно (проверено по golden-фикстуре с пустым модулем).
    let bsl_text = strip_include_areas(&code_obj).replace('\n', "\r\n");
    std::fs::write(target.join(format!("{class_name}.obj.bsl")), bsl_text.as_bytes())?;

    let elem_obj = J::Obj(vec![
        ("params".to_string(), J::Arr(Vec::new())),
        (
            "props".to_string(),
            match props_json {
                Some(p) => J::Arr(p),
                None => J::Null,
            },
        ),
        ("commands".to_string(), J::Arr(Vec::new())),
        ("tree".to_string(), J::Arr(tree_json)),
        ("data".to_string(), J::Obj(data_entries)),
    ]);
    let elem_json_text = elem_obj.to_pretty_string().replace('\n', "\r\n");
    std::fs::write(target.join(format!("{class_name}.elem.json")), elem_json_text.as_bytes())?;

    let id_obj = J::Obj(vec![("uuid".to_string(), J::Str(uuid))]);
    let id_json_text = id_obj.to_pretty_string().replace('\n', "\r\n");
    std::fs::write(target.join(format!("{class_name}.id.json")), id_json_text.as_bytes())?;

    Ok(UnpackOutcome::Done)
}

// ─── golden-тест ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Корень каталога golden-фикстур. В публичном репозитории его нет: фикстуры
    /// — реальные обработки, они не публикуются. Тесты, которым фикстура нужна,
    /// в этом случае пропускаются, а не падают.
    const FIXTURES_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    /// Проверить наличие файла фикстуры. Возвращает `true` (и печатает пояснение),
    /// если файла нет — вызывающий тест должен сразу вернуться.
    fn fixture_missing(path: &str) -> bool {
        if std::path::Path::new(path).exists() {
            return false;
        }
        eprintln!("фикстура {path} отсутствует — тест пропущен");
        true
    }

    /// Каталог фикстуры, имя которого задаётся переменной окружения (сам каталог
    /// назван по реальной обработке, поэтому в исходниках его имени нет).
    /// Переменная не задана — `None`, тест пропускается.
    fn fixture_dir_from_env(var: &str) -> Option<std::path::PathBuf> {
        match std::env::var(var) {
            Ok(name) if !name.trim().is_empty() => {
                Some(std::path::Path::new(FIXTURES_ROOT).join(name.trim()))
            }
            _ => {
                eprintln!("переменная окружения {var} не задана — тест пропущен");
                None
            }
        }
    }

    const FIXTURE_EPF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ТестоваяОбработка/ТестоваяОбработка.epf"
    );
    const FIXTURE_EXPECTED_JSON: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ТестоваяОбработка/expected/ExternalDataProcessor.json"
    );
    const FIXTURE_EXPECTED_BSL: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ТестоваяОбработка/expected/ExternalDataProcessor.obj.bsl"
    );

    /// Сравнить два буфера байт-в-байт; при расхождении вывести длины,
    /// смещение первого отличающегося байта и контекст по ~40 байт с обеих
    /// сторон — для отладки без угадывания.
    fn assert_bytes_eq(actual: &[u8], expected: &[u8], label: &str) {
        if actual == expected {
            return;
        }
        let min_len = actual.len().min(expected.len());
        let diff_at = (0..min_len)
            .find(|&i| actual[i] != expected[i])
            .unwrap_or(min_len);
        const CTX: usize = 40;
        let a_start = diff_at.saturating_sub(CTX);
        let a_end = (diff_at + CTX).min(actual.len());
        let e_start = diff_at.saturating_sub(CTX);
        let e_end = (diff_at + CTX).min(expected.len());
        panic!(
            "{label}: байты не совпадают.\n\
             actual.len()={} expected.len()={}\n\
             первое расхождение на смещении {diff_at}\n\
             actual[{a_start}..{a_end}]   = {:?}\n\
             expected[{e_start}..{e_end}] = {:?}",
            actual.len(),
            expected.len(),
            &actual[a_start..a_end],
            &expected[e_start..e_end],
        );
    }

    #[test]
    fn golden_external_data_processor_skeleton() {
        if fixture_missing(FIXTURE_EPF) {
            return;
        }
        let epf_bytes = std::fs::read(FIXTURE_EPF).expect("читать фикстуру .epf");

        let dest_dir = std::env::temp_dir().join(format!(
            "1c_export_saby_golden_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest_dir);
        std::fs::create_dir_all(&dest_dir).expect("создать temp-каталог для теста");

        let outcome =
            unpack_epf_skeleton(&epf_bytes, &dest_dir).expect("unpack_epf_skeleton не должен падать");
        assert!(
            matches!(outcome, UnpackOutcome::Done),
            "ожидался UnpackOutcome::Done, получено {outcome:?}"
        );

        let actual_json = std::fs::read(dest_dir.join("ExternalDataProcessor.json"))
            .expect("читать выгруженный ExternalDataProcessor.json");
        let expected_json =
            std::fs::read(FIXTURE_EXPECTED_JSON).expect("читать эталонный ExternalDataProcessor.json");
        assert_bytes_eq(&actual_json, &expected_json, "ExternalDataProcessor.json");

        let actual_bsl = std::fs::read(dest_dir.join("ExternalDataProcessor.obj.bsl"))
            .expect("читать выгруженный ExternalDataProcessor.obj.bsl");
        let expected_bsl = std::fs::read(FIXTURE_EXPECTED_BSL)
            .expect("читать эталонный ExternalDataProcessor.obj.bsl");
        assert_bytes_eq(&actual_bsl, &expected_bsl, "ExternalDataProcessor.obj.bsl");

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    // ─── golden-тест ExternalReport + вложенный макет (СКД scheme) ──────────

    const RPT_FIXTURE_ERF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ТестовыйОтчет/ТестовыйОтчет.erf"
    );
    const RPT_EXPECTED_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ТестовыйОтчет/expected"
    );

    #[test]
    fn golden_external_report_with_scheme() {
        if fixture_missing(RPT_FIXTURE_ERF) {
            return;
        }
        let erf_bytes = std::fs::read(RPT_FIXTURE_ERF).expect("читать фикстуру .erf");

        let dest_dir = std::env::temp_dir().join(format!(
            "1c_export_saby_golden_report_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest_dir);
        std::fs::create_dir_all(&dest_dir).expect("создать temp-каталог для теста");

        let outcome =
            unpack_epf_skeleton(&erf_bytes, &dest_dir).expect("unpack_epf_skeleton не должен падать");
        assert!(
            matches!(outcome, UnpackOutcome::Done),
            "ожидался UnpackOutcome::Done, получено {outcome:?}"
        );

        // Все 5 файлов эталона: путь под dest_dir == путь под expected/.
        let expected_root = std::path::Path::new(RPT_EXPECTED_DIR);
        for rel in [
            "ExternalReport.json",
            "ExternalDataProcessor.obj.bsl",
            "Template/ОсновнаяСхемаКомпоновкиДанных/Template.bin",
            "Template/ОсновнаяСхемаКомпоновкиДанных/Template.json",
            "Template/ОсновнаяСхемаКомпоновкиДанных/Template.id.json",
        ] {
            let actual = std::fs::read(dest_dir.join(rel))
                .unwrap_or_else(|e| panic!("читать выгруженный {rel}: {e}"));
            let expected = std::fs::read(expected_root.join(rel))
                .unwrap_or_else(|e| panic!("читать эталонный {rel}: {e}"));
            assert_bytes_eq(&actual, &expected, rel);
        }

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    // ─── ВРЕМЕННЫЙ A/B-харнесс: нативная распаковка всех .erf из каталога ───
    // Запуск: ONEC_ERF_AB_DIR=<in> ONEC_ERF_AB_OUT=<out> cargo test --release \
    //   ab_erf_local_dump -- --ignored --exact --nocapture
    #[test]
    #[ignore]
    fn ab_erf_local_dump() {
        let in_dir = std::env::var("ONEC_ERF_AB_DIR").expect("ONEC_ERF_AB_DIR");
        let out_dir = std::env::var("ONEC_ERF_AB_OUT").expect("ONEC_ERF_AB_OUT");
        let out_root = std::path::Path::new(&out_dir);
        let mut entries: Vec<_> = std::fs::read_dir(&in_dir)
            .expect("read_dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| matches!(p.extension().and_then(|s| s.to_str()), Some("erf") | Some("epf")))
            .collect();
        entries.sort();
        for p in entries {
            let stem = p.file_stem().unwrap().to_string_lossy().to_string();
            let bytes = std::fs::read(&p).expect("read erf");
            let dest = out_root.join(&stem);
            let _ = std::fs::remove_dir_all(&dest);
            std::fs::create_dir_all(&dest).expect("mkdir");
            match unpack_epf_skeleton(&bytes, &dest) {
                Ok(o) => println!("AB {stem}: {o:?}"),
                Err(e) => println!("AB {stem}: ERR {e}"),
            }
        }
    }

    // ─── golden-тест ExternalDataProcessor + вложенная управляемая форма ───

    const FORM_FIXTURE_EPF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/Регистратор/Регистратор.epf"
    );
    const FORM_EXPECTED_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/Регистратор/expected"
    );

    #[test]
    fn golden_form_registrator() {
        if fixture_missing(FORM_FIXTURE_EPF) {
            return;
        }
        let epf_bytes = std::fs::read(FORM_FIXTURE_EPF).expect("читать фикстуру .epf");

        let dest_dir = std::env::temp_dir().join(format!(
            "1c_export_saby_golden_form_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest_dir);
        std::fs::create_dir_all(&dest_dir).expect("создать temp-каталог для теста");

        let outcome =
            unpack_epf_skeleton(&epf_bytes, &dest_dir).expect("unpack_epf_skeleton не должен падать");
        assert!(
            matches!(outcome, UnpackOutcome::Done),
            "ожидался UnpackOutcome::Done, получено {outcome:?}"
        );

        let expected_root = std::path::Path::new(FORM_EXPECTED_DIR);
        for rel in [
            "ExternalDataProcessor.json",
            "ExternalDataProcessor.obj.bsl",
            "Form/Форма/Form.json",
            "Form/Форма/Form.elem.json",
            "Form/Форма/Form.id.json",
            "Form/Форма/Form.obj.bsl",
        ] {
            let actual = std::fs::read(dest_dir.join(rel))
                .unwrap_or_else(|e| panic!("читать выгруженный {rel}: {e}"));
            let expected = std::fs::read(expected_root.join(rel))
                .unwrap_or_else(|e| panic!("читать эталонный {rel}: {e}"));
            assert_bytes_eq(&actual, &expected, rel);
        }

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    // ─── golden-тест формы С ЭЛЕМЕНТАМИ дерева (Field + Button) ────────────

    const FORM_TREE_FIXTURE_EPF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ЗаменаСчетовУчетаНа004/ЗаменаСчетовУчетаНа004.epf"
    );
    const FORM_TREE_EXPECTED_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ЗаменаСчетовУчетаНа004/expected"
    );

    #[test]
    fn golden_izm_scheta() {
        if fixture_missing(FORM_TREE_FIXTURE_EPF) {
            return;
        }
        let epf_bytes = std::fs::read(FORM_TREE_FIXTURE_EPF).expect("читать фикстуру .epf");

        let dest_dir = std::env::temp_dir().join(format!(
            "1c_export_saby_golden_form_tree_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest_dir);
        std::fs::create_dir_all(&dest_dir).expect("создать temp-каталог для теста");

        let outcome =
            unpack_epf_skeleton(&epf_bytes, &dest_dir).expect("unpack_epf_skeleton не должен падать");
        assert!(
            matches!(outcome, UnpackOutcome::Done),
            "ожидался UnpackOutcome::Done, получено {outcome:?}"
        );

        let expected_root = std::path::Path::new(FORM_TREE_EXPECTED_DIR);
        for rel in [
            "ExternalDataProcessor.json",
            "ExternalDataProcessor.obj.bsl",
            "Form/Форма/Form.json",
            "Form/Форма/Form.elem.json",
            "Form/Форма/Form.id.json",
            "Form/Форма/Form.obj.bsl",
        ] {
            let actual = std::fs::read(dest_dir.join(rel))
                .unwrap_or_else(|e| panic!("читать выгруженный {rel}: {e}"));
            let expected = std::fs::read(expected_root.join(rel))
                .unwrap_or_else(|e| panic!("читать эталонный {rel}: {e}"));
            assert_bytes_eq(&actual, &expected, rel);
        }

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    // ─── golden-тест формы с ВЛОЖЕННЫМИ группами (Group → Group → Field) ───

    const FORM_GROUP_FIXTURE_EPF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ШаблонОбработки/ШаблонОбработки.epf"
    );
    const FORM_GROUP_EXPECTED_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ШаблонОбработки/expected"
    );

    #[test]
    fn golden_shablon() {
        if fixture_missing(FORM_GROUP_FIXTURE_EPF) {
            return;
        }
        let epf_bytes = std::fs::read(FORM_GROUP_FIXTURE_EPF).expect("читать фикстуру .epf");

        let dest_dir = std::env::temp_dir().join(format!(
            "1c_export_saby_golden_form_group_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest_dir);
        std::fs::create_dir_all(&dest_dir).expect("создать temp-каталог для теста");

        let outcome =
            unpack_epf_skeleton(&epf_bytes, &dest_dir).expect("unpack_epf_skeleton не должен падать");
        assert!(
            matches!(outcome, UnpackOutcome::Done),
            "ожидался UnpackOutcome::Done, получено {outcome:?}"
        );

        // Модуль объекта у этой обработки пуст → ExternalDataProcessor.obj.bsl
        // не пишется (проверяем только реально формируемые файлы).
        let expected_root = std::path::Path::new(FORM_GROUP_EXPECTED_DIR);
        for rel in [
            "ExternalDataProcessor.json",
            "Form/Форма/Form.json",
            "Form/Форма/Form.elem.json",
            "Form/Форма/Form.id.json",
            "Form/Форма/Form.obj.bsl",
        ] {
            let actual = std::fs::read(dest_dir.join(rel))
                .unwrap_or_else(|e| panic!("читать выгруженный {rel}: {e}"));
            let expected = std::fs::read(expected_root.join(rel))
                .unwrap_or_else(|e| panic!("читать эталонный {rel}: {e}"));
            assert_bytes_eq(&actual, &expected, rel);
        }

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    // ─── golden-тест формы с ТАБЛИЦЕЙ (Table + колонка-Field) ─────────────

    const FORM_TABLE_FIXTURE_EPF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/РегистрацияВыбранного/РегистрацияВыбранного.epf"
    );
    const FORM_TABLE_EXPECTED_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/РегистрацияВыбранного/expected"
    );

    #[test]
    fn golden_registracija() {
        if fixture_missing(FORM_TABLE_FIXTURE_EPF) {
            return;
        }
        let epf_bytes = std::fs::read(FORM_TABLE_FIXTURE_EPF).expect("читать фикстуру .epf");

        let dest_dir = std::env::temp_dir().join(format!(
            "1c_export_saby_golden_form_table_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest_dir);
        std::fs::create_dir_all(&dest_dir).expect("создать temp-каталог для теста");

        let outcome =
            unpack_epf_skeleton(&epf_bytes, &dest_dir).expect("unpack_epf_skeleton не должен падать");
        assert!(
            matches!(outcome, UnpackOutcome::Done),
            "ожидался UnpackOutcome::Done, получено {outcome:?}"
        );

        let expected_root = std::path::Path::new(FORM_TABLE_EXPECTED_DIR);
        for rel in [
            "ExternalDataProcessor.json",
            "ExternalDataProcessor.obj.bsl",
            "Form/Форма/Form.json",
            "Form/Форма/Form.elem.json",
            "Form/Форма/Form.id.json",
            "Form/Форма/Form.obj.bsl",
        ] {
            let actual = std::fs::read(dest_dir.join(rel))
                .unwrap_or_else(|e| panic!("читать выгруженный {rel}: {e}"));
            let expected = std::fs::read(expected_root.join(rel))
                .unwrap_or_else(|e| panic!("читать эталонный {rel}: {e}"));
            assert_bytes_eq(&actual, &expected, rel);
        }

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    // ─── golden-тест макета табличного документа (Template table → .mxl) ───

    const TMPL_MXL_FIXTURE_EPF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ПФДоговорОНеразглашении/ПФДоговорОНеразглашении.epf"
    );
    const TMPL_MXL_EXPECTED_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ПФДоговорОНеразглашении/expected"
    );

    #[test]
    fn golden_pf_dogovor() {
        if fixture_missing(TMPL_MXL_FIXTURE_EPF) {
            return;
        }
        let epf_bytes = std::fs::read(TMPL_MXL_FIXTURE_EPF).expect("читать фикстуру .epf");

        let dest_dir = std::env::temp_dir().join(format!(
            "1c_export_saby_golden_tmpl_mxl_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest_dir);
        std::fs::create_dir_all(&dest_dir).expect("создать temp-каталог для теста");

        let outcome =
            unpack_epf_skeleton(&epf_bytes, &dest_dir).expect("unpack_epf_skeleton не должен падать");
        assert!(
            matches!(outcome, UnpackOutcome::Done),
            "ожидался UnpackOutcome::Done, получено {outcome:?}"
        );

        let expected_root = std::path::Path::new(TMPL_MXL_EXPECTED_DIR);
        for rel in [
            "ExternalDataProcessor.json",
            "ExternalDataProcessor.obj.bsl",
            "Form/Форма/Form.json",
            "Form/Форма/Form.elem.json",
            "Form/Форма/Form.id.json",
            "Form/Форма/Form.obj.bsl",
            "Template/Договор/Template.mxl",
            "Template/Договор/Template.json",
            "Template/Договор/Template.id.json",
        ] {
            let actual = std::fs::read(dest_dir.join(rel))
                .unwrap_or_else(|e| panic!("читать выгруженный {rel}: {e}"));
            let expected = std::fs::read(expected_root.join(rel))
                .unwrap_or_else(|e| panic!("читать эталонный {rel}: {e}"));
            assert_bytes_eq(&actual, &expected, rel);
        }

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    // ─── golden-тест макета base64 (Template base64 → .c1b64) ─────────────

    const TMPL_C1B64_FIXTURE_EPF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/КомплектДокументовПриПриеме/КомплектДокументовПриПриеме.epf"
    );
    const TMPL_C1B64_EXPECTED_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/КомплектДокументовПриПриеме/expected"
    );

    #[test]
    fn golden_template_c1b64_multi() {
        if fixture_missing(TMPL_C1B64_FIXTURE_EPF) {
            return;
        }
        let epf_bytes = std::fs::read(TMPL_C1B64_FIXTURE_EPF).expect("читать фикстуру .epf");

        let dest_dir = std::env::temp_dir().join(format!(
            "1c_export_saby_golden_tmpl_c1b64_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest_dir);
        std::fs::create_dir_all(&dest_dir).expect("создать temp-каталог для теста");

        let outcome =
            unpack_epf_skeleton(&epf_bytes, &dest_dir).expect("unpack_epf_skeleton не должен падать");
        assert!(
            matches!(outcome, UnpackOutcome::Done),
            "ожидался UnpackOutcome::Done, получено {outcome:?}"
        );

        // Явный список путей заменён рекурсивной сверкой каталога `expected/`:
        // имена макетов повторяют реальные данные заказчика и в исходниках не нужны.
        assert_dir_matches_expected(&dest_dir, Path::new(TMPL_C1B64_EXPECTED_DIR));

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    // ─── golden-тест макета-заглушки (table без данных → только .json) ────
    //
    // Каталог фикстуры назван по реальной печатной форме заказчика, поэтому его
    // имя не зашито в исходники: подкаталог `tests/fixtures/` задаётся переменной
    // окружения `ONEC_EXPORT_FIXTURE_PRINTFORM`. Переменная не задана или фикстуры
    // нет — тест пропускается.

    #[test]
    fn golden_printform_templates() {
        let fixture_dir = match fixture_dir_from_env("ONEC_EXPORT_FIXTURE_PRINTFORM") {
            Some(dir) => dir,
            None => return,
        };
        let stem = fixture_dir
            .file_name()
            .expect("имя каталога фикстуры")
            .to_string_lossy()
            .to_string();
        let epf_path = fixture_dir.join(format!("{stem}.epf"));
        if fixture_missing(&epf_path.to_string_lossy()) {
            return;
        }
        let epf_bytes = std::fs::read(&epf_path).expect("читать фикстуру .epf");

        let dest_dir = std::env::temp_dir().join(format!(
            "1c_export_saby_golden_tmpl_empty_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest_dir);
        std::fs::create_dir_all(&dest_dir).expect("создать temp-каталог для теста");

        let outcome =
            unpack_epf_skeleton(&epf_bytes, &dest_dir).expect("unpack_epf_skeleton не должен падать");
        assert!(
            matches!(outcome, UnpackOutcome::Done),
            "ожидался UnpackOutcome::Done, получено {outcome:?}"
        );

        assert_dir_matches_expected(&dest_dir, &fixture_dir.join("expected"));

        // Макет-заглушка не должен создавать файл данных.
        assert!(
            !dest_dir
                .join("Template/МакетАнкетаСотрудника/Template.mxl")
                .exists(),
            "у макета-заглушки не должно быть файла данных Template.mxl"
        );

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    // ─── golden-тест многоуровневого «ПутьКДанным» (реквизит-таблица с колонкой) ─

    const FORM_NESTED_FIXTURE_EPF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ЗаменаФизЛицВПКО/ЗаменаФизЛицВПКО.epf"
    );
    const FORM_NESTED_EXPECTED_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ЗаменаФизЛицВПКО/expected"
    );

    #[test]
    fn golden_zamena_fizlic() {
        if fixture_missing(FORM_NESTED_FIXTURE_EPF) {
            return;
        }
        let epf_bytes = std::fs::read(FORM_NESTED_FIXTURE_EPF).expect("читать фикстуру .epf");

        let dest_dir = std::env::temp_dir().join(format!(
            "1c_export_saby_golden_form_nested_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest_dir);
        std::fs::create_dir_all(&dest_dir).expect("создать temp-каталог для теста");

        let outcome =
            unpack_epf_skeleton(&epf_bytes, &dest_dir).expect("unpack_epf_skeleton не должен падать");
        assert!(
            matches!(outcome, UnpackOutcome::Done),
            "ожидался UnpackOutcome::Done, получено {outcome:?}"
        );

        let expected_root = std::path::Path::new(FORM_NESTED_EXPECTED_DIR);
        for rel in [
            "ExternalDataProcessor.json",
            "Form/Форма/Form.json",
            "Form/Форма/Form.elem.json",
            "Form/Форма/Form.id.json",
            "Form/Форма/Form.obj.bsl",
        ] {
            let actual = std::fs::read(dest_dir.join(rel))
                .unwrap_or_else(|e| panic!("читать выгруженный {rel}: {e}"));
            let expected = std::fs::read(expected_root.join(rel))
                .unwrap_or_else(|e| panic!("читать эталонный {rel}: {e}"));
            assert_bytes_eq(&actual, &expected, rel);
        }

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    // ─── golden-тесты новых возможностей (типы макетов 2/5/7/8/9, native-only) ──

    /// Рекурсивно сравнить дерево `dest_dir` с эталонным `expected_root`: сперва
    /// проверяем, что множества ОТНОСИТЕЛЬНЫХ путей совпадают (никаких лишних или
    /// недостающих файлов), затем каждый общий файл — побайтно через
    /// `assert_bytes_eq`. Нужна для крупных golden-фикстур (десятки-сотни файлов —
    /// например, обработка с полусотней вложенных форм), где статический список
    /// путей `for rel in [...]` (как в тестах выше) стал бы непрактично длинным.
    fn assert_dir_matches_expected(dest_dir: &Path, expected_root: &Path) {
        fn collect_rel_paths(root: &Path, dir: &Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in
                std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
            {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    collect_rel_paths(root, &path, out);
                } else {
                    out.push(path.strip_prefix(root).expect("strip_prefix").to_path_buf());
                }
            }
        }

        let mut expected_rels = Vec::new();
        collect_rel_paths(expected_root, expected_root, &mut expected_rels);
        expected_rels.sort();

        let mut actual_rels = Vec::new();
        collect_rel_paths(dest_dir, dest_dir, &mut actual_rels);
        actual_rels.sort();

        assert_eq!(
            actual_rels, expected_rels,
            "набор файлов не совпадает с эталонным (dest={}, expected={})",
            dest_dir.display(),
            expected_root.display()
        );

        for rel in &expected_rels {
            let actual = std::fs::read(dest_dir.join(rel))
                .unwrap_or_else(|e| panic!("читать выгруженный {}: {e}", rel.display()));
            let expected = std::fs::read(expected_root.join(rel))
                .unwrap_or_else(|e| panic!("читать эталонный {}: {e}", rel.display()));
            assert_bytes_eq(&actual, &expected, &rel.display().to_string());
        }
    }

    // Обычная форма (Тип формы=0, FormElements26/27) внутри ExternalDataProcessor,
    // наряду с управляемой формой в том же файле (Form/Форма — обычная,
    // Form/ФормаУпр — управляемая) — регрессия на decode_regular_form.
    const REGULAR_FORM_FIXTURE_EPF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ФормаОбычная/ФормаОбычная.epf"
    );
    const REGULAR_FORM_EXPECTED_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ФормаОбычная/expected"
    );

    #[test]
    fn golden_regular_form_in_processor() {
        if fixture_missing(REGULAR_FORM_FIXTURE_EPF) {
            return;
        }
        let epf_bytes = std::fs::read(REGULAR_FORM_FIXTURE_EPF).expect("читать фикстуру .epf");

        let dest_dir = std::env::temp_dir().join(format!(
            "1c_export_saby_golden_regular_form_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest_dir);
        std::fs::create_dir_all(&dest_dir).expect("создать temp-каталог для теста");

        let outcome =
            unpack_epf_skeleton(&epf_bytes, &dest_dir).expect("unpack_epf_skeleton не должен падать");
        assert!(
            matches!(outcome, UnpackOutcome::Done),
            "ожидался UnpackOutcome::Done, получено {outcome:?}"
        );

        assert_dir_matches_expected(&dest_dir, Path::new(REGULAR_FORM_EXPECTED_DIR));

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    // Элемент формы Decoration (FORM_ITEM_DECORATION_UUID) — управляемая форма.
    const DECORATION_FIXTURE_EPF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ДекорацияФормы/ДекорацияФормы.epf"
    );
    const DECORATION_EXPECTED_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ДекорацияФормы/expected"
    );

    #[test]
    fn golden_form_decoration() {
        if fixture_missing(DECORATION_FIXTURE_EPF) {
            return;
        }
        let epf_bytes = std::fs::read(DECORATION_FIXTURE_EPF).expect("читать фикстуру .epf");

        let dest_dir = std::env::temp_dir().join(format!(
            "1c_export_saby_golden_decoration_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest_dir);
        std::fs::create_dir_all(&dest_dir).expect("создать temp-каталог для теста");

        let outcome =
            unpack_epf_skeleton(&epf_bytes, &dest_dir).expect("unpack_epf_skeleton не должен падать");
        assert!(
            matches!(outcome, UnpackOutcome::Done),
            "ожидался UnpackOutcome::Done, получено {outcome:?}"
        );

        assert_dir_matches_expected(&dest_dir, Path::new(DECORATION_EXPECTED_DIR));

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    // Параметры управляемой формы: единственный представитель в корпусе —
    // обработка «Консоль запросов SQL» (~6 МБ, 157 файлов), слишком крупная для
    // постоянной golden-фикстуры. Разбор параметров подтверждён полнокорпусной
    // A/B-сверкой с v8unpack (native == v8unpack, байт-в-байт); отдельный
    // in-repo golden не заводим намеренно из-за размера.

    // Макет типа html(3) — base64-полезная нагрузка извлекается из скобко-структуры
    // (Template/ОбОбработке), плюс макет-заглушка без данных (СтандартныеКартинки).
    const TMPL_HTML_FIXTURE_EPF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/МакетHtml/МакетHtml.epf"
    );
    const TMPL_HTML_EXPECTED_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/МакетHtml/expected"
    );

    #[test]
    fn golden_template_html() {
        if fixture_missing(TMPL_HTML_FIXTURE_EPF) {
            return;
        }
        let epf_bytes = std::fs::read(TMPL_HTML_FIXTURE_EPF).expect("читать фикстуру .epf");

        let dest_dir = std::env::temp_dir().join(format!(
            "1c_export_saby_golden_tmpl_html_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest_dir);
        std::fs::create_dir_all(&dest_dir).expect("создать temp-каталог для теста");

        let outcome =
            unpack_epf_skeleton(&epf_bytes, &dest_dir).expect("unpack_epf_skeleton не должен падать");
        assert!(
            matches!(outcome, UnpackOutcome::Done),
            "ожидался UnpackOutcome::Done, получено {outcome:?}"
        );

        assert_dir_matches_expected(&dest_dir, Path::new(TMPL_HTML_EXPECTED_DIR));

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    // Обычная форма ОТЧЁТА легаси (ReportForm, «Версия элементов формы»="0-26",
    // FormElements26 — платформа 8.1) в отличие от современной 0-27.
    const REPORT_FORM_026_FIXTURE_ERF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ФормаОтчета026/ФормаОтчета026.erf"
    );
    const REPORT_FORM_026_EXPECTED_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ФормаОтчета026/expected"
    );

    #[test]
    fn golden_report_form_legacy_026() {
        if fixture_missing(REPORT_FORM_026_FIXTURE_ERF) {
            return;
        }
        let epf_bytes = std::fs::read(REPORT_FORM_026_FIXTURE_ERF).expect("читать фикстуру .erf");

        let dest_dir = std::env::temp_dir().join(format!(
            "1c_export_saby_golden_reportform_026_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest_dir);
        std::fs::create_dir_all(&dest_dir).expect("создать temp-каталог для теста");

        let outcome =
            unpack_epf_skeleton(&epf_bytes, &dest_dir).expect("unpack_epf_skeleton не должен падать");
        assert!(
            matches!(outcome, UnpackOutcome::Done),
            "ожидался UnpackOutcome::Done, получено {outcome:?}"
        );

        assert_dir_matches_expected(&dest_dir, Path::new(REPORT_FORM_026_EXPECTED_DIR));

        let _ = std::fs::remove_dir_all(&dest_dir);
    }
}
