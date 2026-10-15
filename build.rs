// Windows only (a no-op elsewhere): embed the app icon, which GPUI also uses for the window,
// and the name shown in Task Manager and the volume mixer
fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=icon.ico");
    embed_resource::compile("app.rc", embed_resource::NONE).manifest_optional().unwrap();
}
