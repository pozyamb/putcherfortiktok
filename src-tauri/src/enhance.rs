//! Optional first stage: GPU enhancement (upscale, denoise, sharpen, frame rate)
//! and NVENC encoding. Its output then goes through the remux stage, so any
//! container patch is always applied last, to the final file.
//!
//! Scaling, debanding and frame blending run in libplacebo on the GPU (Vulkan);
//! encoding is NVENC. Denoise, sharpen and motion interpolation are CPU filters.

use std::path::Path;

use serde::Deserialize;

use crate::probe::ProbeReport;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Upscaler {
    /// ewa_lanczossharp: crisp, best for clean sources
    #[default]
    Sharp,
    /// ewa_lanczos: softer, hides compression artifacts better
    Smooth,
    /// bilinear: fastest
    Fast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FpsMode {
    #[default]
    Keep,
    /// Target rate by blending neighbouring frames (GPU, fast)
    Blend,
    /// Target rate by motion interpolation (CPU, slow, can produce artifacts)
    Motion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    #[default]
    H264,
    Hevc,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EnhanceOptions {
    pub enabled: bool,
    /// Target size of the short side in pixels; 0 keeps the source size.
    pub short_side: u32,
    pub upscaler: Upscaler,
    /// 0 = off … 3 = strong
    pub denoise: u8,
    /// 0 = off … 3 = strong
    pub sharpen: u8,
    pub deband: bool,
    pub fps: FpsMode,
    /// Frame rate to reach when `fps` is not `Keep`: 60 or 120.
    pub fps_target: u32,
    pub codec: Codec,
    pub bitrate_mbps: u32,
}

impl Default for EnhanceOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            short_side: 0,
            upscaler: Upscaler::Sharp,
            denoise: 0,
            sharpen: 0,
            deband: false,
            fps: FpsMode::Keep,
            fps_target: 60,
            codec: Codec::H264,
            bitrate_mbps: 20,
        }
    }
}

/// Output frame size: the short side becomes `short_side`, aspect ratio kept, both even.
pub fn target_size(width: u32, height: u32, short_side: u32) -> (u32, u32) {
    let even = |v: f64| ((v / 2.0).round() as u32).max(1) * 2;
    if short_side == 0 || width == 0 || height == 0 {
        return (even(width as f64), even(height as f64));
    }
    let k = short_side as f64 / width.min(height) as f64;
    (even(width as f64 * k), even(height as f64 * k))
}

/// Frame rate of the encoded stream.
pub fn target_fps(source_fps: f64, mode: FpsMode, target: u32) -> f64 {
    match mode {
        FpsMode::Keep => source_fps,
        // Never lower the rate: a 120 fps source stays as it is.
        _ => source_fps.max(fps_goal(target) as f64),
    }
}

pub fn filter_chain(report: &ProbeReport, o: &EnhanceOptions) -> String {
    let v = report.video.as_ref();
    let (sw, sh, sfps) = v.map(|v| (v.width, v.height, v.fps)).unwrap_or((0, 0, 0.0));
    let (w, h) = target_size(sw, sh, o.short_side);
    let goal = fps_goal(o.fps_target);
    let raise_fps = o.fps != FpsMode::Keep && sfps > 0.0 && sfps < goal as f64 - 1.0;
    let mut chain: Vec<String> = Vec::new();

    match o.denoise.min(3) {
        1 => chain.push("hqdn3d=1.5:1.5:3:3".into()),
        2 => chain.push("hqdn3d=3:2.5:5:4".into()),
        3 => chain.push("hqdn3d=5:4:8:6".into()),
        _ => {}
    }
    if raise_fps && o.fps == FpsMode::Motion {
        // Interpolate at the source size, before upscaling: far fewer pixels to estimate motion on.
        chain.push(format!("minterpolate=fps={goal}:mi_mode=mci:mc_mode=aobmc:vsbmc=1"));
    }

    let upscaler = match o.upscaler {
        Upscaler::Sharp => "ewa_lanczossharp",
        Upscaler::Smooth => "ewa_lanczos",
        Upscaler::Fast => "bilinear",
    };
    let mut placebo = format!("libplacebo=w={w}:h={h}:upscaler={upscaler}:downscaler=mitchell");
    if o.deband {
        placebo.push_str(":deband=true");
    }
    if raise_fps && o.fps == FpsMode::Blend {
        placebo.push_str(&format!(":fps={goal}:frame_mixer=mitchell_clamp"));
    }
    // Always deliver 8-bit BT.709 limited range; HDR sources get tone-mapped here.
    placebo.push_str(":format=yuv420p:colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv");
    chain.push(placebo);

    match o.sharpen.min(3) {
        1 => chain.push("cas=0.25".into()),
        2 => chain.push("cas=0.5".into()),
        3 => chain.push("cas=0.8".into()),
        _ => {}
    }
    chain.join(",")
}

/// ffmpeg arguments for the enhancement + NVENC stage.
pub fn build_args(report: &ProbeReport, output: &Path, o: &EnhanceOptions) -> Vec<String> {
    let s = |x: &str| x.to_string();
    let fps = target_fps(report.video.as_ref().map(|v| v.fps).unwrap_or(30.0), o.fps, o.fps_target);
    let gop = ((fps * 2.0).round() as u32).clamp(24, 480);
    let mbps = o.bitrate_mbps.clamp(2, 100);

    let mut a = vec![
        s("-hide_banner"), s("-nostdin"), s("-y"), s("-v"), s("error"),
        s("-init_hw_device"), s("vulkan"),
        s("-i"), report.path.clone(),
        s("-map"), s("0:v:0"), s("-map"), s("0:a:0?"),
        s("-vf"), filter_chain(report, o),
    ];
    let (encoder, profile) = match o.codec {
        Codec::H264 => ("h264_nvenc", "high"),
        Codec::Hevc => ("hevc_nvenc", "main"),
    };
    a.extend([
        s("-c:v"), s(encoder), s("-profile:v"), s(profile),
        s("-preset"), s("p7"), s("-tune"), s("hq"), s("-multipass"), s("fullres"),
        s("-rc"), s("vbr"),
        s("-b:v"), format!("{mbps}M"),
        s("-maxrate"), format!("{}M", mbps * 3 / 2),
        s("-bufsize"), format!("{}M", mbps * 2),
        s("-g"), gop.to_string(), s("-bf"), s("0"),
        s("-pix_fmt"), s("yuv420p"),
        s("-color_primaries"), s("bt709"), s("-color_trc"), s("bt709"),
        s("-colorspace"), s("bt709"), s("-color_range"), s("tv"),
    ]);
    if o.codec == Codec::Hevc {
        a.extend([s("-tag:v"), s("hvc1")]);
    }
    // Keep AAC as is; anything else becomes AAC, which is what TikTok expects.
    match report.audio.as_ref().map(|x| x.codec.as_str()) {
        Some("aac") | None => a.extend([s("-c:a"), s("copy")]),
        Some(_) => a.extend([s("-c:a"), s("aac"), s("-b:a"), s("256k")]),
    }
    a.extend([s("-movflags"), s("+faststart"), s("-progress"), s("pipe:1"), s("-nostats")]);
    a.push(output.to_string_lossy().into_owned());
    a
}

/// Supported frame-rate goals: 120, otherwise 60.
pub fn fps_goal(target: u32) -> u32 {
    if target >= 120 {
        120
    } else {
        60
    }
}
