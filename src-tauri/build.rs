fn main() {
    // `tauri::include_image!` embeds icon pixels at compile time without
    // registering the file, so cargo would keep stale icons after an edit.
    println!("cargo:rerun-if-changed=icons");
    tauri_build::build()
}
