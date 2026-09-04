// Встраивание манифеста приложения в exe.
//
// Без манифеста импорт comctl32 связывается с версией 5.82, а nwg::init()
// (enable_visual_styles) подключает comctl32 6.0 контекстом активации уже на
// ходу — в процессе живут две версии, подклассы окон nwg зацикливаются
// в comctl32!DefSubclassProc и GUI виснет на старте.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=resources/1c-export.rc");
        println!("cargo:rerun-if-changed=resources/1c-export.manifest");
        embed_resource::compile("resources/1c-export.rc", embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
}
