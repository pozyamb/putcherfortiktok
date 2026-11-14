//! Locating and launching the ffmpeg / ffprobe sidecars.

use std::path::PathBuf;
use std::process::Command;

/// Rust target triple the app was built for; sidecars in `src-tauri/binaries`
/// carry it as a suffix (Tauri `externalBin` convention).
const TARGET_TRIPLE: &str = env!("TARGET_TRIPLE");

/// Finds a tool by name, in order:
/// 1. next to the running executable (bundled app, or dev build where Tauri copied it);
/// 2. `src-tauri/binaries/<name>-<triple>.exe` (dev, after `scripts/fetch-ffmpeg.ps1`);
/// 3. the system PATH.
pub fn locate(name: &str) -> Option<PathBuf> {
    let exe_name = format!("{name}{}", std::env::consts::EXE_SUFFIX);

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join(&exe_name);
            if p.is_file() {
                return Some(p);
            }
        }
    }

    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join(format!("{name}-{TARGET_TRIPLE}{}", std::env::consts::EXE_SUFFIX));
    if dev.is_file() {
        return Some(dev);
    }

    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(&exe_name))
            .find(|p| p.is_file())
    })
}

/// A `Command` that never flashes a console window on Windows.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

pub fn ffmpeg() -> Result<PathBuf, String> {
    locate("ffmpeg").ok_or_else(|| {
        "ffmpeg не найден. Запустите scripts/fetch-ffmpeg.ps1, чтобы скачать его.".to_string()
    })
}

pub fn ffprobe() -> Result<PathBuf, String> {
    locate("ffprobe").ok_or_else(|| {
        "ffprobe не найден. Запустите scripts/fetch-ffmpeg.ps1, чтобы скачать его.".to_string()
    })
}

/// The optional RIFE frame interpolation tool: (executable, model directory).
/// Looked up next to the app (`rife/`) and in `src-tauri/binaries/rife` during development.
pub fn rife() -> Option<(PathBuf, PathBuf)> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.join("rife"));
        }
    }
    dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries").join("rife"));
    dirs.into_iter().find_map(|d| {
        let exe = d.join(format!("rife-ncnn-vulkan{}", std::env::consts::EXE_SUFFIX));
        let model = d.join("rife-v4.6");
        (exe.is_file() && model.is_dir()).then_some((exe, model))
    })
}

/// Index of the NVIDIA GPU as RIFE (ncnn/Vulkan) numbers devices; 0 when it cannot be told.
/// RIFE prints its device list as `[N name]` lines whenever it starts.
pub fn rife_gpu_index(rife_exe: &std::path::Path) -> u32 {
    let out = command(rife_exe).args(["-v", "-i", "?", "-o", "?"]).output();
    let Ok(out) = out else { return 0 };
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    parse_rife_gpu(&text)
}

pub fn parse_rife_gpu(text: &str) -> u32 {
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix('[') else { continue };
        let Some((idx, name)) = rest.split_once(' ') else { continue };
        if name.to_uppercase().contains("NVIDIA") {
            if let Ok(n) = idx.parse() {
                return n;
            }
        }
    }
    0
}
