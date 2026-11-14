//! Runs the app's pipeline from the command line (no window).
//!
//!   cargo run --example apply -- input.mp4 [--audio-tail] [--retime N] [--rate mvhd|elst]
//!                                [--enhance <json>]
//!
//! `--enhance` takes the same JSON as the app, e.g.
//!   --enhance "{\"shortSide\":2160,\"sharpen\":2,\"fps\":\"blend\"}"
//! The result is written next to the input as `<name>_tiktok.mp4`.

use std::process::Stdio;

use putcherfortiktok_lib::enhance::{self, EnhanceOptions};
use putcherfortiktok_lib::{probe, remux, tools};

fn run(args: Vec<String>) -> Result<(), String> {
    let out = tools::command(tools::ffmpeg()?)
        .args(&args)
        .stdout(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr)))
    }
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let input = args.first().ok_or("usage: apply <input> [--audio-tail] [--retime N] [--rate mvhd|elst] [--enhance <json>]")?;
    let mut json = serde_json::Map::new();
    let mut enh: Option<EnhanceOptions> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--audio-tail" => {
                json.insert("audioTail".into(), true.into());
            }
            "--retime" => {
                i += 1;
                let n: u32 = args.get(i).and_then(|v| v.parse().ok()).ok_or("--retime needs a number")?;
                json.insert("retimeFactor".into(), n.into());
            }
            "--rate" => {
                i += 1;
                json.insert("ratePatch".into(), args.get(i).cloned().ok_or("--rate needs a value")?.into());
            }
            "--enhance" => {
                i += 1;
                let raw = args.get(i).ok_or("--enhance needs JSON")?;
                let mut o: EnhanceOptions = serde_json::from_str(raw).map_err(|e| e.to_string())?;
                o.enabled = true;
                enh = Some(o);
            }
            other => return Err(format!("unknown option {other}")),
        }
        i += 1;
    }
    let opts: remux::Options = serde_json::from_value(json.into()).map_err(|e| e.to_string())?;

    let mut source = probe::probe(input)?;
    let output = remux::output_path(std::path::Path::new(input));
    let stage1 = remux::stage1_path(&output);
    if let Some(o) = &enh {
        let started = std::time::Instant::now();
        run(enhance::build_args(&source, &stage1, o))?;
        eprintln!("enhance stage: {:.1}s for {:.1}s of video", started.elapsed().as_secs_f64(), source.duration_s);
        source = probe::probe(&stage1.to_string_lossy())?;
    }
    let result = run(remux::build_args(&source, &output, opts)).and_then(|_| remux::post_process(&output, opts));
    let _ = std::fs::remove_file(&stage1);
    result?;
    println!("{}", output.display());
    Ok(())
}
