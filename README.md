# Desktop-Camera-KMP

A desktop app (Windows, Linux in progress) that shows your webcam and streams it over the
network via RTSP.
The UI is built with **Kotlin Multiplatform + Compose Multiplatform**; the camera, rendering,
encoding and streaming run in a **Rust** native library, exposed to Kotlin through **UniFFI**.
On Windows it talks directly to **Direct3D 11** and **Windows Media Foundation**; on Linux to
**V4L2**, **VA-API** and **EGL**.

This is a personal learning project: the goal is to explore KMP, Rust and the native video
APIs of each platform by building something real. Contributions, questions and suggestions are welcome.

## Features

| Feature | Status |
| --- | --- |
| Windows: live camera preview (GPU-rendered with Direct3D 11) | ✅ Done |
| Windows: RTSP streaming (hardware H.264 encoder, RTP over UDP or TCP) | ✅ Done |
| Stream settings: interface, port, path, UDP/TCP | ✅ Done |
| Take snapshots | 🔜 Planned |
| Record videos | 🔜 Planned |
| Linux: RTSP streaming of H.264 cameras (V4L2, no re-encoding) | 🚧 In progress |
| Linux: preview of H.264 and MJPEG cameras (VA-API decode, EGL on X11/XWayland, zero copy) | 🚧 In progress |
| Linux: RTSP streaming of MJPEG cameras (VA-API JPEG decode + H.264 encode, zero copy) | 🚧 In progress |
| Linux: raw (YUYV/NV12) cameras, NVIDIA (NVDEC/NVENC) | 🔜 Planned |
| Support for macOS | 💭 Idea |

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
┌────────── Rust: camera_engine (.dll / .so) ──────────────────┐
│  CameraController ─► media traits (capture, preview, encoder)│
│     capture ─► FrameHub ──► preview renderer   [preview]     │
│                         └─► H.264 encoder                    │
│                                └─► RTP packetizer ─► RTSP    │
│                                                     server   │
├──────────────── backend picked at compile time ──────────────┤
│  Windows: WinRT MediaCapture → D3D11 renderer (HWND)         │
│           → Media Foundation H.264 encoder                   │
│  Linux:   V4L2 capture                                       │
│           H.264 camera → stream: straight to RTP (as is)     │
│                          preview: VA-API H.264 decode → EGL  │
│           MJPEG camera → VA-API JPEG decode → EGL preview    │
│                          and VA-API H.264 encode → stream    │
└──────────────────────────────────────────────────────────────┘
```

- A single camera capture is shared by every consumer (preview windows and the stream).
  It starts when the first consumer appears and stops when the last one goes away.
- `CameraController` only talks to the traits in `media/`; each platform implements them in
  its own backend (`win32/` or `linux/`).
- Frames never leave the GPU between capture, preview and encoding. On Linux the decoded
  frames are shared as DMA-BUFs between VA-API and EGL.
- On Linux the driver libraries (libva, EGL, GLES, X11, and CUDA/NVENC/NVDEC to detect
  NVIDIA GPUs) are
  loaded with `dlopen` at run time, so building needs no system development packages.
- The RTSP server is implemented from scratch (no external streaming library) on top of Tokio.

## Requirements

- **Windows 10 or 11** with a GPU with **Direct3D 11.1** support and a hardware H.264
  encoder (practically any Intel, AMD or NVIDIA GPU from the last years)
- or **Linux** (work in progress): a camera that sends H.264 or MJPEG (`v4l2-ctl
  --list-formats` shows `H264` or `MJPG`). H.264 is streamed without re-encoding; MJPEG
  is decoded and encoded to H.264 on the GPU, which needs VA-API JPEG decoding and H.264
  encoding with constant bitrate (Intel or AMD). The preview needs an X11 session (or
  XWayland) and a GPU that decodes H.264 or JPEG with VA-API. Raw cameras are not
  supported yet
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

On Linux, `cargo` must be on the `PATH` and the app needs an X11 session (or XWayland):

```sh
./gradlew :desktopApp:run
```

The first build takes a while. Gradle automatically:

1. Builds the Rust library with `cargo build --release` (task `buildRustRelease`).
2. Generates the Kotlin bindings with UniFFI into `shared/build/generated/uniffi/kotlin`
   (task `generateUniFFIBindings`).
3. Packages the library (`camera_engine.dll` or `libcamera_engine.so`) as a JVM resource
   under the JNA platform folder (e.g. `win32-x86-64/`) and runs the app.

You can open the project in **IntelliJ IDEA** or **Android Studio** with the Kotlin
Multiplatform plugin and run the `desktopApp` configuration from there.

### Linux notes

- **Libraries at run time:** the engine opens `libva.so.2`, `libva-drm.so.2`, `libEGL.so.1`,
  `libGLESv2.so.2` and `libX11.so.6` with `dlopen`. They come with the usual Mesa/VA-API
  drivers (e.g. `intel-media-driver` or Mesa's VA-API driver for AMD); no `-dev` packages
  are needed to build. Use `vainfo` to see which profiles your GPU decodes and encodes.
- **Permissions:** your user needs access to the camera (`/dev/video*`) and to the GPU render
  nodes (`/dev/dri/renderD*`), usually through the `video` and `render` groups.
- **Camera:** the first V4L2 camera found is used. Its nodes are grouped by USB bus, since a
  camera can expose each format on a different `/dev/videoN`, and H.264 is preferred over
  MJPEG when the camera offers both.
- **Choosing the GPU:** with more than one GPU, every stage stays on the GPU that drives the
  display by default, so frames are not copied between GPUs. Override it with the
  `CAMERA_ENGINE_GPU` environment variable:
  ```sh
  CAMERA_ENGINE_GPU=intel ./gradlew :desktopApp:run    # or nvidia, amd, /dev/dri/renderD129
  ```
- **No preview?** When there is no X11 display or no GPU that decodes the camera's format with
  VA-API, the **Ver** tab shows a notice instead of the preview. Streaming H.264 cameras still
  works, since it doesn't need a GPU.

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
If clients on other machines can't connect, check whether the firewall (Windows Firewall,
or `ufw`/`firewalld` on Linux) is blocking the port. Over UDP the video goes to separate
ports on the client; if it connects but shows no image, try TCP, which only uses the RTSP port.

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
│       ├── jvmMain/             # Desktop implementation (Windows and Linux)
│       │   ├── NativeCameraService.kt    # CameraService → Rust bindings
│       │   └── NativeLibLoader.kt        # Loads the native library through JNA
│       └── jvmTest/             # Loads the real native library from the classpath
└── rust/
    └── camera_engine/src/
        ├── ffi/                 # API exported to Kotlin (UniFFI)
        │   ├── camera.rs        # CameraController: preview + capture lifecycle
        │   └── rtsp_transmitter.rs  # start/stop streaming
        ├── codec/h264/          # H.264 parsing and decoding process (POC, references,
        │                        #   lists, output order) for the hardware decoders, and the
        │                        #   SPS/PPS the hardware encoders send
        ├── codec/jpeg.rs        # Baseline JPEG headers (MJPEG), default Huffman tables
        ├── media/               # Platform independent contracts
        │   ├── mod.rs           # Backend traits (capture, preview, encoder) and shared types
        │   └── hub.rs           # FrameHub: fan-out of frames to consumers
        ├── win32/               # Windows backend
        │   ├── capture.rs       # Camera capture (WinRT MediaCapture)
        │   ├── device.rs        # Shared D3D11 device
        │   ├── frame.rs         # Pool of NV12 GPU textures
        │   ├── renderer.rs      # Draws frames into the preview window
        │   └── encoder.rs       # Hardware H.264 encoder (Media Foundation)
        ├── linux/               # Linux backend: H.264 cameras stream as is (passthrough),
        │                        #   MJPEG ones are decoded and encoded again on the GPU
        │   ├── egl/             # Preview: imports decoded DMA-BUFs, draws on the X11 window
        │   ├── gpu/             # GPU detection (sysfs, VA-API, CUDA/NVENC/NVDEC) and choice
        │   ├── v4l2/            # Camera capture (V4L2 ioctls, mmap buffers, DMA-BUF export)
        │   └── vaapi/           # H.264 and JPEG decode to NV12 surfaces, H.264 encode (VA-API,
        │                        #   loaded with dlopen);
        │                        #   sys.rs is generated by tools/gen-va-bindings.sh
        ├── rtsp/                # RTSP server, RTP packetizer, H.264 parsing
        └── bin/uniffi-bindgen.rs  # Binding generator used by Gradle
```

