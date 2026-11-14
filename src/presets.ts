import { DEFAULT_ENHANCE, type EnhanceOptions, type RatePatch, type RemuxOptions } from "./api";

export type Method = "tail" | "retime" | "plain";

/** Everything the right-hand panel controls. */
export interface Settings {
  method: Method;
  retime: number;
  ratePatch: RatePatch;
  enh: EnhanceOptions;
}

export interface Preset {
  name: string;
  settings: Settings;
  builtin?: boolean;
}

export const DEFAULT_SETTINGS: Settings = { method: "tail", retime: 2, ratePatch: "mvhd", enh: DEFAULT_ENHANCE };

const enhanced = (patch: Partial<EnhanceOptions>): Settings => ({
  ...DEFAULT_SETTINGS,
  enh: { ...DEFAULT_ENHANCE, enabled: true, ...patch },
});

export const BUILTIN_PRESETS: Preset[] = [
  { name: "Только патч", settings: DEFAULT_SETTINGS, builtin: true },
  { name: "1080p 60 fps + патч", settings: enhanced({ shortSide: 1080, fps: "blend", fpsTarget: 60, bitrateMbps: 20 }), builtin: true },
  { name: "1080p 120 fps + патч", settings: enhanced({ shortSide: 1080, fps: "blend", fpsTarget: 120, bitrateMbps: 30 }), builtin: true },
  { name: "4K 60 fps + патч", settings: enhanced({ shortSide: 2160, sharpen: 1, fps: "blend", fpsTarget: 60, bitrateMbps: 35 }), builtin: true },
  { name: "4K 120 fps + патч", settings: enhanced({ shortSide: 2160, sharpen: 1, fps: "blend", fpsTarget: 120, bitrateMbps: 50 }), builtin: true },
];

const KEY = "presets.v1";

/** User presets live in the webview's local storage; the app works without it. */
export function loadUserPresets(): Preset[] {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return [];
    const list = JSON.parse(raw);
    if (!Array.isArray(list)) return [];
    return list
      .filter((p) => p && typeof p.name === "string" && p.settings && p.settings.enh)
      .map((p) => ({
        name: p.name,
        // Fill in fields added after the preset was saved.
        settings: { ...DEFAULT_SETTINGS, ...p.settings, enh: { ...DEFAULT_ENHANCE, ...p.settings.enh } },
      }));
  } catch {
    return [];
  }
}

export function saveUserPresets(list: Preset[]): boolean {
  try {
    localStorage.setItem(KEY, JSON.stringify(list.map(({ name, settings }) => ({ name, settings }))));
    return true;
  } catch {
    return false;
  }
}

export const sameSettings = (a: Settings, b: Settings) => JSON.stringify(a) === JSON.stringify(b);

export function toRemuxOptions(s: Settings): RemuxOptions {
  return {
    retimeFactor: s.method === "retime" ? s.retime : 1,
    ratePatch: s.method === "retime" ? s.ratePatch : "none",
    audioTail: s.method === "tail",
  };
}
