//! GPU detection: model and driver via nvidia-smi, NVENC support via a tiny test encode.

use serde::Serialize;

use crate::tools;

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GpuInfo {
    /// e.g. "NVIDIA GeForce RTX 5060"; None when no NVIDIA GPU / driver.
    pub name: Option<String>,
    pub driver: Option<String>,
    pub memory_mb: Option<u32>,
    pub is_rtx: bool,
    /// Encoders that actually worked in a test encode: "h264", "hevc", "av1".
    pub nvenc: Vec<String>,
    /// Human-readable reason when something is missing.
    pub note: Option<String>,
}

pub fn detect() -> GpuInfo {
    let mut info = GpuInfo::default();

    match tools::command("nvidia-smi")
        .args([
            "--query-gpu=name,driver_version,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(line) = text.lines().next() {
                let parts: Vec<&str> = line.split(',').map(str::trim).collect();
                info.name = parts.first().map(|s| s.to_string());
                info.driver = parts.get(1).map(|s| s.to_string());
                info.memory_mb = parts.get(2).and_then(|s| s.parse().ok());
            }
        }
        _ => {
            info.note = Some("Видеокарта NVIDIA не найдена или не установлен драйвер.".into());
            return info;
        }
    }

    info.is_rtx = info
        .name
        .as_deref()
        .map(|n| n.to_uppercase().contains("RTX"))
        .unwrap_or(false);

    let ffmpeg = match tools::ffmpeg() {
        Ok(p) => p,
        Err(e) => {
            info.note = Some(e);
            return info;
        }
    };

    // Listing encoders is not enough: a build can include nvenc while the GPU lacks it.
    for (label, encoder) in [("h264", "h264_nvenc"), ("hevc", "hevc_nvenc"), ("av1", "av1_nvenc")] {
        let ok = tools::command(&ffmpeg)
            .args([
                "-hide_banner", "-v", "error", "-f", "lavfi", "-i",
                "color=c=black:s=256x256:d=0.1", "-frames:v", "1", "-c:v", encoder, "-f", "null", "-",
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            info.nvenc.push(label.to_string());
        }
    }

    if info.nvenc.is_empty() {
        info.note = Some("NVENC недоступен: доступен только режим «Ремукс».".into());
    }
    info
}
