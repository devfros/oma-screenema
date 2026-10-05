# Screenema

A local recording studio for Hyprland. Capture a display, choose a backdrop, and export one finished MP4.

```sh
cargo run --release
```

Requires GTK 4.10+, Hyprland, FFmpeg with libx264, and GPU Screen Recorder with `-write-first-frame-ts` support. `grim` supplies the real display preview. GTK's media backend supplies playback, with **Open video** as an external-player option.

## Recording

1. Choose a backdrop and your audio sources.
2. **Auto camera** is off at startup. Turn it on to open the system mouse-access prompt. Approve for click-triggered zooms, or cancel to use pause-based zooms. Approval lasts for this app session, including later takes. Small pointer movements leave the camera steady. **Preview camera move** demonstrates a cross-screen move on a snapshot of your actual display. **Refresh display** briefly hides Screenema to take a new snapshot.
3. **Start recording** runs a cancellable three-second countdown. Minimize Screenema for a clean take.
4. Return and choose **Stop & finish**. `Ctrl+Shift+R` also starts/stops while Screenema has keyboard focus.
5. Play the finished video in the app or open it in your usual player.

## Image and camera

The overview keeps source pixels at 1:1. Padding increases the output canvas instead of shrinking the capture. GPU capture uses the ultra quality preset; the final H.264 export uses CRF 12, 60 fps, and AAC audio. The MP4 uses 4:2:0 color for player compatibility, so fine colored details can still lose chroma detail.

The camera transforms the entire composition, including the screen edges, shadow, and cursor. Every move is eased in, held, and eased back: it approaches over 500 ms, holds at full zoom while the pointer keeps working, then returns over 800 ms, and rests in the overview outside activity regions. While zoomed, a 25% inset safe zone keeps small movements steady; leaving that zone moves the focus with a damped spring. Nearby activity extends the current zoom instead of queuing delayed shots. Focus stops following during zoom-out. It does not slide a permanent crop around inside a fixed frame. Preview and export use the same compositor and camera planner. Source timing is aligned to the GPU recorder's first frame.

With mouse access, auto-camera groups clicks no more than 2.5 seconds apart into one shot. A shot reaches full zoom on its first click, holds there while every later click in the cluster re-arms the dwell and the cursor keeps steering the focus, then eases back to the overview starting 600 ms after the last click. A click that lands during the release eases back in rather than dropping to the overview, so a burst of clicking across the screen plays as one continuous move. A take with no clicks stays in overview. Without access, 450 ms cursor pauses trigger zooms and takes under three seconds stay in overview. The camera follows movement once zoomed in either mode. There is no timeline editor or manual zoom-region editing yet.

[Recordly](https://github.com/webadderallorg/Recordly/tree/4fda917aff39729eb22e72bbb881c6b7123d4033) is the behavior reference, specifically `cursorFollowCamera.ts`, `sceneMotion.ts`, `motionSmoothing.ts`, and `zoomSuggestionUtils.ts`. This Rust/Cairo implementation uses its safe-zone and spring-motion principles. Click clustering uses the same 2.5 s gap as Recordly; each cluster is then shaped into a 500 ms approach, a 600 ms dwell, and an 800 ms release, and the existing Rust/Cairo transition curve is retained. Click timestamps use the same monotonic clock as the capture, and click positions are interpolated from the cursor track after alignment to the first video frame. Camera frames are precomputed at 60 fps so seeking and preview playback produce the same path as export.

Twelve built-in gradient backdrops are available: Dusk, Ocean, Graphite, Aurora, Ember, Iris, Rose, Sand, Forest, Glacier, Midnight, and Pearl.

## Mouse access

Click detection needs `pkexec` and a running graphical Polkit agent (provided by Omarchy's shell). Screenema uses the system authentication dialog and never asks for or handles your password. Nothing is installed, and no groups, udev rules, or device permissions are changed.

The `--click-helper` process opens only devices advertising mouse buttons, applies kernel event masks that exclude keyboard keys and other payloads, then drops root privileges and disables future privilege elevation. It keeps those mouse handles for the app session. It sends button-press timestamps only during a recording; idle events are discarded at the next start. Stopping a take pauses collection, disabling auto-camera prevents collection on subsequent takes, and closing Screenema terminates the helper and releases access. There is no always-running service.

Cancelling or failing authorization leaves pause-based zooms available. If a mouse disconnects or the helper fails, the current take falls back to pause-based zooms and reports the problem. Toggle auto-camera off and on to retry. Newly connected mice require reauthorization after restarting Screenema. This uses physical mouse-button events; compositor-generated touchpad taps and synthetic clicks are not detected.

## Files

Successful takes leave one finished file in `~/Videos/OmaScreenema/`. The temporary capture, log, and first-frame timestamp live under `$XDG_CACHE_HOME/oma-screenema/` (normally `~/.cache/oma-screenema/`) and are deleted after the final MP4 is verified. Failed exports retain recovery data in that cache. Existing recordings are not swept or deleted.

Export uses CPU compositing and encoding, so full-resolution camera effects can take longer than the recording itself. Recordings and display snapshots stay local.

## Checks

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

The tests cover authorization denial, idle filtering, repeated takes, helper shutdown, keyboard-event rejection, click/video alignment, click clustering, click-triggered exported frames, the dwell after a click, click bursts that stay zoomed and follow, late target handoffs, safe-zone stability, following during movement, seek independence, frozen zoom-out focus, all twelve backdrops, and actual rendered frame boundaries during zoom, return to overview, one-pixel detail retention, audio and frame-rate preservation, scaled/rotated monitor coordinates, timing alignment, and temporary-file cleanup. Failed exports must preserve recovery data and any prior finished video.

Some tests render real video with FFmpeg and are slower than the rest.

## License

MIT. See [LICENSE](LICENSE).
