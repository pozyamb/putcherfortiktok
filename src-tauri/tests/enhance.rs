//! Enhancement + NVENC stage, and the full order: enhance → remux → patch last.

use putcherfortiktok_lib::boxpatch::{self, AUDIO_TAIL_FRAMES, AUDIO_TAIL_PATTERN};
use putcherfortiktok_lib::enhance::{self, EnhanceOptions, FpsMode};
use putcherfortiktok_lib::{probe, remux, tools};

#[test]
fn target_size_keeps_aspect_and_even_dimensions() {
    assert_eq!(enhance::target_size(720, 1280, 1080), (1080, 1920));
    assert_eq!(enhance::target_size(1920, 1080, 2160), (3840, 2160));
    assert_eq!(enhance::target_size(1080, 1440, 0), (1080, 1440));
    assert_eq!(enhance::target_size(853, 480, 1080), (1920, 1080));
    assert_eq!(enhance::target_fps(30.0, FpsMode::Blend, 60), 60.0);
    assert_eq!(enhance::target_fps(120.0, FpsMode::Motion, 60), 120.0);
    assert_eq!(enhance::target_fps(30.0, FpsMode::Keep, 120), 30.0);
    assert_eq!(enhance::target_fps(60.0, FpsMode::Blend, 120), 120.0);
}

fn sample_report() -> probe::ProbeReport {
    probe::report_from_json(
        "C:/v/clip.mp4",
        1000,
        br#"{"streams":[
            {"codec_type":"video","codec_name":"h264","width":720,"height":1280,"pix_fmt":"yuv420p",
             "r_frame_rate":"30/1","avg_frame_rate":"30/1"},
            {"codec_type":"audio","codec_name":"opus","sample_rate":"48000","channels":2}],
           "format":{"format_name":"mov,mp4","duration":"4"}}"#,
        Some(true),
    )
    .unwrap()
}

