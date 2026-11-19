# Slow startup on Linux: measure it, then fix it

The OpenTune desktop app is a Tauri 2 app: a Rust backend plus a WebKitGTK
webview. When the window takes many seconds to appear, the cause is almost
never OpenTune's own code. Since the startup work (this document's companion
change), the backend parses nothing before the window shows and every
startup-path command runs off the UI thread. The usual causes are the platform
underneath: GTK/WebKit initialisation, the GPU renderer, D-Bus timeouts, or the
AppImage runtime. This page explains how to find out which one it is on your
machine.

## Recommended launch environment

Start OpenTune with as little WebKit/GTK tuning as possible. The usual cause
of a laggy window is a launcher, wrapper script or `.desktop` file that sets
a workaround variable globally.

| Variable | Recommendation | Why |
|---|---|---|
| `WEBKIT_DISABLE_COMPOSITING_MODE` | **Leave unset** | Turns off accelerated compositing: every pixel is painted on the CPU, and the whole window (sidebar included) scrolls slowly. OpenTune shows a one-time warning when it is set. Use it only to work around a GPU fault, and only after measuring (section 2). |
| `GDK_BACKEND` | Leave unset (native Wayland on a Wayland session) | Native Wayland is preferred. Set `GDK_BACKEND=x11` only if the window will not appear on Wayland. |
| `WEBKIT_DISABLE_DMABUF_RENDERER` | Leave unset; OpenTune decides | GPU (DMA-BUF) rendering stays on except on known-bad setups, see below. A value you set is never changed. |

Tested 2026-11-19: AMD GPU (amdgpu) on a local Wayland session, with
compositing and DMA-BUF on, scrolls smoothly. The same machine with
`WEBKIT_DISABLE_COMPOSITING_MODE=1` lagged across the whole window, sidebar
included.

### What OpenTune sets itself

