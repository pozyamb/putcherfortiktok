# putcherfortiktok

Локальное десктоп-приложение, которое готовит видео к загрузке в TikTok так, чтобы платформа пережимала его как можно мягче. Обработка идёт на видеокартах NVIDIA RTX через NVENC, без облака.

Подробный план и макет интерфейса: [docs/plan.html](docs/plan.html).

## Режимы

- **Ремукс** — без перекодирования: moov в начало файла, цветовые теги BT.709, очистка метаданных.
- **NVENC** — перекодирование на GPU (`h264_nvenc` / `hevc_nvenc`) в рекомендованные параметры: 1080×1920 или 4K, 60 fps, 10–40 Мбит/с, 8-bit, AAC 48 кГц.

## Стек

Tauri 2 (Rust) + React + TypeScript + Vite. ffmpeg/ffprobe с NVENC подключаются как sidecar.

## Требования для разработки (Windows)

- Node.js LTS
- Rust (rustup, toolchain `stable-x86_64-pc-windows-msvc`)
- Компилятор C++, на выбор:
  - Visual Studio Build Tools 2022+ с компонентом «Разработка классических приложений на C++» (toolchain `stable-x86_64-pc-windows-msvc`), или
  - MinGW-w64 (`winget install BrechtSanders.WinLibs.POSIX.MSVCRT`) и toolchain `stable-x86_64-pc-windows-gnu` (`rustup override set stable-x86_64-pc-windows-gnu` в `src-tauri`). В этом варианте путь к проекту должен быть только из латинских символов: инструменты MinGW не открывают файлы по кириллическим путям.
- WebView2 (в Windows 11 уже есть)
- GeForce RTX 20xx или новее, драйвер 551+

## Запуск

```bash
npm install
npm run tauri dev
```

Только фронтенд, без Rust:

```bash
npm run dev
```

## Этапы

0. Каркас репозитория ← сейчас
1. Анализ файла (ffprobe) и режим «Ремукс», определение GPU
2. NVENC-пайплайн, пресеты, очередь, прогресс
3. Интерфейс как в макете
4. A/B-тест на реальных загрузках в TikTok
5. Установщик, автообновление, апскейл RTX VSR
