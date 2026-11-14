//! Runs the app's remux pipeline from the command line (no window).
//!
//!   cargo run --example apply -- input.mp4 [--audio-tail] [--retime N] [--rate mvhd|elst]
//!
//! The result is written next to the input as `<name>_tiktok.mp4`.

use putcherfortiktok_lib::{probe, remux, tools};

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let input = args.first().ok_or("usage: apply <input> [--audio-tail] [--retime N] [--rate mvhd|elst]")?;
    let mut json = serde_json::Map::new();
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
            other => return Err(format!("unknown option {other}")),
        }
        i += 1;
    }
    let opts: remux::Options = serde_json::from_value(json.into()).map_err(|e| e.to_string())?;

    let report = probe::probe(input)?;
    let output = remux::output_path(std::path::Path::new(input));
    let status = tools::command(tools::ffmpeg()?)
        .args(remux::build_args(&report, &output, opts))
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("ffmpeg failed".into());
    }
    remux::post_process(&output, opts)?;
    println!("{}", output.display());
    Ok(())
}
