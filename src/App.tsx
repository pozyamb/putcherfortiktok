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
  rifeAvailable,
  onJobEvents,
  probeFile,
  startRemux,
  type EnhanceOptions,
  type GpuInfo,
  type ProbeReport,
  type RatePatch,
} from "./api";
import {
  BUILTIN_PRESETS,
  DEFAULT_SETTINGS,
  loadUserPresets,
  sameSettings,
  saveUserPresets,
  toRemuxOptions,
  type Method,
  type Preset,
  type Settings,
} from "./presets";
import "./App.css";

const LEVELS = ["выкл", "слабо", "средне", "сильно"];
const VIDEO_EXT = ["mp4", "mov", "m4v", "mkv", "webm", "avi"];

/** Mirrors `enhance::target_size` in Rust: short side → target, aspect kept, even numbers. */
function targetSize(w: number, h: number, shortSide: number): [number, number] {
  const even = (v: number) => Math.max(1, Math.round(v / 2)) * 2;
  if (!shortSide || !w || !h) return [even(w), even(h)];
  const k = shortSide / Math.min(w, h);
  return [even(w * k), even(h * k)];
}

type Status = "probing" | "ready" | "running" | "done" | "failed" | "cancelled";

interface Item {
  id: number;
  path: string;
  name: string;
  status: Status;
  report: ProbeReport | null;
  jobId: number | null;
  percent: number;
  speed: number | null;
  stage: string;
  output: string | null;
  sizeBytes: number;
  outReport: ProbeReport | null;
  message: string | null;
}

const STATUS_LABEL: Record<Status, string> = {
  probing: "анализ…",
  ready: "ждёт",
  running: "",
  done: "готово",
  failed: "ошибка",
  cancelled: "отменён",
};

let nextItemId = 1;
const baseName = (p: string) => p.split(/[\\/]/).pop() ?? p;

