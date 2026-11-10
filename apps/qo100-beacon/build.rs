// made by CasualArclamp
//! Puts the icon (assets/icon.ico, made by the QO100-Beacon repository's scripts/make-icon.py) into the
//! Windows exe. Other systems have nothing to do here.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        // Without the resource compiler the exe is still built, just plain.
        if let Err(e) = res.compile() {
            println!("cargo:warning=the exe has no icon: {e}");
        }
    }
}