WebKitGTK 2.42+ renders through DMA-BUF/GBM buffers: the GPU hands finished
frames to the compositor without a CPU copy. On Linux, OpenTune keeps that on
by default and sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` (the shared-memory
path) only when it detects a setup where DMA-BUF is known to stall or draw a
blank window. The first rule that matches wins and is shown as the reason:

| # | Rule | Detected by |
|---|---|---|
| 1 | No usable render node | No `/dev/dri/renderD*`, or none that this user can read and write |
| 2 | NVIDIA proprietary driver | `/proc/driver/nvidia/version` exists, or a render node's driver (`/sys/class/drm/renderD*/device/driver`) is `nvidia`. Nouveau is fine. |
| 3 | Virtual or emulated GPU | Render-node driver `virtio_gpu`, `vmwgfx`, `qxl`, `vboxvideo`, `bochs-drm`, `bochs`, `cirrus` or `simpledrm` |
| 4 | Software GL forced | `LIBGL_ALWAYS_SOFTWARE` set to a true value, or `GALLIUM_DRIVER=llvmpipe` / `softpipe` |
| 5 | Remote X | `SSH_CONNECTION` set, no `WAYLAND_DISPLAY`, and `DISPLAY` is not a local `:N` / `:N.M` |
| 6 | Self-heal: the last start did not draw | See below |

amdgpu, i915/xe, radeon, nouveau, panfrost, v3d and other native drivers fall
through to DMA-BUF on.

**Self-heal.** At every start OpenTune writes a small marker file
(`~/.cache/opentune/render-pending`, or under `$XDG_CACHE_HOME`) and removes it
when the window has painted its first frame. If the marker is still there at
the next start and that start had DMA-BUF on, the window never drew: OpenTune
says so once (in the window and in the startup trace) and uses the fallback.
After 2 clean starts with the fallback it retries DMA-BUF once. If that start
fails too, DMA-BUF stays off for good on this machine. (A marker left by an
instance that is still running is not counted, so starting a second window
does not trip it.) The state is in
`~/.cache/opentune/render-state` (`state: on | fallback | retry | off`);
delete that file to start over.

**Overrides** always win, in this order:

1. `OPENTUNE_KEEP_WEBKIT_ENV=1`: OpenTune changes nothing (and writes no
   self-heal files).
2. `WEBKIT_DISABLE_DMABUF_RENDERER` set by you (`1` or `0`): never changed.
3. `OPENTUNE_DMABUF=auto|on|off`: `auto` (default) applies the rules above,
   `on` never disables DMA-BUF, `off` always does.

The startup trace (`platform env: DMA-BUF on|off — <reason>`), `render_env`
and the Render check card show the decision, the rule that fired (or "none,
DMA-BUF on") and the render-node driver name(s).

OpenTune never sets `WEBKIT_DISABLE_COMPOSITING_MODE` or `GDK_BACKEND`.

### Render check

Alpha/bench builds have a **Render check** card in the Debug panel. It shows
the rendering environment the app sees (the variables above, whether a
Wayland/X11 display and an AppImage are in use, the `/dev/dri` render nodes,
the DMA-BUF decision with its rule and the render-node driver, what OpenTune
set), the WebGL renderer (`llvmpipe` or `SwiftShader` means
software GL), and measures scrolling: pick a panel, press **Measure scrolling**
and scroll it for about 5 seconds. A smooth panel shows about 60 fps with p50
and p95 frame times near 17 ms on a 60 Hz screen; a CPU-painted window shows
long frames (over 50 ms) and a low frame rate. Results go to the session log.

On any build, `OPENTUNE_PERF=1` logs the same environment at startup and
measures the first ~5 s of scrolling after every panel switch, printed to
stderr:

```bash
OPENTUNE_PERF=1 ./OpenTune.AppImage 2>&1 | grep -E '\[perf|render:'
```

## 1. Get a timing breakdown

```bash
OPENTUNE_TRACE_STARTUP=1 ./OpenTune.AppImage 2>&1 | tee startup.log
```

(For an extracted or `.deb` install, run the binary with the same variable.)
Every line is `[startup <ms since main> ms] <milestone>`, for example:

```
[startup      0.0 ms] trace on (process was alive 180 ms before this point: ...)
[startup      0.2 ms] platform env: DMA-BUF on — none, DMA-BUF on [None]; render-node driver(s): amdgpu; set []; now ...
[startup     35.1 ms] setup plugin: start
[startup     35.3 ms] setup: data roots resolved (0.1 ms)
[startup     35.9 ms] setup plugin: done (state managed)
[startup     36.4 ms] data: background warm-up started
[startup     52.0 ms] window 'main' created
[startup     60.3 ms] webview 'main' created
[startup     61.0 ms] event loop ready
[startup    140.2 ms] page load Started: tauri://localhost
[startup    310.7 ms] ipc build_channel (dispatch held the UI thread 0.1 ms)
[startup    315.2 ms] ui: app-core.js running (page +170 ms)
[startup    360.9 ms] ui: first frame painted (page +215 ms)
[startup    410.3 ms] ui: boot done (page +265 ms)
```

How to read the gaps:

| Large gap ... | Points at |
|---|---|
| "process was alive N ms before this point" | Dynamic linking and the AppImage FUSE mount (see section 4) |
| `setup plugin: done` → `window created` / `webview created` | GTK/WebKit init: D-Bus, portal, accessibility bus, GPU/EGL (sections 2 and 3) |
| `webview created` → `page load Started` / `ui: app-core.js running` | The WebKit web process starting: GPU renderer, GStreamer, fonts (sections 2 and 3) |
| `ipc <cmd> (dispatch held the UI thread N ms)` with a large N | A synchronous backend command. Please report it: it is an OpenTune bug |
| `data: parsed ecudefs ... (N ms)` | Definition parsing. It runs on a background thread and should not delay the window |

No `window`/`webview` lines for 25–30 s, followed by everything else at
normal speed, almost always means a **D-Bus call timed out**. The default
D-Bus reply timeout is 25 s. See section 3.

Run it twice. If only the first launch after a reboot or update is slow, the
cause is a cache being built (fontconfig, the GStreamer registry, the page
cache warming up the AppImage). If every launch is slow, it is a timeout or
a renderer problem.

## 2. A/B the WebKit/GTK environment

OpenTune keeps WebKitGTK's DMA-BUF renderer on unless it detects a
known-bad setup (see "What OpenTune sets itself" above). On some driver
stacks that path stalls, shows a blank or white window, or falls back slowly.
The risk is highest in an AppImage, where a bundled WebKit (from the Debian 12
build image) runs against the host's Mesa/libgbm.

- Force the shared-memory path: `OPENTUNE_DMABUF=off` (or
  `WEBKIT_DISABLE_DMABUF_RENDERER=1`).
- Force DMA-BUF on, whatever the rules say: `OPENTUNE_DMABUF=on` (or
  `WEBKIT_DISABLE_DMABUF_RENDERER=0`).
- Opt out of every OpenTune environment decision: `OPENTUNE_KEEP_WEBKIT_ENV=1`.

Time each variant with this loop. It needs the trace and stops each run when
the first frame has been painted:

```bash
app=./OpenTune.AppImage
for v in "OPENTUNE_DMABUF=off" "" \
         "WEBKIT_DISABLE_COMPOSITING_MODE=1" \
         "NO_AT_BRIDGE=1" \
         "GDK_BACKEND=x11" \
         "GST_PLUGIN_SYSTEM_PATH_1_0=/nonexistent GST_REGISTRY_UPDATE=no" \
         "GIO_USE_PROXY_RESOLVER=dummy"; do
  start=$(date +%s.%N)
  env $v OPENTUNE_TRACE_STARTUP=1 timeout 90 "$app" 2>&1 | grep -m1 'first frame painted' >/dev/null
  printf '%-60s %6.1f s\n' "${v:-<OpenTune defaults>}" "$(echo "$(date +%s.%N) - $start" | bc)"
  pkill -f opentune-desktop; sleep 1
