pub mod boxpatch;
pub mod gpu;
pub mod probe;
pub mod remux;
pub mod tools;

use std::sync::atomic::{AtomicU64, Ordering};

use tauri::{AppHandle, State};

static NEXT_JOB: AtomicU64 = AtomicU64::new(1);

#[tauri::command]
async fn gpu_info() -> gpu::GpuInfo {
    tauri::async_runtime::spawn_blocking(gpu::detect)
        .await
        .unwrap_or_default()
}

#[tauri::command]
async fn probe_file(path: String) -> Result<probe::ProbeReport, String> {
    tauri::async_runtime::spawn_blocking(move || probe::probe(&path))
        .await
        .map_err(|e| e.to_string())?
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Started {
    job_id: u64,
    output: String,
}

#[tauri::command]
async fn start_remux(
    app: AppHandle,
    jobs: State<'_, remux::Jobs>,
    path: String,
    options: Option<remux::Options>,
) -> Result<Started, String> {
    let report = tauri::async_runtime::spawn_blocking(move || probe::probe(&path))
        .await
        .map_err(|e| e.to_string())??;
    if report.video.is_none() {
        return Err("В файле нет видеодорожки.".into());
    }
    let job_id = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
    let output = remux::start(app, jobs.inner().clone(), job_id, report, options.unwrap_or_default())?;
    Ok(Started { job_id, output })
}

#[tauri::command]
fn cancel_job(jobs: State<'_, remux::Jobs>, job_id: u64) -> bool {
    jobs.cancel(job_id)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(remux::Jobs::default())
        .invoke_handler(tauri::generate_handler![gpu_info, probe_file, start_remux, cancel_job])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
