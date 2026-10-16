//! Compiles the GLSL shaders to SPIR-V with glslangValidator.
use std::{env, path::PathBuf, process::Command};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    for name in ["fullscreen.vert", "video.frag"] {
        let src = format!("shaders/{name}");
        println!("cargo:rerun-if-changed={src}");
        let status = Command::new("glslangValidator")
            .args(["-V", &src, "-o"])
            .arg(out.join(format!("{name}.spv")))
            .status()
            .expect("glslangValidator not found (install glslang-tools)");
        assert!(status.success(), "failed to compile {src}");
    }
}
