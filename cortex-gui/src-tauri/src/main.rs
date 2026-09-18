// The bundled app owns its window and writes nothing to a terminal, so on Windows it must not
// be given a console. `debug_assertions` keeps one for a development build, where `println!` and
// a panic message are the whole of the debugging story.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    cortex_gui_lib::run()
}
