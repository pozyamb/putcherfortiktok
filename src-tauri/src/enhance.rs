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
    /// Target rate by the RIFE neural network (GPU; needs the optional rife-ncnn-vulkan
    /// tool and temporary disk space for every frame as PNG)
    Rife,
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
    /// Motion blur strength in percent of one output frame interval
    /// (100 = 360° shutter, 500 = maximum); 0 = off.
    pub motion_blur: u32,
    /// Fading trail behind moving objects, in output frames (0 = off, 15 = maximum).
    pub trail_frames: u32,
    /// Intermediate frames RIFE makes per output frame for the blur: 4 or 8.
    pub blur_steps: u32,
}

pub const MAX_BLUR_PERCENT: u32 = 500;
pub const MAX_TRAIL_FRAMES: u32 = 15;

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
            motion_blur: 0,
            trail_frames: 0,
            blur_steps: 8,
        }
    }
}

/// How the motion blur is made: RIFE renders `per_source` frames for every source
/// frame (`sub_fps` in total), `mix` consecutive ones are averaged, centred on the
/// output frame's moment, and every result at `out_fps` is kept. With a trail, the
/// last `trail` one-frame slices (`slice` RIFE frames each) are added behind it,
/// fading with age.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlurPlan {
    pub out_fps: f64,
    pub per_source: u32,
    pub sub_fps: f64,
    pub mix: u32,
    pub slice: u32,
    pub trail: u32,
}

/// True when this job gets motion blur or a trail.
pub fn uses_blur(report: &ProbeReport, o: &EnhanceOptions) -> bool {
    let sfps = report.video.as_ref().map(|v| v.fps).unwrap_or(0.0);
    o.enabled && (o.motion_blur > 0 || o.trail_frames > 0) && sfps > 0.0
}

pub fn blur_plan(source_fps: f64, o: &EnhanceOptions) -> BlurPlan {
    let out_fps = target_fps(source_fps, o.fps, o.fps_target);
    let steps = if o.blur_steps <= 4 { 4.0 } else { 8.0 };
    let per_source = ((out_fps * steps / source_fps).round() as u32).max(1);
    let sub_fps = source_fps * per_source as f64;
    let per_out = sub_fps / out_fps;
    let strength = o.motion_blur.min(MAX_BLUR_PERCENT) as f64 / 100.0;
    let mix = ((strength * per_out).round() as u32).max(1);
    let slice = (per_out.round() as u32).max(1);
    BlurPlan { out_fps, per_source, sub_fps, mix, slice, trail: o.trail_frames.min(MAX_TRAIL_FRAMES) }
}

/// Average of `n` consecutive RIFE frames, centred on each output frame's moment.
/// tmix averages the current frame with the ones before it; dropping the first
/// half-window moves each average back to the middle of its time span, and as many
/// copies of the last frame at the end keep the frame count exact. Equal weights
/// keep tmix on its fast running-sum path.
fn centred_average(n: u32, out_fps: f64) -> String {
    let mut f = format!("tmix=frames={n}");
    let skip = (n - 1) / 2;
    if skip > 0 {
        f.push_str(&format!(",trim=start_frame={skip},setpts=PTS-STARTPTS,tpad=stop_mode=clone:stop={skip}"));
    }
    f.push_str(&format!(",fps={out_fps}"));
    f
}

/// Filters that turn the RIFE frames into blurred frames at the output rate.
///
/// The trail is built on a second branch at the output rate, where weighted
/// averaging is cheap: one-frame slices (360° blur each, so the trail has no gaps),
/// the last `trail` of them weighted 1 … n from oldest to newest and the current one
/// 0, then mixed half and half with the blurred frame: however long the trail, the
/// object itself keeps half its strength.
pub fn blur_filter(p: &BlurPlan) -> String {
    let main = centred_average(p.mix, p.out_fps);
    if p.trail == 0 {
        return main;
    }
    let n = p.trail;
    let weights: Vec<String> = (1..=n).map(|i| i.to_string()).chain(["0".into()]).collect();
    format!(
        "split[bm][bs];[bm]{main}[bmain];[bs]{slices},tmix=frames={frames}:weights='{w}'[btrail];[bmain][btrail]mix=inputs=2:weights='1 1'",
        slices = centred_average(p.slice, p.out_fps),
        frames = n + 1,
        w = weights.join(" "),
    )
}

