// build.rs - compile the Win32 icon resource into the flygo binary.
//
// Uses the GNU toolchain's windres (C:\ProgramData\mingw64\mingw64\bin, on
// PATH per .cargo/config.toml) to translate resources/app.rc into an object,
// then tells cargo to pass it to the linker. This embeds the multi-res FlyGo
// icon so the .exe shows the brand identity in Explorer and the taskbar, in
// addition to the in-process tray icon (static/tray.ico, loaded at runtime).
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Always re-run when the resource or the icon changes.
    println!("cargo:rerun-if-changed=resources/app.rc");
    println!("cargo:rerun-if-changed=resources/flygo.ico");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set"));
    let res_obj = out_dir.join("app_resource.o");
    let rc = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"))
        .join("resources")
        .join("app.rc");
    let ico = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"))
        .join("resources")
        .join("flygo.ico");

    // windres runs from the manifest dir so relative include of
    // `resources\flygo.ico` inside app.rc resolves.
    let status = Command::new("windres")
        .arg(rc.to_str().expect("rc path utf8"))
        .arg("-O")
        .arg("coff")
        .arg("-o")
        .arg(&res_obj)
        .current_dir(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"))
        .status();

    match status {
        Ok(s) if s.success() => {
            println!(
                "cargo:rustc-link-arg-bin=flygo={}",
                res_obj.to_str().expect("obj path utf8")
            );
            // Link the same resource into the launcher binary (its own icon).
            let launcher_obj = out_dir.join("launcher_resource.o");
            let st = Command::new("windres")
                .arg(&rc)
                .arg("-O")
                .arg("coff")
                .arg("-o")
                .arg(&launcher_obj)
                .current_dir(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"))
                .status();
            if let Ok(s) = st {
                if s.success() {
                    println!(
                        "cargo:rustc-link-arg-bin=launcher={}",
                        launcher_obj.to_str().expect("obj path utf8")
                    );
                }
            }
        }
        Ok(s) => {
            panic!(
                "windres failed for {} (exit {:?}); ensure GNU binutils (windres) is on PATH",
                rc.display(),
                s.code()
            );
        }
        Err(e) => {
            panic!("could not run windres ({e}); ensure GNU binutils is on PATH");
        }
    }

    // Make the icon file available (references for docs/tooling).
    let _ = ico;
}