export default function App() {
  const [gpu, setGpu] = useState<GpuInfo | null>(null);
  const [rife, setRife] = useState(false);
  const [items, setItems] = useState<Item[]>([]);
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [dragOver, setDragOver] = useState(false);
  const [queueOn, setQueueOn] = useState(false);
  const [settings, setSettings] = useState<Settings>(DEFAULT_SETTINGS);
  const [userPresets, setUserPresets] = useState<Preset[]>(loadUserPresets);
  const [presetName, setPresetName] = useState<string | null>(null);

  // The queue runner and job events read the latest state through refs.
  const itemsRef = useRef(items);
  itemsRef.current = items;
  const settingsRef = useRef(settings);
  settingsRef.current = settings;
  const queueOnRef = useRef(false);

  const patchItem = useCallback((id: number, patch: Partial<Item>) => {
    itemsRef.current = itemsRef.current.map((it) => (it.id === id ? { ...it, ...patch } : it));
    setItems((prev) => prev.map((it) => (it.id === id ? { ...it, ...patch } : it)));
  }, []);

  const { method, retime, ratePatch, enh } = settings;
  const setMethod = (v: Method) => setSettings((p) => ({ ...p, method: v }));
  const setRetime = (v: number) => setSettings((p) => ({ ...p, retime: v }));
  const setRatePatch = (v: RatePatch) => setSettings((p) => ({ ...p, ratePatch: v }));
  const setEnh = (fn: (prev: EnhanceOptions) => EnhanceOptions) => setSettings((p) => ({ ...p, enh: fn(p.enh) }));
  const setEnhField = <K extends keyof EnhanceOptions>(key: K, value: EnhanceOptions[K]) =>
    setEnh((prev) => ({ ...prev, [key]: value }));

  useEffect(() => {
    rifeAvailable().then(setRife).catch(() => setRife(false));
    gpuInfo().then(setGpu).catch(() => setGpu({ name: null, driver: null, memoryMb: null, isRtx: false, nvenc: [], note: "Не удалось определить видеокарту." }));
  }, []);

  const addFiles = useCallback((paths: string[]) => {
    const videos = paths.filter((f) => VIDEO_EXT.includes(f.split(".").pop()?.toLowerCase() ?? ""));
    if (videos.length === 0) {
      setError("Это не видеофайлы. Подходят: " + VIDEO_EXT.join(", ").toUpperCase());
      return;
    }
    setError(null);
    const known = new Set(itemsRef.current.filter((it) => it.status !== "done").map((it) => it.path));
    const fresh: Item[] = videos
      .filter((p) => !known.has(p))
      .map((path) => ({
        id: nextItemId++, path, name: baseName(path), status: "probing", report: null, jobId: null,
        percent: 0, speed: null, stage: "", output: null, sizeBytes: 0, outReport: null, message: null,
      }));
    if (fresh.length === 0) return;
    itemsRef.current = [...itemsRef.current, ...fresh];
    setItems((prev) => [...prev, ...fresh]);
    setSelectedId(fresh[0].id);
    for (const it of fresh) {
      probeFile(it.path)
        .then((report) =>
          patchItem(it.id, report.video
            ? { report, status: "ready" }
            : { report, status: "failed", message: "В файле нет видеодорожки." }))
        .catch((e) => patchItem(it.id, { status: "failed", message: String(e) }));
    }
  }, [patchItem]);

  // Drag-and-drop from Explorer arrives as a webview event with real file paths.
  useEffect(() => {
    const un = getCurrentWebview().onDragDropEvent((e) => {
      const p = e.payload;
      if (p.type === "enter" || p.type === "over") setDragOver(true);
      else if (p.type === "leave") setDragOver(false);
      else if (p.type === "drop") {
        setDragOver(false);
        addFiles(p.paths);
      }
    });
    return () => {
      un.then((f) => f());
    };
  }, [addFiles]);

  /** Starts the next waiting file, or ends the queue when none is left. */
  const startNext = useCallback(async () => {
    if (!queueOnRef.current) return;
    if (itemsRef.current.some((it) => it.status === "running")) return;
    const next = itemsRef.current.find((it) => it.status === "ready");
    if (!next) {
      queueOnRef.current = false;
      setQueueOn(false);
      return;
    }
    patchItem(next.id, { status: "running", percent: 0, speed: null, stage: "Запуск…", message: null });
    setSelectedId(next.id);
    try {
      const s = settingsRef.current;
      const { jobId } = await startRemux(next.path, toRemuxOptions(s), s.enh);
      patchItem(next.id, { jobId });
    } catch (e) {
      patchItem(next.id, { status: "failed", message: String(e) });
      startNext();
    }
  }, [patchItem]);

  useEffect(
    () =>
      onJobEvents({
        progress: (p) => {
          const it = itemsRef.current.find((x) => x.jobId === p.jobId);
          if (it) patchItem(it.id, { percent: p.percent, speed: p.speed, stage: p.stage });
        },
        done: (d) => {
          const it = itemsRef.current.find((x) => x.jobId === d.jobId);
          if (!it) return;
          patchItem(it.id, { status: "done", jobId: null, percent: 100, output: d.output, sizeBytes: d.sizeBytes });
          probeFile(d.output).then((outReport) => patchItem(it.id, { outReport })).catch(() => {});
          startNext();
        },
        failed: (f) => {
          const it = itemsRef.current.find((x) => x.jobId === f.jobId);
          if (!it) return;
          patchItem(it.id, { status: f.cancelled ? "cancelled" : "failed", jobId: null, message: f.message });
          startNext();
        },
      }),
    [patchItem, startNext],
  );

  async function pickFiles() {
    const sel = await open({ multiple: true, directory: false, filters: [{ name: "Видео", extensions: VIDEO_EXT }] });
    if (Array.isArray(sel)) addFiles(sel);
    else if (typeof sel === "string") addFiles([sel]);
  }

  function runQueue() {
    // Files stopped earlier go back into the queue.
    for (const it of itemsRef.current) {
      if (it.status === "cancelled") patchItem(it.id, { status: "ready", message: null });
    }
    queueOnRef.current = true;
    setQueueOn(true);
    startNext();
  }

  function stopQueue() {
    queueOnRef.current = false;
    setQueueOn(false);
    const cur = itemsRef.current.find((it) => it.status === "running");
    if (cur?.jobId != null) cancelJob(cur.jobId);
  }

  function removeItem(id: number) {
    itemsRef.current = itemsRef.current.filter((it) => it.id !== id || it.status === "running");
    setItems((prev) => prev.filter((it) => it.id !== id || it.status === "running"));
    if (selectedId === id) setSelectedId(null);
  }

  function clearDone() {
    itemsRef.current = itemsRef.current.filter((it) => it.status !== "done");
    setItems((prev) => prev.filter((it) => it.status !== "done"));
  }

  // ---------- presets ----------
  const allPresets = [...BUILTIN_PRESETS, ...userPresets];
  const activePreset = allPresets.find((p) => sameSettings(p.settings, settings));

  function savePreset() {
    const name = (presetName ?? "").trim();
    if (!name) return;
    if (BUILTIN_PRESETS.some((p) => p.name === name)) {
      setError(`«${name}» — встроенный пресет, выберите другое название.`);
      return;
    }
    const list = [...userPresets.filter((p) => p.name !== name), { name, settings }];
    setUserPresets(list);
    if (!saveUserPresets(list)) setError("Не удалось сохранить пресет на диск: он пропадёт после закрытия приложения.");
    else setError(null);
    setPresetName(null);
  }

  function deletePreset(name: string) {
    const list = userPresets.filter((p) => p.name !== name);
    setUserPresets(list);
    saveUserPresets(list);
  }

  const running = queueOn || items.some((it) => it.status === "running");
  const canEncode = !!gpu && gpu.nvenc.includes("h264");
  const selected = items.find((it) => it.id === selectedId) ?? items[items.length - 1] ?? null;
  const report = selected?.report ?? null;
  const waiting = items.filter((it) => it.status === "ready" || it.status === "cancelled").length;
  const finished = items.filter((it) => it.status === "done").length;
  const outSize = report?.video ? targetSize(report.video.width, report.video.height, enh.shortSide) : null;
  // PNG frames take roughly 0.6 bytes per pixel: source frames plus generated ones.
  const rifeDiskGb = report?.video && report.video.fps > 0
    ? Math.max(1, Math.ceil((report.video.width * report.video.height * 0.65 * report.durationS * (report.video.fps + Math.max(report.video.fps, enh.fpsTarget))) / 1e9))
    : 0;
  const outFps = report?.video ? (enh.fps === "keep" ? report.video.fps : Math.max(report.video.fps, enh.fpsTarget)) : 0;

  return (
    <div className="shell">
      <header className="titlebar">
        <span className="brand">putcherfortiktok</span>
        <span className="chip">v0.1.0</span>
        <span className="sp" />
        <GpuBadge gpu={gpu} />
      </header>

      <div className="app">
        <nav className="side" aria-label="Очередь файлов">
          <div className="side-h">
            <h4>Очередь</h4>
            {finished > 0 && !running && <button className="link" onClick={clearDone}>Убрать готовые</button>}
          </div>
          {items.length === 0 && <p className="hint">Файлов пока нет. Можно добавить сразу несколько.</p>}
          <ul className="queue">
            {items.map((it) => (
              <li key={it.id} className={"q " + it.status + (selected?.id === it.id ? " on" : "")}>
                <button className="q-main" onClick={() => setSelectedId(it.id)} title={it.path}>
                  <span className="q-name">{it.name}</span>
                  <span className="q-state">{it.status === "running" ? `${it.percent.toFixed(0)}%` : STATUS_LABEL[it.status]}</span>
                </button>
                {it.status !== "running" && (
                  <button className="q-x" onClick={() => removeItem(it.id)} aria-label={`Убрать ${it.name} из очереди`}>×</button>
                )}
                {it.status === "running" && <span className="q-bar" style={{ width: `${it.percent}%` }} />}
              </li>
            ))}
          </ul>
        </nav>

        <main className="main">
          <button className={"drop" + (dragOver ? " over" : "")} onClick={pickFiles}>
            <span className="drop-icon" aria-hidden>⇪</span>
            <span>
              <b>Перетащите видео сюда</b> или нажмите, чтобы выбрать · можно несколько · MP4, MOV, MKV, WEBM
            </span>
          </button>

          {error && <div className="alert">{error}</div>}

          {selected?.status === "probing" && <div className="muted">Анализирую файл…</div>}
          {report && <FileCard report={report} />}

          {!selected && !error && (
            <div className="empty">
              <p>Выберите ролики, и приложение покажет, что в них мешает TikTok: кодек, битовую глубину, цветовые теги, положение moov-атома, частоту кадров.</p>
              <p className="muted">Всё работает локально, файлы никуда не отправляются.</p>
            </div>
          )}

          {selected?.status === "running" && (
            <div className="prog">
              <div className="track">
                <div className="fill" style={{ width: `${selected.percent}%` }} />
              </div>
              <div className="row">
                <span>{selected.stage}</span>
                <span>
                  {selected.percent.toFixed(0)}%{selected.speed ? ` · ${selected.speed.toFixed(1)}×` : ""}
                </span>
              </div>
            </div>
          )}

          {selected?.status === "done" && selected.output && (
            <div className="result">
              <div className="result-h">
                <span className="chip ok">Готово</span>
                <span className="path" title={selected.output}>{selected.output}</span>
                <span className="mono muted">{fmtBytes(selected.sizeBytes)}</span>
              </div>
              {selected.outReport && (
                <ul className="notes">
                  {selected.outReport.notes.map((n) => (
                    <li key={n.code} className={"note " + n.level}>{n.message}</li>
                  ))}
                </ul>
              )}
              <button className="ghost" onClick={() => selected.output && revealItemInDir(selected.output)}>Показать в папке</button>
            </div>
          )}

          {selected && (selected.status === "failed" || selected.status === "cancelled") && selected.message && (
            <div className={selected.status === "cancelled" ? "muted" : "alert"}>{selected.message}</div>
          )}
        </main>

        <aside className="panel" aria-label="Параметры">
          <h4>Пресет</h4>
          <div className="field">
            <select
              id="preset"
              aria-label="Пресет настроек"
              value={activePreset ? activePreset.name : ""}
              onChange={(e) => {
                const p = allPresets.find((x) => x.name === e.target.value);
                if (p) setSettings(p.settings);
              }}
              disabled={running}
            >
              {!activePreset && <option value="">Свои настройки</option>}
              <optgroup label="Встроенные">
                {BUILTIN_PRESETS.map((p) => <option key={p.name} value={p.name}>{p.name}</option>)}
              </optgroup>
              {userPresets.length > 0 && (
                <optgroup label="Мои">
                  {userPresets.map((p) => <option key={p.name} value={p.name}>{p.name}</option>)}
                </optgroup>
              )}
            </select>
          </div>
          {presetName === null ? (
            <div className="row-btns">
              <button className="ghost" onClick={() => setPresetName("")} disabled={running || !!activePreset} title={activePreset ? "Эти настройки уже сохранены как пресет" : undefined}>Сохранить как пресет</button>
              {activePreset && !activePreset.builtin && (
                <button className="ghost" onClick={() => deletePreset(activePreset.name)} disabled={running}>Удалить</button>
              )}
            </div>
          ) : (
            <form className="row-btns" onSubmit={(e) => { e.preventDefault(); savePreset(); }}>
              <input id="presetName" className="text" autoFocus placeholder="Название пресета" maxLength={40} value={presetName} onChange={(e) => setPresetName(e.target.value)} />
              <button className="ghost" type="submit" disabled={!presetName.trim()}>Сохранить</button>
              <button className="ghost" type="button" onClick={() => setPresetName(null)}>Отмена</button>
            </form>
          )}

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
                <select
                  id="fps"
                  value={enh.fps === "keep" ? "keep" : String(enh.fpsTarget)}
                  onChange={(e) => {
                    const v = e.target.value;
                    setEnh((prev) => v === "keep"
                      ? { ...prev, fps: "keep" }
                      : { ...prev, fps: prev.fps === "keep" ? "blend" : prev.fps, fpsTarget: Number(v) });
                  }}
                  disabled={running}
                >
                  <option value="keep">Как в исходнике</option>
                  <option value="60">60 fps</option>
                  <option value="120">120 fps</option>
                </select>
              </div>
              {enh.fps !== "keep" && (
                <div className="field">
                  <label htmlFor="fpsMode">Как достраивать кадры</label>
                  <select id="fpsMode" value={enh.fps} onChange={(e) => setEnhField("fps", e.target.value as EnhanceOptions["fps"])} disabled={running}>
                    <option value="blend">Смешивание соседних кадров (быстро)</option>
                    <option value="rife" disabled={!rife}>Нейросеть RIFE (лучшее качество){rife ? "" : " — не установлена"}</option>
                    <option value="motion">По движению (медленно, возможны артефакты)</option>
                  </select>
                </div>
              )}
              {enh.fps === "rife" && (
                <p className="hint warn">
                  RIFE временно сохраняет каждый кадр на диск рядом с роликом
                  {rifeDiskGb ? `: для этого файла нужно около ${rifeDiskGb} ГБ свободного места` : ""}. После обработки кадры удаляются.
                </p>
              )}
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
            <button className="go cancel" onClick={stopQueue}>Остановить</button>
          ) : (
            <button className="go" onClick={runQueue} disabled={waiting === 0}>
              {waiting > 1 ? `Оптимизировать (${waiting})` : "Оптимизировать"}
            </button>
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
