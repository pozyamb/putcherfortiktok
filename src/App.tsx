import { useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import {
  cancelJob,
  codecLabel,
  fmtBitrate,
  fmtBytes,
  fmtDuration,
  gpuInfo,
  onJobEvents,
  probeFile,
  startRemux,
  DEFAULT_ENHANCE,
  type EnhanceOptions,
  type GpuInfo,
  type ProbeReport,
  type RatePatch,
} from "./api";
import "./App.css";

type Method = "tail" | "retime" | "plain";

const LEVELS = ["выкл", "слабо", "средне", "сильно"];

/** Mirrors `enhance::target_size` in Rust: short side → target, aspect kept, even numbers. */
function targetSize(w: number, h: number, shortSide: number): [number, number] {
  const even = (v: number) => Math.max(1, Math.round(v / 2)) * 2;
  if (!shortSide || !w || !h) return [even(w), even(h)];
  const k = shortSide / Math.min(w, h);
  return [even(w * k), even(h * k)];
}

const VIDEO_EXT =["mp4", "mov", "m4v", "mkv", "webm", "avi"];

type Job =
  | { state: "idle" }
  | { state: "running"; jobId: number; percent: number; speed: number | null; stage: string }
  | { state: "done"; output: string; sizeBytes: number; report: ProbeReport | null }
  | { state: "failed"; message: string; cancelled: boolean };

const NAV = [
  { id: "optimize", label: "Оптимизатор", ready: true },
  { id: "queue", label: "Очередь", ready: false },
  { id: "presets", label: "Пресеты", ready: false },
  { id: "history", label: "История", ready: false },
  { id: "settings", label: "Настройки", ready: false },
];

export default function App() {
  const [gpu, setGpu] = useState<GpuInfo | null>(null);
  const [report, setReport] = useState<ProbeReport | null>(null);
  const [probing, setProbing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [dragOver, setDragOver] = useState(false);
  const [job, setJob] = useState<Job>({ state: "idle" });
  const jobIdRef = useRef<number | null>(null);
  const [method, setMethod] = useState<Method>("tail");
  const [retime, setRetime] = useState(2);
  const [ratePatch, setRatePatch] = useState<RatePatch>("mvhd");
  const [enh, setEnh] = useState<EnhanceOptions>(DEFAULT_ENHANCE);
  const setEnhField = <K extends keyof EnhanceOptions>(key: K, value: EnhanceOptions[K]) =>
    setEnh((prev) => ({ ...prev, [key]: value }));

  useEffect(() => {
    gpuInfo().then(setGpu).catch(() => setGpu({ name: null, driver: null, memoryMb: null, isRtx: false, nvenc: [], note: "Не удалось определить видеокарту." }));
  }, []);

  const analyze = useCallback(async (path: string) => {
    setError(null);
    setJob({ state: "idle" });
    setProbing(true);
    try {
      setReport(await probeFile(path));
    } catch (e) {
      setReport(null);
      setError(String(e));
    } finally {
      setProbing(false);
    }
  }, []);

  // Drag-and-drop from Explorer arrives as a webview event with real file paths.
  useEffect(() => {
    const un = getCurrentWebview().onDragDropEvent((e) => {
      const p = e.payload;
      if (p.type === "enter" || p.type === "over") setDragOver(true);
      else if (p.type === "leave") setDragOver(false);
      else if (p.type === "drop") {
        setDragOver(false);
        const file = p.paths.find((f) => VIDEO_EXT.includes(f.split(".").pop()?.toLowerCase() ?? ""));
        if (file) analyze(file);
        else setError("Перетащите видеофайл: " + VIDEO_EXT.join(", ").toUpperCase());
      }
    });
    return () => {
      un.then((f) => f());
    };
  }, [analyze]);

  useEffect(
    () =>
      onJobEvents({
        progress: (p) => {
          if (p.jobId === jobIdRef.current) setJob({ state: "running", jobId: p.jobId, percent: p.percent, speed: p.speed, stage: p.stage });
        },
        done: async (d) => {
          if (d.jobId !== jobIdRef.current) return;
          jobIdRef.current = null;
          setJob({ state: "done", output: d.output, sizeBytes: d.sizeBytes, report: null });
          try {
            const r = await probeFile(d.output);
            setJob({ state: "done", output: d.output, sizeBytes: d.sizeBytes, report: r });
          } catch {
            /* the result is still usable without the second analysis */
          }
        },
        failed: (f) => {
          if (f.jobId !== jobIdRef.current) return;
          jobIdRef.current = null;
          setJob({ state: "failed", message: f.message, cancelled: f.cancelled });
        },
      }),
    [],
  );

  async function pickFile() {
    const sel = await open({ multiple: false, directory: false, filters: [{ name: "Видео", extensions: VIDEO_EXT }] });
    if (typeof sel === "string") analyze(sel);
  }

  async function run() {
    if (!report) return;
    setError(null);
    try {
      const { jobId } = await startRemux(report.path, {
        retimeFactor: method === "retime" ? retime : 1,
        ratePatch: method === "retime" ? ratePatch : "none",
        audioTail: method === "tail",
      }, enh);
      jobIdRef.current = jobId;
      setJob({ state: "running", jobId, percent: 0, speed: null, stage: "Запуск…" });
    } catch (e) {
      setJob({ state: "failed", message: String(e), cancelled: false });
    }
  }

  const running = job.state === "running";
  const canEncode = !!gpu && gpu.nvenc.includes("h264");
  const outSize = report?.video ? targetSize(report.video.width, report.video.height, enh.shortSide) : null;
  const outFps = report?.video ? (enh.fps === "keep" ? report.video.fps : Math.max(report.video.fps, 60)) : 0;

  return (
    <div className="shell">
      <header className="titlebar">
        <span className="brand">putcherfortiktok</span>
        <span className="chip">v0.1.0</span>
        <span className="sp" />
        <GpuBadge gpu={gpu} />
      </header>

      <div className="app">
        <nav className="side" aria-label="Разделы">
          {NAV.map((n) => (
            <button key={n.id} className={"nav" + (n.id === "optimize" ? " on" : "")} disabled={!n.ready} title={n.ready ? undefined : "Будет в следующих версиях"}>
              {n.label}
              {!n.ready && <span className="soon">скоро</span>}
            </button>
          ))}
        </nav>

        <main className="main">
          <button className={"drop" + (dragOver ? " over" : "")} onClick={pickFile} disabled={running}>
            <span className="drop-icon" aria-hidden>⇪</span>
            <span>
              <b>Перетащите видео сюда</b> или нажмите, чтобы выбрать · MP4, MOV, MKV, WEBM
            </span>
          </button>

          {error && <div className="alert">{error}</div>}
          {probing && <div className="muted">Анализирую файл…</div>}

          {report && <FileCard report={report} />}

          {!report && !probing && !error && (
            <div className="empty">
              <p>Выберите ролик, и приложение покажет, что в нём мешает TikTok: кодек, битовую глубину, цветовые теги, положение moov-атома, частоту кадров.</p>
              <p className="muted">Всё работает локально, файлы никуда не отправляются.</p>
            </div>
          )}

          {job.state === "running" && (
            <div className="prog">
              <div className="track">
                <div className="fill" style={{ width: `${job.percent}%` }} />
              </div>
              <div className="row">
                <span>{job.stage}</span>
                <span>
                  {job.percent.toFixed(0)}%{job.speed ? ` · ${job.speed.toFixed(1)}×` : ""}
                </span>
              </div>
            </div>
          )}

          {job.state === "done" && (
            <div className="result">
              <div className="result-h">
                <span className="chip ok">Готово</span>
                <span className="path" title={job.output}>{job.output}</span>
                <span className="mono muted">{fmtBytes(job.sizeBytes)}</span>
              </div>
              {job.report && (
                <ul className="notes">
                  {job.report.notes.map((n) => (
                    <li key={n.code} className={"note " + n.level}>{n.message}</li>
                  ))}
                </ul>
              )}
              <button className="ghost" onClick={() => revealItemInDir(job.output)}>Показать в папке</button>
            </div>
          )}

          {job.state === "failed" && <div className={job.cancelled ? "muted" : "alert"}>{job.message}</div>}
        </main>

        <aside className="panel" aria-label="Параметры">
          <h4>Шаг 1 · Видео</h4>
          <div className="seg" role="tablist">
            <button role="tab" aria-selected={!enh.enabled} className={enh.enabled ? "" : "on"} onClick={() => setEnhField("enabled", false)} disabled={running}>Без изменений</button>
            <button role="tab" aria-selected={enh.enabled} className={enh.enabled ? "on" : ""} onClick={() => setEnhField("enabled", true)} disabled={running || !canEncode} title={canEncode ? undefined : "NVENC недоступен на этом компьютере"}>Улучшение</button>
          </div>
          {!enh.enabled && <p className="hint">Кадры не перекодируются, качество не теряется, занимает секунды.</p>}
          {enh.enabled && (
            <>
              <p className="hint">
                Видео обрабатывается на видеокарте и кодируется заново через NVENC.
                {report?.video && outSize ? ` Итог: ${outSize[0]}×${outSize[1]}, ${fmtFps(outFps)} fps.` : ""}
              </p>
              <div className="field">
                <label htmlFor="shortSide">Разрешение</label>
                <select id="shortSide" value={enh.shortSide} onChange={(e) => setEnhField("shortSide", Number(e.target.value))} disabled={running}>
                  <option value={0}>Как в исходнике</option>
                  <option value={1080}>1080p</option>
                  <option value={1440}>1440p</option>
                  <option value={2160}>4K</option>
                </select>
              </div>
              <div className="field">
                <label htmlFor="upscaler">Алгоритм масштабирования</label>
                <select id="upscaler" value={enh.upscaler} onChange={(e) => setEnhField("upscaler", e.target.value as EnhanceOptions["upscaler"])} disabled={running}>
                  <option value="sharp">Резкий (для чистых исходников)</option>
                  <option value="smooth">Мягкий (прячет артефакты сжатия)</option>
                  <option value="fast">Быстрый</option>
                </select>
              </div>
              <div className="field">
                <label htmlFor="fps">Частота кадров</label>
                <select id="fps" value={enh.fps} onChange={(e) => setEnhField("fps", e.target.value as EnhanceOptions["fps"])} disabled={running}>
                  <option value="keep">Как в исходнике</option>
                  <option value="blend">60 fps, смешивание кадров (быстро)</option>
                  <option value="motion">60 fps, по движению (медленно)</option>
                </select>
              </div>
              <div className="field">
                <label htmlFor="denoise">Шумоподавление: {LEVELS[enh.denoise]}</label>
                <input id="denoise" type="range" min={0} max={3} step={1} value={enh.denoise} onChange={(e) => setEnhField("denoise", Number(e.target.value))} disabled={running} />
              </div>
              <div className="field">
                <label htmlFor="sharpen">Резкость: {LEVELS[enh.sharpen]}</label>
                <input id="sharpen" type="range" min={0} max={3} step={1} value={enh.sharpen} onChange={(e) => setEnhField("sharpen", Number(e.target.value))} disabled={running} />
              </div>
              <label className="check">
                <input id="deband" type="checkbox" checked={enh.deband} onChange={(e) => setEnhField("deband", e.target.checked)} disabled={running} />
                Убирать полосы на градиентах
              </label>
              <div className="field">
                <label htmlFor="codec">Кодек</label>
                <select id="codec" value={enh.codec} onChange={(e) => setEnhField("codec", e.target.value as EnhanceOptions["codec"])} disabled={running}>
                  <option value="h264">H.264 (рекомендуется)</option>
                  <option value="hevc" disabled={gpu ? !gpu.nvenc.includes("hevc") : false}>HEVC</option>
                </select>
              </div>
              <div className="field">
                <label htmlFor="bitrate">Битрейт: <span className="mono">{enh.bitrateMbps}</span> Мбит/с</label>
                <input id="bitrate" type="range" min={8} max={60} step={1} value={enh.bitrateMbps} onChange={(e) => setEnhField("bitrateMbps", Number(e.target.value))} disabled={running} />
              </div>
            </>
          )}

          <h4>Шаг 2 · Патч</h4>
          <div className="field">
            <select id="method" aria-label="Способ обработки" value={method} onChange={(e) => setMethod(e.target.value as Method)} disabled={running}>
              <option value="tail">Патч хвоста аудио (как на сайте сейчас)</option>
              <option value="retime">Ретайминг (прежний способ)</option>
              <option value="plain">Только ремукс</option>
            </select>
          </div>

          <h4>Что будет сделано</h4>
          <ul className="checks">
            <li>moov-атом в начало файла (faststart)</li>
            {method === "tail" ? (
              <>
                <li>Видео и метаданные дорожек без изменений</li>
                <li>Последние 4 аудиокадра заменяются служебной вставкой: последние ~0,1 с звука теряются</li>
              </>
            ) : (
              <>
                <li>Цветовые теги BT.709, если их нет</li>
                <li>Удаление метаданных, глав и лишних дорожек</li>
              </>
            )}
            <li>Результат: <span className="mono">имя_tiktok.mp4</span> рядом с исходником</li>
          </ul>

          {method === "tail" && (
            <p className="hint">
              Повторяет файл, который сайт-пример выдаёт сейчас: проверено побайтным сравнением на одном ролике.
              {report && !report.audio ? " В этом файле нет звука, способ не сработает." : ""}
            </p>
          )}

          {method === "retime" && (
            <>
              <div className="field">
                <label htmlFor="retime">Заявленная частота кадров</label>
                <select id="retime" value={retime} onChange={(e) => setRetime(Number(e.target.value))} disabled={running}>
                  <option value={2}>÷2{report?.video ? ` (${fmtFps(report.video.fps / 2)} fps)` : ""}</option>
                  <option value={4}>÷4{report?.video ? ` (${fmtFps(report.video.fps / 4)} fps)` : ""}</option>
                </select>
              </div>
              <div className="field">
                <label htmlFor="ratePatch">Метка скорости в заголовке</label>
                <select id="ratePatch" value={ratePatch} onChange={(e) => setRatePatch(e.target.value as RatePatch)} disabled={running}>
                  <option value="mvhd">mvhd rate ×{retime}</option>
                  <option value="elst">elst media rate ×{retime}</option>
                  <option value="none">Без метки</option>
                </select>
              </div>
              <p className="hint warn">
                Кадры те же, но файл объявляет частоту в {retime} раза ниже и на компьютере играет замедленно. Сработает ли это в TikTok, не проверено: сначала загрузите тестовый ролик.
              </p>
            </>
          )}

          {report && !report.remuxOk && (
            !enh.enabled && <p className="hint warn">Этому файлу нужно перекодирование: включите «Улучшение» в шаге 1.</p>
          )}

          <span className="sp" />
          {running ? (
            <button className="go cancel" onClick={() => job.state === "running" && cancelJob(job.jobId)}>Отменить</button>
          ) : (
            <button className="go" onClick={run} disabled={!report || !report.video}>Оптимизировать</button>
          )}
        </aside>
      </div>
    </div>
  );
}

const fmtFps = (f: number) => f.toFixed(2).replace(/\.?0+$/, "");

function GpuBadge({ gpu }: { gpu: GpuInfo | null }) {
  if (!gpu) return <span className="gpu muted">Определяю видеокарту…</span>;
  const ok = gpu.name && gpu.nvenc.length > 0;
  return (
    <span className="gpu" title={gpu.note ?? undefined}>
      <span className={"dot" + (ok ? "" : " warn")} />
      {gpu.name ? gpu.name.replace(/^NVIDIA\s+/, "") : "Нет GPU NVIDIA"}
      {gpu.nvenc.length > 0 && <> · NVENC {gpu.nvenc.map((c) => codecLabel(c)).join(" / ")}</>}
      {gpu.driver && <> · драйвер {gpu.driver}</>}
      {!ok && gpu.note && <> · {gpu.note}</>}
    </span>
  );
}

function FileCard({ report: r }: { report: ProbeReport }) {
  const v = r.video;
  const a = r.audio;
  const specs: [string, string][] = [
    ["Разрешение", v ? `${v.width}×${v.height}` : "—"],
    ["Кадры", v ? `${v.fps.toFixed(2).replace(/\.00$/, "")} fps${v.isVfr ? " VFR" : ""}` : "—"],
    ["Кодек", v ? `${codecLabel(v.codec)}${v.profile ? " " + v.profile : ""}` : "—"],
    ["Битрейт", fmtBitrate(v?.bitrate ?? r.overallBitrate)],
    ["Длительность", fmtDuration(r.durationS)],
    ["Размер", fmtBytes(r.sizeBytes)],
    ["Аудио", a ? `${codecLabel(a.codec)}${a.sampleRate ? ` ${a.sampleRate / 1000} кГц` : ""}` : "нет"],
    ["Цвет", v ? `${v.pixFmt}${v.colorPrimaries ? " · " + v.colorPrimaries : ""}` : "—"],
  ];
  return (
    <section className="file">
      <div className="file-h">
        <strong title={r.path}>{r.fileName}</strong>
        <span className={"chip " + (r.remuxOk ? "ok" : "warn")}>{r.remuxOk ? "Ремукса достаточно" : "Нужно перекодирование"}</span>
      </div>
      <div className="specs">
        {specs.map(([k, val]) => (
          <div className="spec" key={k}>
            <div className="k">{k}</div>
            <div className="v">{val}</div>
          </div>
        ))}
      </div>
      <ul className="notes">
        {r.notes.map((n) => (
          <li key={n.code} className={"note " + n.level}>{n.message}</li>
        ))}
      </ul>
    </section>
  );
}