done
```

| Variable | What it changes | If it fixes the delay |
|---|---|---|
| `OPENTUNE_DMABUF=off` | Shared-memory rendering instead of DMA-BUF | DMA-BUF is the problem on this machine: keep `OPENTUNE_DMABUF=off` and please report the GPU and driver so the rules can learn it |
| `WEBKIT_DISABLE_COMPOSITING_MODE=1` | No accelerated compositing at all (CPU drawing) | A GPU/EGL problem: check the Mesa version and `glxinfo -B` / `eglinfo` |
| `NO_AT_BRIDGE=1` | GTK skips the accessibility (AT-SPI) bus | The a11y bus is timing out: `systemctl --user status at-spi-dbus-bus` |
| `GDK_BACKEND=x11` | XWayland instead of native Wayland | A Wayland/portal issue in the bundled GTK |
| `GST_PLUGIN_SYSTEM_PATH_1_0=/nonexistent GST_REGISTRY_UPDATE=no` | GStreamer finds no plugins and does not rescan | WebKit's GStreamer init is scanning the host's plugins (section 3) |
| `GIO_USE_PROXY_RESOLVER=dummy` | GIO skips libproxy/GNOME proxy lookup | Proxy auto-config (WPAD/PAC) lookup is timing out |

Only the DMA-BUF default is applied automatically. Each of the others has a
cost: no GPU compositing, no screen-reader support, or no proxy support. Use
them as a workaround only once the measurement shows they help. They can go in
a wrapper script or in the `.desktop` file's `Exec=env VAR=1 ...` line.

## 3. Look for timeouts and scans

**D-Bus timeouts**: a single ~25 s wait.

```bash
strace -f -tt -T -e trace=connect,sendmsg,recvmsg,poll,ppoll -o st.txt ./OpenTune.AppImage
# A ppoll/poll that returned after ~25 s is the timeout. Look at the connect/sendmsg just before it:
awk -F'<' '{t=$NF; sub(/>.*/,"",t); if (t+0 > 2) print}' st.txt | head
```

In a second terminal, `dbus-monitor --session` shows which call is waiting.
The usual suspects are `org.freedesktop.portal.*` (portal settings or colour
scheme; check `systemctl --user status xdg-desktop-portal xdg-desktop-portal-gnome`),
`org.a11y.Bus` (AT-SPI), and `org.freedesktop.Notifications`.
`G_MESSAGES_DEBUG=all` makes GLib/GTK log what they are doing.

**Where the time goes overall**:

```bash
strace -c -f -o summary.txt ./OpenTune.AppImage    # close the window when it appears
sort -k2 -n -r summary.txt | head -20              # syscalls by total time
```

A high `openat`/`stat` count with long totals points at a cache build or a
plugin scan. Lots of time in `futex`/`poll` points at waiting on another
process or a D-Bus peer.

**GStreamer.** WebKit uses GStreamer for media. The first time it
initialises, GStreamer scans every plugin under
`/usr/lib/x86_64-linux-gnu/gstreamer-1.0` and caches the result in
`~/.cache/gstreamer-1.0/registry.x86_64.bin`. The AppImage bundles its own
`libgstreamer` (Debian 12) but no plugins, so it reads the host's plugins and
registry. A mismatch can trigger a rescan. To check:

```bash
GST_DEBUG=GST_REGISTRY:4,GST_PLUGIN_LOADING:4 OPENTUNE_TRACE_STARTUP=1 ./OpenTune.AppImage 2>&1 | grep -E 'registry|startup' | head -50
```

If it rescans on every launch, run with `GST_PLUGIN_SYSTEM_PATH_1_0=/nonexistent`.
OpenTune plays no media, so nothing is lost.

**WebKit logging.** `WEBKIT_DEBUG=all` (or e.g. `WEBKIT_DEBUG=Process,Loading`)
prints WebKit's log channels. Many distro builds compile most channels out,
so no output is not a sign of anything.

**fontconfig.** The first launch after installing fonts may rebuild the font
cache. Run `fc-cache -v` once by hand; if the next launch is fast, that was it.

## 4. AppImage-specific checks

The AppImage is a squashfs image mounted through FUSE (`libfuse2`) every time
it starts. It bundles ~200 libraries, including WebKitGTK, GTK 3 and GLib from
the Debian 12 build image, but **not** Mesa, libEGL, libgbm, fontconfig or
freetype. Those come from the host, so the bundled WebKit meets the host's
GPU stack.

```bash
# 1. Take FUSE out of the picture: extract once, run the extracted copy.
./OpenTune.AppImage --appimage-extract >/dev/null && OPENTUNE_TRACE_STARTUP=1 squashfs-root/AppRun
# (or: APPIMAGE_EXTRACT_AND_RUN=1 ./OpenTune.AppImage)