## Contributing

Every kind of contribution helps: bug reports, ideas, docs and code.

1. **Open an issue first** for bigger changes, so we can discuss the approach.
2. Fork the repo and create a branch from `main`.
3. Make your change and check that the app still builds and runs, and that the JVM tests
   pass (they load the native library the same way the app does):
   ```powershell
   .\gradlew.bat :desktopApp:run
   .\gradlew.bat :shared:jvmTest
   ```
   On Linux use `./gradlew` instead of `.\gradlew.bat`.
4. If you touched the Rust code, run its tests and the formatter/linter:
   ```sh
   cd rust
   cargo test
   cargo fmt
   cargo clippy
   ```
   On Linux, a few tests need real hardware (a VA-API GPU, EGL, `ffmpeg`) and are skipped by
   default. Run them with `cargo test -- --ignored --nocapture`.
5. Open a pull request describing what changed and how you tested it. If the change touches
   the native layer, say which platform(s) you tested it on.

### Where to start

- **Adding a feature to the native layer:** implement it in `rust/camera_engine/src`, export it
  with `#[uniffi::export]` in `ffi/`, then call the generated binding from
  `NativeCameraService.kt` and add the method to the `CameraService` interface. If it needs
  platform code, add it to the traits in `media/mod.rs` and implement it in both `win32/`
  and `linux/`.
- **UI changes:** everything lives in `shared/src/commonMain`.
- **Good first contributions:** the planned features (snapshots, video recording), saving the
  stream settings across restarts, a bitrate option in the settings dialog, or improving
  error messages. On Linux: support for raw (YUYV/NV12) cameras or NVIDIA (NVDEC/NVENC).

### Tips

- The Kotlin bindings are **generated** — don't edit them by hand. They're rebuilt whenever
  the Rust code changes.
- If the build behaves strangely after changing Rust code, try `.\gradlew.bat clean` and
  `cargo clean` in the `rust/` folder.
- `linux/vaapi/sys.rs` is generated and committed. Regenerate it with
  `rust/camera_engine/tools/gen-va-bindings.sh` only when you need a new VA-API struct or
  constant (it needs `bindgen-cli`, libclang and the libva headers).
- Learning resources for the stack:
  [Kotlin Multiplatform](https://kotlinlang.org/docs/multiplatform.html),
  [UniFFI](https://mozilla.github.io/uniffi-rs/),
  [windows-rs](https://github.com/microsoft/windows-rs),
  [Media Foundation](https://learn.microsoft.com/windows/win32/medfound/microsoft-media-foundation-sdk),
  [V4L2](https://docs.kernel.org/userspace-api/media/v4l/v4l2.html),
  [VA-API (libva)](https://github.com/intel/libva).

## License

[MIT](LICENSE)
