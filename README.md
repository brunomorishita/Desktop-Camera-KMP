# Desktop-Camera-KMP

A Windows desktop app that shows your webcam and streams it over the network via RTSP.
The UI is built with **Kotlin Multiplatform + Compose Multiplatform**; the camera, rendering,
encoding and streaming run in a **Rust** native library that talks directly to
**Direct3D 11** and **Windows Media Foundation**, exposed to Kotlin through **UniFFI**.

This is a personal learning project: the goal is to explore KMP, Rust, DirectX and Media
Foundation by building something real. Contributions, questions and suggestions are welcome.

## Features

| Feature | Status |
| --- | --- |
| Live camera preview (GPU-rendered with Direct3D 11) | ✅ Done |
| RTSP streaming (hardware H.264 encoder, RTP over UDP or TCP) | ✅ Done |
| Stream settings: interface, port, path, UDP/TCP | ✅ Done |
| Take snapshots | 🔜 Planned |
| Record videos | 🔜 Planned |
| Support for macOS/Linux | 💭 Idea |

The app has two screens, selected from the bottom bar:

- **Ver** (View): shows the camera preview.
- **Transmitir** (Transmit): starts an RTSP server and shows the stream URL and how many
  clients are connected.

## How it works

```
┌──────────────────────── Kotlin (JVM) ────────────────────────┐
│  desktopApp  →  shared (Compose UI, ViewModels, Koin DI)     │
│                    │  CameraService                          │
│                    ▼                                         │
│        UniFFI-generated bindings (JNA)                       │
└────────────────────┬─────────────────────────────────────────┘
                     ▼
┌──────────────── Rust: camera_engine (.dll) ──────────────────┐
│  CameraController                                            │
│     capture (WinRT MediaCapture / MediaFrameReader)          │
│        │  frames stay on the GPU (shared D3D11 device)       │
│        ▼                                                     │
│     FrameHub ──► renderer (D3D11 → window HWND)  [preview]   │
│              └─► H.264 encoder (Media Foundation)            │
│                     └─► RTP packetizer ─► RTSP server        │
└──────────────────────────────────────────────────────────────┘
```

- A single camera capture is shared by every consumer (preview windows and the stream).
  It starts when the first consumer appears and stops when the last one goes away.
- Frames never leave the GPU between capture, preview and encoding.
- The RTSP server is implemented from scratch (no external streaming library) on top of Tokio.

## Requirements

- **Windows 10 or 11** (the native layer uses Windows-only APIs)
- A GPU with **Direct3D 11.1** support and a hardware H.264 encoder (practically any
  Intel, AMD or NVIDIA GPU from the last years)
