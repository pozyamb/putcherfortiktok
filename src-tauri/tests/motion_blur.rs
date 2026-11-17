//! Motion blur: RIFE intermediate frames averaged over a shutter window.

use std::path::Path;

use putcherfortiktok_lib::enhance::{self, EnhanceOptions};
use putcherfortiktok_lib::{probe, remux, tools};

fn opts(json: &str) -> EnhanceOptions {
    serde_json::from_str(json).unwrap()
}

#[test]
fn plan_follows_strength_and_frame_rate() {
    // 60 fps kept, 8 steps: 8 RIFE frames per source frame; 100 % averages one frame interval.
    let p = enhance::blur_plan(60.0, &opts(r#"{"enabled":true,"motionBlur":100,"blurSteps":8}"#));
    assert_eq!((p.out_fps, p.per_source, p.sub_fps, p.mix), (60.0, 8, 480.0, 8));
    // 300 % is three intervals; 4 steps halves the work.
    let p = enhance::blur_plan(60.0, &opts(r#"{"enabled":true,"motionBlur":300,"blurSteps":4}"#));
    assert_eq!((p.per_source, p.mix), (4, 12));
    // 60 → 120 fps: still 4 or 8 steps per output frame, so twice as many per source frame.
    let p = enhance::blur_plan(60.0, &opts(r#"{"enabled":true,"motionBlur":50,"blurSteps":4,"fps":"rife","fpsTarget":120}"#));
    assert_eq!((p.out_fps, p.per_source, p.mix), (120.0, 8, 2));
    // Rates that do not divide evenly round to the nearest whole number of steps.
    let p = enhance::blur_plan(25.0, &opts(r#"{"enabled":true,"motionBlur":100,"fps":"blend","fpsTarget":60}"#));
    assert_eq!((p.out_fps, p.per_source, p.mix), (60.0, 19, 8));
    // Very weak blur still averages at least one frame.
    let p = enhance::blur_plan(60.0, &opts(r#"{"enabled":true,"motionBlur":5}"#));
    assert_eq!(p.mix, 1);
    // About 1200 generated frames per RIFE run.
    let p = enhance::blur_plan(60.0, &opts(r#"{"enabled":true,"motionBlur":100}"#));
    assert_eq!(enhance::blur_chunk(&p), 150);
}

#[test]
fn filter_centres_the_window() {
    let p = enhance::blur_plan(60.0, &opts(r#"{"enabled":true,"motionBlur":100}"#));
    assert_eq!(enhance::blur_filter(&p), "tmix=frames=8,trim=start_frame=3,setpts=PTS-STARTPTS,tpad=stop_mode=clone:stop=3,fps=60");
    let p = enhance::blur_plan(60.0, &opts(r#"{"enabled":true,"motionBlur":20,"blurSteps":4}"#));
    assert_eq!(enhance::blur_filter(&p), "tmix=frames=1,fps=60");
}

#[test]
fn encoder_reads_the_stream_and_keeps_the_rate() {
    let report = probe::report_from_json(
        "C:/v/clip.mp4",
        1000,
        br#"{"streams":[
            {"codec_type":"video","codec_name":"h264","width":1080,"height":1920,"pix_fmt":"yuv420p",
             "r_frame_rate":"30/1","avg_frame_rate":"30/1"},
            {"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2}],
           "format":{"format_name":"mov,mp4","duration":"4"}}"#,
        Some(true),
    )
    .unwrap();
    let o = opts(r#"{"enabled":true,"motionBlur":200,"fps":"blend","fpsTarget":60,"sharpen":1}"#);
    assert!(enhance::uses_blur(&report, &o));
    assert!(!enhance::uses_blur(&report, &opts(r#"{"enabled":false,"motionBlur":200}"#)));
    assert!(!enhance::uses_blur(&report, &opts(r#"{"enabled":true,"motionBlur":0}"#)));

    let plan = enhance::blur_plan(30.0, &o);
    let args = enhance::build_args_blur(&report, &plan, Path::new("C:/v/o.mp4"), &o);
    let pos = |x: &str| args.iter().position(|a| a == x).unwrap();
    assert_eq!(args[pos("-f") + 1], "image2pipe");
    assert_eq!(args[pos("-framerate") + 1], "480");
    assert!(args.iter().any(|a| a == "pipe:0") && args.iter().any(|a| a == "1:a:0?"));
    let chain = &args[pos("-vf") + 1];
    assert!(chain.starts_with("tmix=frames=16,"), "{chain}");
    // The blur already gives 60 fps: libplacebo must not mix frames again.
    assert!(!chain.contains("frame_mixer") && chain.ends_with("cas=0.25"), "{chain}");
    assert_eq!(args[pos("-g") + 1], "120");
}

/// Real run through the same function the app uses: 12 s at 30 fps is three RIFE
/// chunks, so the joins between chunks are exercised. Skipped without RIFE or NVENC.
#[test]
fn blur_end_to_end() {
    let (Ok(ffmpeg), Some(_)) = (tools::ffmpeg(), tools::rife()) else {
        eprintln!("ffmpeg or RIFE not found, skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("blur-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("sample.mp4");
    let st = tools::command(&ffmpeg)
        .args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=360x640:r=30:d=12",
               "-f", "lavfi", "-i", "sine=f=440:d=12", "-c:v", "libx264", "-pix_fmt", "yuv420p",
               "-c:a", "aac", "-shortest"])
        .arg(&input)
        .status()
        .unwrap();
    assert!(st.success());

    let report = probe::probe(input.to_str().unwrap()).unwrap();
    let enh = opts(r#"{"enabled":true,"motionBlur":150,"blurSteps":8,"bitrateMbps":8}"#);
    let output = remux::output_path(&input);
    let stage1 = remux::stage1_path(&output);
    let frames = remux::frames_dir(&output);
    let stages = std::cell::RefCell::new(Vec::<&str>::new());
    let progress = |_: f64, _: Option<f64>, stage: &'static str| {
        let mut s = stages.borrow_mut();
        if s.last() != Some(&stage) {
            s.push(stage);
        }
    };
    if let Err(e) = remux::blur_to_file(&report, &enh, &frames, &stage1, &progress) {
        if e.contains("nvenc") || e.contains("NVENC") {
            eprintln!("NVENC not available, skipping: {e}");
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }
        panic!("{e}");
    }
    assert_eq!(*stages.borrow(), ["Извлечение кадров", "Размытие движения"]);
    assert!(!frames.exists(), "temporary frames left behind");

    let done = probe::probe(stage1.to_str().unwrap()).unwrap();
    let v = done.video.as_ref().unwrap();
    assert!((v.fps - 30.0).abs() < 0.1, "fps {}", v.fps);
    assert!((done.duration_s - report.duration_s).abs() < 0.1, "duration {}", done.duration_s);
    assert!(done.audio.is_some());
    let count = tools::command(tools::ffprobe().unwrap())
        .args(["-v", "error", "-count_frames", "-select_streams", "v", "-show_entries",
               "stream=nb_read_frames", "-of", "csv=p=0"])
        .arg(&stage1)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "360");
    std::fs::remove_dir_all(&dir).unwrap();
}
