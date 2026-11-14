import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface GpuInfo {
  name: string | null;
  driver: string | null;
  memoryMb: number | null;
  isRtx: boolean;
  nvenc: string[];
  note: string | null;
}

export interface VideoInfo {
  codec: string;
  profile: string | null;
  width: number;
  height: number;
  fps: number;
  isVfr: boolean;
  pixFmt: string;
  bitDepth: number;
  bitrate: number | null;
  colorPrimaries: string | null;
  colorTransfer: string | null;
  colorSpace: string | null;
  colorRange: string | null;
}

export interface AudioInfo {
  codec: string;
  sampleRate: number | null;
  channels: number | null;
  bitrate: number | null;
}

export type NoteLevel = "ok" | "info" | "warn" | "error";

export interface Note {
  level: NoteLevel;
  code: string;
  message: string;
}

export interface ProbeReport {
  path: string;
  fileName: string;
  sizeBytes: number;
  durationS: number;
  container: string;
  overallBitrate: number | null;
  video: VideoInfo | null;
  audio: AudioInfo | null;
  extraStreams: number;
  moovFirst: boolean | null;
  notes: Note[];
  remuxOk: boolean;
}

export interface JobProgress {
  jobId: number;
  percent: number;
  speed: number | null;
}
export interface JobDone {
  jobId: number;
  output: string;
  sizeBytes: number;
}
export interface JobFailed {
  jobId: number;
  message: string;
  cancelled: boolean;
}

export const gpuInfo = () => invoke<GpuInfo>("gpu_info");
export const probeFile = (path: string) => invoke<ProbeReport>("probe_file", { path });
export type RatePatch = "none" | "mvhd" | "elst";
export interface RemuxOptions {
  /** Experimental: 1 = off; N stretches video timestamps N×. */
  retimeFactor: number;
  /** Experimental: playback-rate hint written into the header after a retime. */
  ratePatch: RatePatch;
}
export const startRemux = (path: string, options: RemuxOptions) =>
  invoke<{ jobId: number; output: string }>("start_remux", { path, options });
export const cancelJob = (jobId: number) => invoke<boolean>("cancel_job", { jobId });

export function onJobEvents(handlers: {
  progress: (p: JobProgress) => void;
  done: (d: JobDone) => void;
  failed: (f: JobFailed) => void;
}): () => void {
  const subs: Promise<UnlistenFn>[] = [
    listen<JobProgress>("job-progress", (e) => handlers.progress(e.payload)),
    listen<JobDone>("job-done", (e) => handlers.done(e.payload)),
    listen<JobFailed>("job-failed", (e) => handlers.failed(e.payload)),
  ];
  return () => subs.forEach((s) => s.then((un) => un()));
}

// ---------- formatting ----------

export function fmtBytes(n: number): string {
  if (n >= 1024 ** 3) return `${(n / 1024 ** 3).toFixed(2)} ГБ`;
  if (n >= 1024 ** 2) return `${(n / 1024 ** 2).toFixed(1)} МБ`;
  if (n >= 1024) return `${Math.round(n / 1024)} КБ`;
  return `${n} Б`;
}

export function fmtBitrate(bps: number | null | undefined): string {
  if (!bps) return "—";
  return bps >= 1e6 ? `${(bps / 1e6).toFixed(1)} Мбит/с` : `${Math.round(bps / 1e3)} кбит/с`;
}

export function fmtDuration(s: number): string {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = (s % 60).toFixed(1).padStart(4, "0");
  return `${String(h).padStart(2, "0")}:${String(m).padStart(2, "0")}:${sec}`;
}

const CODECS: Record<string, string> = { h264: "H.264", hevc: "HEVC", prores: "ProRes", av1: "AV1", vp9: "VP9", aac: "AAC", opus: "Opus", mp3: "MP3" };
export const codecLabel = (c: string) => CODECS[c] ?? c.toUpperCase();