# 2. Compare with the host's own WebKit: install the .deb from the same release
#    and run /usr/bin/opentune-desktop with the trace.
```

- The extracted copy is fast: the FUSE mount or a cold page cache is the
  cost. Check that `libfuse2t64` is installed (Ubuntu 24.04 does not install
  it by default).
- The `.deb` is fast but the AppImage is slow: the cause is a bundled-library
  mismatch with the host (GPU stack, GStreamer, GIO modules). Keep the DMA-BUF
  default and try the section 2 variables. Please report which one fixed it.
- Both are slow: the cause is in the session itself (D-Bus, portal, GPU
  driver). Use section 3.

The AppImage's `AppRun` hook also asks the desktop portal for the colour
scheme with `dbus-send ... --reply-timeout=1000`. It costs at most 1 s, even
when the portal hangs.

## 5. Slow scrolling or a low frame rate

Scrolling a long panel should run at the display's refresh rate. WebKitGTK
paints scrolled content on the CPU, so expensive CSS shows up directly as a
low frame rate. The UI avoids the known costs: card shadows have no blur (a
blurred shadow on every card held long panels at 30–45 fps in WebKitGTK 2.52),
the panel fade animates opacity only, and the document itself never scrolls
(the sidebar and the content pane are separate scroll areas).

If scrolling is still slow, run the Render check (Debug panel, or
`OPENTUNE_PERF=1`). It shows whether compositing is off
(`WEBKIT_DISABLE_COMPOSITING_MODE` set: everything is CPU-painted), whether
DMA-BUF is off and which rule turned it off, and whether WebGL runs on a
software renderer. If a rule turned DMA-BUF off on a machine with a working
GPU driver, try it on:

```bash
OPENTUNE_DMABUF=on ./OpenTune.AppImage
```

With DMA-BUF on, WebKit hands finished frames to the compositor without a
CPU copy. If the window comes up and scrolls better, keep the variable in a
wrapper script or the `.desktop` file, and please report the setup.
`WEBKIT_DISABLE_COMPOSITING_MODE=1` does the opposite (all CPU) and is only a
workaround for GPU faults.

## 6. What to send with a bug report

- `startup.log` from section 1, from a slow run (and from a second run).
- The section 2 timing table.
- `lsb_release -a`, `uname -r`, `echo $XDG_SESSION_TYPE`, GPU and driver
  (`lspci -k | grep -A3 VGA`, `glxinfo -B | head`).
- Whether the extracted AppImage and the `.deb` behave the same.
- For slow scrolling: the Render check output (Debug panel, or the
  `OPENTUNE_PERF=1` lines).