- A webcam
- **Rust** toolchain (`rustc` + `cargo`) — install with [rustup](https://rustup.rs/)
- **JDK 21** — Gradle downloads it automatically if needed (via the foojay toolchain resolver)

You don't need to install Gradle; use the wrapper included in the repository.

## Getting started

```powershell
git clone https://github.com/brunomorishita/Desktop-Camera-KMP.git
cd Desktop-Camera-KMP
.\gradlew.bat :desktopApp:run
```

The first build takes a while. Gradle automatically:

1. Builds the Rust library with `cargo build --release` (task `buildRustRelease`).
2. Generates the Kotlin bindings with UniFFI into `shared/build/generated/uniffi/kotlin`
   (task `generateUniFFIBindings`).
3. Packages `camera_engine.dll` as a JVM resource and runs the app.

You can open the project in **IntelliJ IDEA** or **Android Studio** with the Kotlin
Multiplatform plugin and run the `desktopApp` configuration from there.

### Watching the stream

Open the **Transmitir** tab and click the **Transmitir** button (**Parar** stops it). The app
shows a URL like:

```
rtsp://<your-ip>:8554/camera
```

Open it in any RTSP player on the same network, for example:

```powershell
ffplay rtsp://192.168.0.10:8554/camera
# or
vlc rtsp://192.168.0.10:8554/camera
```

The gear button next to **Transmitir** opens the stream settings (only while stopped):

- **Interface:** which network address to listen on, or all of them (`0.0.0.0`).
  Pick `127.0.0.1` to keep the stream on this machine.
- **Porta / Caminho:** the port (default `8554`) and the URL path (default `camera`).
- **Transporte:** accept RTP over UDP and TCP, or only one of them. Clients asking for
  the other one get `461 Unsupported Transport`. FFmpeg retries with the allowed one; for
  other players, force it (e.g. `ffplay -rtsp_transport tcp ...` or VLC's "RTP over RTSP (TCP)").

Settings are kept while the app is open and reset to the defaults on restart.
If clients on other machines can't connect, check whether Windows Firewall is blocking the port.

## Project structure

```
Desktop-Camera-KMP/
├── desktopApp/                  # JVM entry point (main.kt), window and packaging
├── shared/
│   └── src/
│       ├── commonMain/          # Platform-agnostic code
│       │   ├── App.kt           # Navigation and bottom bar
│       │   ├── CameraScreen.kt / CameraView.kt / CameraViewModel.kt
│       │   ├── TransmitScreen.kt / TransmitViewModel.kt
│       │   ├── domain/CameraService.kt   # Interface the UI depends on
│       │   └── di/                       # Koin modules
│       └── jvmMain/             # Windows implementation
│           ├── WindowsCameraService.kt   # CameraService → Rust bindings
│           └── NativeLibLoader.kt        # Loads camera_engine.dll
└── rust/
    └── camera_engine/src/
        ├── ffi/                 # API exported to Kotlin (UniFFI)
        │   ├── camera.rs        # CameraController: preview + capture lifecycle
        │   └── rtsp_transmitter.rs  # start/stop streaming
        ├── media/               # Platform independent contracts
        │   ├── mod.rs           # Backend traits (capture, preview, encoder) and shared types
        │   └── hub.rs           # FrameHub: fan-out of frames to consumers
        ├── win32/               # Windows backend
        │   ├── capture.rs       # Camera capture (WinRT MediaCapture)
        │   ├── device.rs        # Shared D3D11 device
        │   ├── frame.rs         # Pool of NV12 GPU textures
        │   ├── renderer.rs      # Draws frames into the preview window
        │   └── encoder.rs       # Hardware H.264 encoder (Media Foundation)
        ├── linux/               # Linux backend (placeholder, not implemented yet)
        ├── rtsp/                # RTSP server, RTP packetizer, H.264 parsing
        └── bin/uniffi-bindgen.rs  # Binding generator used by Gradle
```

## Contributing

Every kind of contribution helps: bug reports, ideas, docs and code.

1. **Open an issue first** for bigger changes, so we can discuss the approach.
2. Fork the repo and create a branch from `main`.
3. Make your change and check that the app still builds and runs:
   ```powershell
   .\gradlew.bat :desktopApp:run
   ```
4. If you touched the Rust code, run its tests and the formatter/linter:
   ```powershell
   cd rust
   cargo test
   cargo fmt
   cargo clippy
   ```
5. Open a pull request describing what changed and how you tested it.

### Where to start

- **Adding a feature to the native layer:** implement it in `rust/camera_engine/src`, export it
  with `#[uniffi::export]` in `ffi/`, then call the generated binding from
  `WindowsCameraService.kt` and add the method to the `CameraService` interface.
- **UI changes:** everything lives in `shared/src/commonMain`.
- **Good first contributions:** the planned features (snapshots, video recording), saving the
  stream settings across restarts, a bitrate option in the settings dialog, or improving
  error messages.

### Tips

- The Kotlin bindings are **generated** — don't edit them by hand. They're rebuilt whenever
  the Rust code changes.
- If the build behaves strangely after changing Rust code, try `.\gradlew.bat clean` and
  `cargo clean` in the `rust/` folder.
- Learning resources for the stack:
  [Kotlin Multiplatform](https://kotlinlang.org/docs/multiplatform.html),
  [UniFFI](https://mozilla.github.io/uniffi-rs/),
  [windows-rs](https://github.com/microsoft/windows-rs),
  [Media Foundation](https://learn.microsoft.com/windows/win32/medfound/microsoft-media-foundation-sdk).

## License

[MIT](LICENSE)