#[test]
fn filter_chain_follows_options() {
    let r = sample_report();
    let o: EnhanceOptions = serde_json::from_str(
        r#"{"enabled":true,"shortSide":1080,"upscaler":"smooth","denoise":2,"sharpen":1,"deband":true,"fps":"blend"}"#,
    )
    .unwrap();
    let chain = enhance::filter_chain(&r, &o);
    assert!(chain.starts_with("hqdn3d="), "{chain}");
    assert!(chain.contains("libplacebo=w=1080:h=1920:upscaler=ewa_lanczos:"), "{chain}");
    assert!(chain.contains(":deband=true") && chain.contains(":fps=60:frame_mixer="), "{chain}");
    assert!(chain.ends_with("cas=0.25"), "{chain}");

    // Motion interpolation runs before the upscale; nothing is added when all is off.
    let o: EnhanceOptions = serde_json::from_str(r#"{"enabled":true,"fps":"motion"}"#).unwrap();
    let chain = enhance::filter_chain(&r, &o);
    assert!(chain.find("minterpolate").unwrap() < chain.find("libplacebo").unwrap());
    assert!(!chain.contains("hqdn3d") && !chain.contains("cas=") && !chain.contains("deband"));

    // Non-AAC audio is converted, and the encoder follows the codec choice.
    let o: EnhanceOptions = serde_json::from_str(r#"{"enabled":true,"codec":"hevc","bitrateMbps":30}"#).unwrap();
    let args = enhance::build_args(&r, std::path::Path::new("C:/v/out.mp4"), &o);
    let has = |x: &str| args.iter().any(|a| a == x);
    assert!(has("hevc_nvenc") && has("30M") && has("45M") && has("aac") && has("hvc1"));
}

/// Real GPU run: 720p30 → 1080p60 through NVENC, then remux and the audio tail patch.
/// Skipped when ffmpeg or NVENC is unavailable.
#[test]
fn enhance_then_patch_end_to_end() {
    let (Ok(ffmpeg), Ok(_)) = (tools::ffmpeg(), tools::ffprobe()) else {
        eprintln!("ffmpeg/ffprobe not found, skipping");
        return;
    };
    let nvenc = tools::command(&ffmpeg)
        .args(["-hide_banner", "-v", "error", "-f", "lavfi", "-i", "color=c=black:s=256x256:d=0.1",
               "-frames:v", "1", "-c:v", "h264_nvenc", "-f", "null", "-"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !nvenc {
        eprintln!("NVENC not available, skipping");
        return;
    }

    let dir = std::env::temp_dir().join(format!("enh-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("sample.mp4");
    let status = tools::command(&ffmpeg)
        .args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=720x1280:r=30:d=3",
               "-f", "lavfi", "-i", "sine=f=440:d=3", "-c:v", "libx264", "-pix_fmt", "yuv420p",
               "-c:a", "aac", "-shortest"])
        .arg(&input)
        .status()
        .unwrap();
    assert!(status.success());

    let report = probe::probe(input.to_str().unwrap()).unwrap();
    let enh: EnhanceOptions = serde_json::from_str(
        r#"{"enabled":true,"shortSide":1080,"denoise":1,"sharpen":2,"fps":"blend","bitrateMbps":12}"#,
    )
    .unwrap();
    let opts: remux::Options = serde_json::from_str(r#"{"audioTail":true}"#).unwrap();

    let output = remux::output_path(&input);
    let stage1 = remux::stage1_path(&output);
    let run = |args: Vec<String>| {
        let out = tools::command(&ffmpeg).args(&args).stdout(std::process::Stdio::null()).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    run(enhance::build_args(&report, &stage1, &enh));
    let mid = probe::probe(stage1.to_str().unwrap()).unwrap();
    run(remux::build_args(&mid, &output, opts));
    remux::post_process(&output, opts).unwrap();

    let done = probe::probe(output.to_str().unwrap()).unwrap();
    let v = done.video.as_ref().unwrap();
    assert_eq!((v.width, v.height), (1080, 1920));
    assert!((v.fps - 60.0).abs() < 0.5, "fps {}", v.fps);
    assert_eq!(v.codec, "h264");
    assert_eq!(v.bit_depth, 8);
    assert_eq!(v.color_primaries.as_deref(), Some("bt709"));
    assert_eq!(done.moov_first, Some(true));
    assert!((done.duration_s - report.duration_s).abs() < 0.2);

    // The patch is in the final file: its last audio frames carry the pattern.
    let bytes = std::fs::read(&output).unwrap();
    let moov_at = bytes.windows(4).position(|w| w == b"moov").unwrap();
    let moov_size = u32::from_be_bytes(bytes[moov_at - 4..moov_at].try_into().unwrap()) as usize;
    let samples = boxpatch::audio_samples(&bytes[moov_at + 4..moov_at - 4 + moov_size]).unwrap();
    for &(off, size) in &samples[samples.len() - AUDIO_TAIL_FRAMES..] {
        let n = AUDIO_TAIL_PATTERN.len().min(size as usize);
        assert_eq!(&bytes[off as usize..off as usize + n], &AUDIO_TAIL_PATTERN[..n]);
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn frame_rate_goal_of_120() {
    let r = sample_report();
    let o: EnhanceOptions = serde_json::from_str(r#"{"enabled":true,"fps":"blend","fpsTarget":120}"#).unwrap();
    assert!(enhance::filter_chain(&r, &o).contains(":fps=120:frame_mixer="));
    let o: EnhanceOptions = serde_json::from_str(r#"{"enabled":true,"fps":"motion","fpsTarget":120}"#).unwrap();
    assert!(enhance::filter_chain(&r, &o).contains("minterpolate=fps=120:"));
    // GOP stays two seconds long.
    let args = enhance::build_args(&r, std::path::Path::new("C:/v/out.mp4"), &o);
    let g = args.iter().position(|a| a == "-g").unwrap();
    assert_eq!(args[g + 1], "240");
}

#[test]
fn rife_helpers() {
    let r = sample_report(); // 30 fps
    let o: EnhanceOptions = serde_json::from_str(r#"{"enabled":true,"fps":"rife","fpsTarget":120}"#).unwrap();
    assert!(enhance::uses_rife(&r, &o));
    // RIFE makes the frames, so ffmpeg must not change the rate again.
    let chain = enhance::filter_chain(&r, &o);
    assert!(!chain.contains("fps=") && !chain.contains("minterpolate"), "{chain}");
    assert_eq!(enhance::rife_frame_count(90, 30.0, 120), 360);
    assert_eq!(enhance::rife_frame_count(250, 25.0, 60), 600);

    let off: EnhanceOptions = serde_json::from_str(r#"{"enabled":false,"fps":"rife"}"#).unwrap();
    assert!(!enhance::uses_rife(&r, &off));

    let args = enhance::build_args_from_frames(&r, std::path::Path::new("C:/t/out"), 120.0, std::path::Path::new("C:/v/o.mp4"), &o);
    let pos = |x: &str| args.iter().position(|a| a == x).unwrap();
    assert_eq!(args[pos("-framerate") + 1], "120");
    assert!(args.iter().any(|a| a.ends_with("%08d.png")));
    assert!(args.iter().any(|a| a == "1:a:0?"));

    let listing = "[0 NVIDIA GeForce RTX 5060]  queueC=2[8]\n[1 AMD Radeon(TM) Graphics]  queueC=1[2]";
    assert_eq!(tools::parse_rife_gpu(listing), 0);
    assert_eq!(tools::parse_rife_gpu("[0 AMD Radeon(TM) Graphics] x\n[1 NVIDIA GeForce RTX 5060] y"), 1);
    assert_eq!(tools::parse_rife_gpu("nothing here"), 0);
}

/// Real run with RIFE: 30 fps → 60 fps, exact length kept. Skipped when RIFE or NVENC is missing.
#[test]
fn rife_end_to_end() {
    let (Ok(ffmpeg), Some((rife_exe, model))) = (tools::ffmpeg(), tools::rife()) else {
        eprintln!("ffmpeg or RIFE not found, skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("rife-e2e-{}", std::process::id()));
    let (din, dout) = (dir.join("f").join("in"), dir.join("f").join("out"));
    std::fs::create_dir_all(&din).unwrap();
    std::fs::create_dir_all(&dout).unwrap();
    let input = dir.join("sample.mp4");
    let run = |args: Vec<String>| {
        let out = tools::command(&ffmpeg).args(&args).stdout(std::process::Stdio::null()).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    let s = |x: &str| x.to_string();
    run(vec![s("-hide_banner"), s("-v"), s("error"), s("-y"), s("-f"), s("lavfi"), s("-i"), s("testsrc2=s=360x640:r=30:d=2"),
             s("-f"), s("lavfi"), s("-i"), s("sine=f=440:d=2"), s("-c:v"), s("libx264"), s("-pix_fmt"), s("yuv420p"),
             s("-c:a"), s("aac"), s("-shortest"), input.to_string_lossy().into_owned()]);

    let report = probe::probe(input.to_str().unwrap()).unwrap();
    let enh: EnhanceOptions = serde_json::from_str(r#"{"enabled":true,"fps":"rife","fpsTarget":60,"bitrateMbps":8}"#).unwrap();
    run(enhance::extract_frames_args(&report, &din));
    let frames_in = std::fs::read_dir(&din).unwrap().count() as u64;
    assert_eq!(frames_in, 60);
    let frames_out = enhance::rife_frame_count(frames_in, 30.0, 60);
    let gpu = tools::rife_gpu_index(&rife_exe);
    let out = tools::command(&rife_exe).args(enhance::rife_args(&din, &dout, frames_out, &model, gpu)).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read_dir(&dout).unwrap().count() as u64, 120);

    let stage1 = dir.join("stage1.mp4");
    let nvenc_args = enhance::build_args_from_frames(&report, &dout, 60.0, &stage1, &enh);
    let enc = tools::command(&ffmpeg).args(&nvenc_args).stdout(std::process::Stdio::null()).output().unwrap();
    if !enc.status.success() {
        eprintln!("NVENC not available, skipping the encode check");
        std::fs::remove_dir_all(&dir).unwrap();
        return;
    }
    let done = probe::probe(stage1.to_str().unwrap()).unwrap();
    let v = done.video.as_ref().unwrap();
    assert!((v.fps - 60.0).abs() < 0.5, "fps {}", v.fps);
    assert!((done.duration_s - report.duration_s).abs() < 0.1, "duration {}", done.duration_s);
    assert!(done.audio.is_some());
    std::fs::remove_dir_all(&dir).unwrap();
}