/// Source frames per RIFE run, so that one run makes about 1200 frames on disk.
pub fn blur_chunk(p: &BlurPlan) -> u64 {
    (1200 / p.per_source.max(1) as u64).clamp(8, 300)
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

/// True when this job goes through RIFE (the source is below the frame-rate goal).
pub fn uses_rife(report: &ProbeReport, o: &EnhanceOptions) -> bool {
    let sfps = report.video.as_ref().map(|v| v.fps).unwrap_or(0.0);
    o.enabled && o.fps == FpsMode::Rife && sfps > 0.0 && sfps < fps_goal(o.fps_target) as f64 - 1.0
}

/// How many frames RIFE must produce from `frames_in` source frames.
pub fn rife_frame_count(frames_in: u64, source_fps: f64, target: u32) -> u64 {
    if source_fps <= 0.0 {
        return frames_in;
    }
    ((frames_in as f64) * fps_goal(target) as f64 / source_fps).round() as u64
}

/// ffmpeg arguments that dump every video frame as PNG into `frames_dir`.
pub fn extract_frames_args(report: &ProbeReport, frames_dir: &Path) -> Vec<String> {
    let s = |x: &str| x.to_string();
    vec![
        s("-hide_banner"), s("-nostdin"), s("-y"), s("-v"), s("error"),
        s("-i"), report.path.clone(),
        s("-map"), s("0:v:0"), s("-fps_mode"), s("passthrough"),
        s("-pix_fmt"), s("rgb24"), s("-compression_level"), s("1"),
        s("-progress"), s("pipe:1"), s("-nostats"),
        frames_dir.join("%08d.png").to_string_lossy().into_owned(),
    ]
}

/// rife-ncnn-vulkan arguments: `frames_out` frames from the PNGs in `input_dir`.
pub fn rife_args(input_dir: &Path, output_dir: &Path, frames_out: u64, model_dir: &Path, gpu: u32) -> Vec<String> {
    let s = |x: &str| x.to_string();
    let p = |x: &Path| x.to_string_lossy().into_owned();
    vec![
        s("-v"),
        s("-i"), p(input_dir), s("-o"), p(output_dir),
        s("-n"), frames_out.to_string(),
        s("-m"), p(model_dir),
        s("-g"), gpu.to_string(),
        s("-j"), s("2:4:4"),
        s("-f"), s("%08d.png"),
    ]
}

/// Where the encoder takes the video from.
enum VideoIn<'a> {
    Source,
    /// Numbered PNG files at this rate.
    Frames(&'a Path, f64),
    /// PNG images written to stdin at `sub_fps`, blurred down to `out_fps`.
    Pipe(&'a BlurPlan),
}

/// ffmpeg arguments for the enhancement + NVENC stage.
pub fn build_args(report: &ProbeReport, output: &Path, o: &EnhanceOptions) -> Vec<String> {
    build_args_with(report, output, o, VideoIn::Source)
}

/// Like `build_args`, but the video comes from numbered PNG frames at `fps`
/// (the RIFE output); the audio still comes from the source file.
pub fn build_args_from_frames(report: &ProbeReport, frames_dir: &Path, fps: f64, output: &Path, o: &EnhanceOptions) -> Vec<String> {
    build_args_with(report, output, o, VideoIn::Frames(frames_dir, fps))
}

/// Like `build_args`, but the video is a stream of PNG images on stdin at `plan.sub_fps`
/// (RIFE output, chunk by chunk), blurred down to `plan.out_fps` before the other filters.
pub fn build_args_blur(report: &ProbeReport, plan: &BlurPlan, output: &Path, o: &EnhanceOptions) -> Vec<String> {
    build_args_with(report, output, o, VideoIn::Pipe(plan))
}

fn build_args_with(report: &ProbeReport, output: &Path, o: &EnhanceOptions, input: VideoIn) -> Vec<String> {
    let s = |x: &str| x.to_string();
    let source_fps = report.video.as_ref().map(|v| v.fps).unwrap_or(30.0);
    let fps = match input {
        VideoIn::Frames(_, f) => f,
        VideoIn::Pipe(p) => p.out_fps,
        VideoIn::Source => target_fps(source_fps, o.fps, o.fps_target),
    };
    let gop = ((fps * 2.0).round() as u32).clamp(24, 480);
    let mbps = o.bitrate_mbps.clamp(2, 100);

    let mut a = vec![
        s("-hide_banner"), s("-nostdin"), s("-y"), s("-v"), s("error"),
        s("-init_hw_device"), s("vulkan"),
    ];
    let chain = match input {
        VideoIn::Frames(dir, f) => {
            a.extend([
                s("-framerate"), format!("{f}"),
                s("-i"), dir.join("%08d.png").to_string_lossy().into_owned(),
                s("-i"), report.path.clone(),
                s("-map"), s("0:v:0"), s("-map"), s("1:a:0?"),
            ]);
            filter_chain(report, o)
        }
        VideoIn::Pipe(p) => {
            a.extend([
                s("-f"), s("image2pipe"), s("-c:v"), s("png"), s("-framerate"), format!("{}", p.sub_fps),
                s("-i"), s("pipe:0"),
                s("-i"), report.path.clone(),
                s("-map"), s("0:v:0"), s("-map"), s("1:a:0?"),
            ]);
            // The blur already delivers the output rate: no further frame-rate change.
            let rest = filter_chain(report, &EnhanceOptions { fps: FpsMode::Keep, ..*o });
            format!("{},{rest}", blur_filter(p))
        }
        VideoIn::Source => {
            a.extend([s("-i"), report.path.clone(), s("-map"), s("0:v:0"), s("-map"), s("0:a:0?")]);
            filter_chain(report, o)
        }
    };
    a.extend([s("-vf"), chain]);
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
