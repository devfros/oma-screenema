use super::*;
use gtk::cairo::{self, Context, Format, ImageSurface};
use std::io::{BufRead, BufReader, Read};

const ZOOM: f64 = 1.6;
const EASE_SECONDS: f64 = 0.95;
// A click reaches full zoom on the click itself, dwells there while the pointer keeps
// working, then eases back out. Splitting the ramp into three phases is what makes the
// move read as a camera move instead of a single-frame spike.
const ZOOM_IN_SECONDS: f64 = 0.5;
const ZOOM_HOLD_SECONDS: f64 = 0.6;
const ZOOM_OUT_SECONDS: f64 = 0.8;
// Recordly groups clicks this far apart. A cluster is fully released 1.4 s after its last
// click, and the next one cannot start before 2.0 s, so clusters never overlap.
const CLICK_CLUSTER_SECONDS: f64 = 2.5;

#[derive(Clone, Copy, Debug)]
pub(super) struct Geometry {
    pub width: u32,
    pub height: u32,
    pub padding: f64,
    pub source_width: u32,
    pub source_height: u32,
}

impl Geometry {
    pub fn new(width: u32, height: u32) -> Self {
        // Keep the capture at exactly 1:1 in the overview. Add canvas, never shrink text.
        let padding = ((width.min(height) as f64 * 0.075).round() as u32).div_ceil(2) * 2;
        Self {
            width: width.div_ceil(2) * 2 + padding * 2,
            height: height.div_ceil(2) * 2 + padding * 2,
            padding: padding as f64,
            source_width: width,
            source_height: height,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Camera {
    pub zoom: f64,
    pub x: f64,
    pub y: f64,
}

impl Camera {
    fn overview(geometry: Geometry) -> Self {
        Self {
            zoom: 1.0,
            x: geometry.width as f64 / 2.0,
            y: geometry.height as f64 / 2.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Shot {
    /// Zoom begins.
    start: f64,
    /// Full zoom reached, and held until the release.
    peak: f64,
    /// Zoom starts returning to the overview.
    release: f64,
    /// Zoom is back at the overview.
    end: f64,
    x: f64,
    y: f64,
}

pub(super) struct CameraPlan {
    geometry: Geometry,
    frames: Vec<Camera>,
}

fn ease(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * t * (10.0 + t * (-15.0 + 6.0 * t))
}

impl CameraPlan {
    pub fn new(
        geometry: Geometry,
        area: CaptureArea,
        samples: &[CursorSample],
        duration: f64,
        enabled: bool,
        click_camera: bool,
    ) -> Self {
        let overview = Camera::overview(geometry);
        let mut shots: Vec<Shot> = Vec::new();
        if enabled
            && duration.is_finite()
            && duration >= if click_camera { 0.1 } else { 3.0 }
            && area.width > 0.0
            && area.height > 0.0
        {
            if click_camera {
                // Recordly groups clicks within 2.5 seconds; the shot reaches full zoom on
                // its first click and holds through every click that joins the cluster.
                let mut last_click = None;
                for sample in samples
                    .iter()
                    .filter(|sample| sample.click && inside_capture(**sample, area))
                {
                    let time = sample.elapsed.as_secs_f64();
                    if time > duration {
                        continue;
                    }
                    if let (Some(previous), Some(previous_click)) = (shots.last_mut(), last_click)
                        && time - previous_click <= CLICK_CLUSTER_SECONDS
                    {
                        // Nearby activity re-arms the dwell; it never leaves the zoomed shot.
                        previous.release = time + ZOOM_HOLD_SECONDS;
                        previous.end = previous.release + ZOOM_OUT_SECONDS;
                    } else {
                        let (x, y) = source_position(
                            *sample,
                            area,
                            geometry.source_width,
                            geometry.source_height,
                        );
                        shots.push(Shot {
                            start: (time - ZOOM_IN_SECONDS).max(0.0),
                            peak: time,
                            release: time + ZOOM_HOLD_SECONDS,
                            end: time + ZOOM_HOLD_SECONDS + ZOOM_OUT_SECONDS,
                            x: x + geometry.padding,
                            y: y + geometry.padding,
                        });
                    }
                    last_click = Some(time);
                }
            } else {
                let mut settled: Option<CursorSample> = None;
                let mut used = false;
                for sample in samples {
                    if !inside_capture(*sample, area) {
                        settled = None;
                        used = false;
                        continue;
                    }
                    let anchor = *settled.get_or_insert(*sample);
                    let distance = ((sample.x - anchor.x) / area.width)
                        .hypot((sample.y - anchor.y) / area.height);
                    if distance > 0.02 {
                        settled = Some(*sample);
                        used = false;
                        continue;
                    }
                    if used || sample.elapsed.saturating_sub(anchor.elapsed).as_secs_f64() < 0.45 {
                        continue;
                    }
                    used = true;
                    let start = (anchor.elapsed.as_secs_f64() - 0.5).max(0.8);
                    let end = (start + 4.4).min(duration - 0.35);
                    let (x, y) = source_position(
                        anchor,
                        area,
                        geometry.source_width,
                        geometry.source_height,
                    );
                    if let Some(previous) = shots.last_mut()
                        && start <= previous.end
                    {
                        // Nearby activity extends the zoom, never queues a stale focus behind it.
                        previous.end = previous.end.max(end);
                        previous.release = end - EASE_SECONDS;
                        continue;
                    }
                    if end - start >= EASE_SECONDS * 2.0 + 0.4 {
                        shots.push(Shot {
                            start,
                            peak: start + EASE_SECONDS,
                            release: end - EASE_SECONDS,
                            end,
                            x: x + geometry.padding,
                            y: y + geometry.padding,
                        });
                    }
                }
            }
        }
        if shots.is_empty() {
            return Self {
                geometry,
                frames: Vec::new(),
            };
        }

        // Recordly's follow behavior: a 25% inset safe zone, bounded focus,
        // near-critically damped movement, and frozen focus during zoom-out.
        // Bake at export FPS so seeking and preview playback follow the same path.
        // This costs ~5 MiB/hour; use sparse checkpoints if long takes need less memory.
        let mut frames = Vec::new();
        let mut shot_index = 0;
        let mut sample_index = 0;
        let mut focus = (overview.x, overview.y);
        let mut position = focus;
        let mut velocity = (0.0, 0.0);
        let mut active = false;
        let half_width = geometry.width as f64 / (2.0 * ZOOM);
        let half_height = geometry.height as f64 / (2.0 * ZOOM);
        for frame in 0..=(duration * FPS as f64).ceil() as usize {
            let time = frame as f64 / FPS as f64;
            while shot_index < shots.len() && time >= shots[shot_index].end {
                shot_index += 1;
                active = false;
            }
            let Some(shot) = shots.get(shot_index).filter(|shot| time >= shot.start) else {
                frames.push(overview);
                continue;
            };
            if !active {
                focus = (
                    shot.x.clamp(half_width, geometry.width as f64 - half_width),
                    shot.y
                        .clamp(half_height, geometry.height as f64 - half_height),
                );
                position = focus;
                velocity = (0.0, 0.0);
                active = true;
            }
            // The pointer drives the focus through the approach and the dwell, then the
            // camera freezes for the whole release.
            if time < shot.release
                && let Some(pointer) =
                    interpolated_cursor(samples, Duration::from_secs_f64(time), &mut sample_index)
                        .filter(|pointer| inside_capture(*pointer, area))
            {
                let (x, y) =
                    source_position(pointer, area, geometry.source_width, geometry.source_height);
                let x = x + geometry.padding;
                let y = y + geometry.padding;
                if (x - focus.0).abs() > half_width * 0.5 {
                    focus.0 = x.clamp(half_width, geometry.width as f64 - half_width);
                }
                if (y - focus.1).abs() > half_height * 0.5 {
                    focus.1 = y.clamp(half_height, geometry.height as f64 - half_height);
                }
            }
            spring_step(&mut position.0, &mut velocity.0, focus.0);
            spring_step(&mut position.1, &mut velocity.1, focus.1);
            position.0 = position
                .0
                .clamp(half_width, geometry.width as f64 - half_width);
            position.1 = position
                .1
                .clamp(half_height, geometry.height as f64 - half_height);
            // A click before the first frame leaves no room to approach; floor the
            // approach ramp at one frame.
            let frame = 1.0 / FPS as f64;
            let transition = (shot.peak - shot.start).max(frame);
            let settle = (shot.end - shot.release).max(frame);
            let amount = ease((time - shot.start) / transition)
                .min(ease((shot.end - time) / settle));
            frames.push(Camera {
                zoom: 1.0 + (ZOOM - 1.0) * amount,
                x: overview.x + (position.0 - overview.x) * amount,
                y: overview.y + (position.1 - overview.y) * amount,
            });
        }
        Self { geometry, frames }
    }

    pub fn at(&self, time: f64) -> Camera {
        if !time.is_finite() || time < 0.0 {
            return Camera::overview(self.geometry);
        }
        let frame = time * FPS as f64;
        let Some(a) = self.frames.get(frame.floor() as usize) else {
            return Camera::overview(self.geometry);
        };
        let b = self.frames.get(frame.floor() as usize + 1).unwrap_or(a);
        let amount = frame.fract();
        Camera {
            zoom: a.zoom + (b.zoom - a.zoom) * amount,
            x: a.x + (b.x - a.x) * amount,
            y: a.y + (b.y - a.y) * amount,
        }
    }
}

fn inside_capture(sample: CursorSample, area: CaptureArea) -> bool {
    sample.x.is_finite()
        && sample.y.is_finite()
        && sample.x >= area.x
        && sample.x < area.x + area.width
        && sample.y >= area.y
        && sample.y < area.y + area.height
}

fn spring_step(position: &mut f64, velocity: &mut f64, target: f64) {
    // Exact solution of x'' + 21x' + 100(x - target) = 0, at 60 Hz.
    let slow = (-21.0 + 41.0_f64.sqrt()) / 2.0;
    let fast = (-21.0 - 41.0_f64.sqrt()) / 2.0;
    let delta = *position - target;
    let a = (*velocity - fast * delta) / (slow - fast);
    let b = delta - a;
    let a = a * (slow / FPS as f64).exp();
    let b = b * (fast / FPS as f64).exp();
    *position = target + a + b;
    *velocity = slow * a + fast * b;
}

pub(super) fn rounded(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, PI * 1.5);
    cr.close_path();
}

pub(super) fn backdrop(geometry: Geometry, style: Style) -> Result<ImageSurface, String> {
    let surface =
        ImageSurface::create(Format::Rgb24, geometry.width as i32, geometry.height as i32)
            .map_err(|e| e.to_string())?;
    let cr = Context::new(&surface).map_err(|e| e.to_string())?;
    let (_, top, bottom) = BACKGROUNDS[style.background];
    let gradient =
        cairo::LinearGradient::new(0.0, 0.0, geometry.width as f64, geometry.height as f64);
    for (offset, color) in [(0.0, top), (1.0, bottom)] {
        gradient.add_color_stop_rgb(
            offset,
            color[0] as f64 / 255.0,
            color[1] as f64 / 255.0,
            color[2] as f64 / 255.0,
        );
    }
    cr.set_source(&gradient).map_err(|e| e.to_string())?;
    cr.paint().map_err(|e| e.to_string())?;
    let radius = geometry.source_height as f64 * 0.012;
    for i in (1..=24).rev() {
        let spread = i as f64 * geometry.padding / 40.0;
        rounded(
            &cr,
            geometry.padding - spread,
            geometry.padding - spread + geometry.padding * 0.1,
            geometry.source_width as f64 + spread * 2.0,
            geometry.source_height as f64 + spread * 2.0,
            radius + spread,
        );
        cr.set_source_rgba(0.02, 0.015, 0.03, 0.018);
        cr.fill().map_err(|e| e.to_string())?;
    }
    Ok(surface)
}

fn cursor(cr: &Context, x: f64, y: f64, size: f64) -> Result<(), cairo::Error> {
    cr.save()?;
    cr.translate(x, y);
    cr.scale(size, size);
    cr.move_to(0.0, 0.0);
    cr.line_to(0.0, 1.0);
    cr.line_to(0.25, 0.78);
    cr.line_to(0.46, 1.2);
    cr.line_to(0.65, 1.1);
    cr.line_to(0.44, 0.7);
    cr.line_to(0.8, 0.7);
    cr.close_path();
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.fill_preserve()?;
    cr.set_source_rgba(0.05, 0.05, 0.06, 0.95);
    cr.set_line_width(1.3 / size);
    cr.set_line_join(cairo::LineJoin::Round);
    cr.stroke()?;
    cr.restore()
}

// Preview and export call this exact compositor. Every part of the scene shares one camera transform.
pub(super) fn draw_scene(
    cr: &Context,
    source: &ImageSurface,
    background: &ImageSurface,
    geometry: Geometry,
    camera: Camera,
    pointer: Option<(f64, f64)>,
) -> Result<(), cairo::Error> {
    cr.save()?;
    cr.rectangle(0.0, 0.0, geometry.width as f64, geometry.height as f64);
    cr.clip();
    cr.translate(geometry.width as f64 / 2.0, geometry.height as f64 / 2.0);
    cr.scale(camera.zoom, camera.zoom);
    cr.translate(-camera.x, -camera.y);
    cr.set_source_surface(background, 0.0, 0.0)?;
    cr.paint()?;
    cr.save()?;
    rounded(
        cr,
        geometry.padding,
        geometry.padding,
        geometry.source_width as f64,
        geometry.source_height as f64,
        geometry.source_height as f64 * 0.012,
    );
    cr.clip();
    cr.set_source_surface(source, geometry.padding, geometry.padding)?;
    cr.source().set_filter(cairo::Filter::Best);
    cr.paint()?;
    if let Some((x, y)) = pointer {
        cursor(
            cr,
            geometry.padding + x,
            geometry.padding + y,
            (geometry.source_height as f64 * 0.026).max(12.0),
        )?;
    }
    cr.restore()?;
    cr.restore()
}

fn drain_errors(child: &mut Child) -> thread::JoinHandle<String> {
    let stderr = child.stderr.take().unwrap();
    thread::spawn(move || {
        let mut last = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            last = line;
        }
        last
    })
}

pub(super) fn render_cinematic(
    source: &Path,
    output: &Path,
    area: CaptureArea,
    samples: &[CursorSample],
    style: Style,
    progress: impl Fn(f64),
) -> Result<(), String> {
    let info = video_info(source)?;
    let geometry = Geometry::new(info.width, info.height);
    let plan = CameraPlan::new(
        geometry,
        area,
        samples,
        info.duration,
        style.follow_cursor,
        style.click_camera,
    );
    let background = backdrop(geometry, style)?;
    let mut source_surface =
        ImageSurface::create(Format::Rgb24, info.width as i32, info.height as i32)
            .map_err(|e| e.to_string())?;
    let mut canvas =
        ImageSurface::create(Format::Rgb24, geometry.width as i32, geometry.height as i32)
            .map_err(|e| e.to_string())?;
    let temporary = output.with_extension("rendering.mp4");
    let mut decoder = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(source)
        .args([
            "-map", "0:v:0", "-vf", "fps=60", "-f", "rawvideo", "-pix_fmt", "bgr0", "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Cannot decode capture: {e}"))?;
    let decoder_errors = drain_errors(&mut decoder);
    let mut decoded = decoder.stdout.take().unwrap();
    let encoder = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-nostats",
            "-f",
            "rawvideo",
            "-pixel_format",
            "bgr0",
            "-video_size",
            &format!("{}x{}", geometry.width, geometry.height),
            "-framerate",
            "60",
            "-i",
            "pipe:0",
            "-i",
        ])
        .arg(source)
        .args([
            "-map",
            "0:v:0",
            "-map",
            "1:a?",
            "-c:v",
            "libx264",
            "-crf",
            "12",
            "-preset",
            "medium",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "256k",
            "-movflags",
            "+faststart",
            "-t",
        ])
        .arg(info.duration.to_string())
        .arg(&temporary)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut encoder = match encoder {
        Ok(child) => child,
        Err(e) => {
            let _ = decoder.kill();
            let _ = decoder.wait();
            let _ = decoder_errors.join();
            return Err(e.to_string());
        }
    };
    let encoder_errors = drain_errors(&mut encoder);
    let mut input = encoder.stdin.take().unwrap();
    let mut frame_index = 0;
    let mut sample_index = 0;
    let result = (|| -> Result<(), String> {
        loop {
            {
                let mut data = source_surface.data().map_err(|e| e.to_string())?;
                if decoded.read(&mut data[..1]).map_err(|e| e.to_string())? == 0 {
                    break;
                }
                decoded
                    .read_exact(&mut data[1..])
                    .map_err(|e| format!("Incomplete source frame: {e}"))?;
            }
            source_surface.mark_dirty();
            let time = frame_index as f64 / FPS as f64;
            let pointer =
                interpolated_cursor(samples, Duration::from_secs_f64(time), &mut sample_index)
                    .filter(|s| {
                        s.x >= area.x
                            && s.x < area.x + area.width
                            && s.y >= area.y
                            && s.y < area.y + area.height
                    })
                    .map(|s| source_position(s, area, info.width, info.height));
            {
                let cr = Context::new(&canvas).map_err(|e| e.to_string())?;
                draw_scene(
                    &cr,
                    &source_surface,
                    &background,
                    geometry,
                    plan.at(time),
                    pointer,
                )
                .map_err(|e| e.to_string())?;
            }
            canvas.flush();
            input
                .write_all(&canvas.data().map_err(|e| e.to_string())?)
                .map_err(|e| format!("Cannot encode frame: {e}"))?;
            if frame_index % FPS as usize == 0 {
                progress((time / info.duration).min(0.98));
            }
            frame_index += 1;
        }
        if frame_index == 0 {
            return Err("Capture contains no decoded frames.".into());
        }
        progress(0.99);
        Ok(())
    })();
    drop(input);
    drop(decoded);
    if result.is_err() {
        let _ = decoder.kill();
        let _ = encoder.kill();
    }
    let decoder_status = decoder.wait().map_err(|e| e.to_string());
    let encoder_status = encoder.wait().map_err(|e| e.to_string());
    let decoder_error = decoder_errors.join().unwrap_or_default();
    let encoder_error = encoder_errors.join().unwrap_or_default();
    let outcome = result.and_then(|_| {
        if !decoder_status?.success() {
            return Err(format!("Decoding failed: {decoder_error}"));
        }
        if !encoder_status?.success() {
            return Err(format!("Encoding failed: {encoder_error}"));
        }
        // Validate the playable file before replacing the destination or discarding capture data.
        let finished = video_info(&temporary)?;
        if finished.width != geometry.width
            || finished.height != geometry.height
            || (finished.duration - info.duration).abs() > 0.1
        {
            return Err(
                "Export verification failed. Temporary capture retained for recovery.".into(),
            );
        }
        fs::rename(&temporary, output).map_err(|e| e.to_string())
    });
    if outcome.is_err() {
        let _ = fs::remove_file(temporary);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_trigger_and_cluster_zooms_without_requiring_a_pause() {
        let geometry = Geometry::new(1920, 1080);
        let area = CaptureArea {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let mut samples: Vec<_> = (0..=400)
            .map(|i| CursorSample {
                elapsed: Duration::from_millis(i * 25),
                x: 200.0 + (i % 20) as f64 * 75.0,
                y: 540.0,
                click: matches!(i, 80 | 160 | 320),
            })
            .collect();
        let plan = CameraPlan::new(geometry, area, &samples, 10.0, true, true);
        assert_eq!(plan.at(1.0).zoom, 1.0);
        assert_eq!(
            plan.at(3.0).zoom,
            ZOOM,
            "Nearby clicks must share a continuous zoom"
        );
        assert_eq!(
            plan.at(6.0).zoom,
            1.0,
            "Separated clicks must form separate regions"
        );
        assert_eq!(
            plan.at(8.0).zoom,
            ZOOM,
            "An isolated click must reach full zoom"
        );
        assert_eq!(
            plan.at(9.5).zoom,
            1.0,
            "An isolated click must be fully released 1.5s after the click"
        );
        assert!(
            CameraPlan::new(geometry, area, &samples, 10.0, false, true)
                .frames
                .is_empty()
        );
        for sample in &mut samples {
            sample.click = false;
            sample.x = 960.0;
        }
        assert!(
            CameraPlan::new(geometry, area, &samples, 10.0, true, true)
                .frames
                .is_empty(),
            "An authorized take with no clicks must not silently fall back to pauses"
        );
        assert!(
            !CameraPlan::new(geometry, area, &samples, 10.0, true, false)
                .frames
                .is_empty()
        );
        samples[40].click = true;
        assert_eq!(
            CameraPlan::new(geometry, area, &samples, 2.0, true, true)
                .at(1.0)
                .zoom,
            ZOOM
        );
        samples[40].x = -100.0;
        assert!(
            CameraPlan::new(geometry, area, &samples, 2.0, true, true)
                .frames
                .is_empty(),
            "Clicks on another monitor must not trigger zooms"
        );
    }

    #[test]
    fn a_click_dwells_at_full_zoom_before_easing_back() {
        let geometry = Geometry::new(1920, 1080);
        let area = CaptureArea {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let samples: Vec<_> = (0..=400)
            .map(|i| CursorSample {
                elapsed: Duration::from_millis(i * 25),
                x: 960.0,
                y: 540.0,
                click: i == 200,
            })
            .collect();
        let plan = CameraPlan::new(geometry, area, &samples, 10.0, true, true);
        let zoom = |time: f64| plan.at(time).zoom;
        // The whole point of the three-phase move: full zoom is a plateau, not a spike.
        for time in [5.0, 5.2, 5.4, 5.6] {
            assert!(
                (zoom(time) - ZOOM).abs() < 1e-9,
                "Zoom left full at {time}s: {}",
                zoom(time)
            );
        }
        assert!(zoom(4.5) < ZOOM - 0.1, "Must still be approaching the click");
        assert!(
            zoom(6.0) > 1.0 + (ZOOM - 1.0) * 0.4,
            "Must still be releasing 1s after the click: {}",
            zoom(6.0)
        );
        assert!(
            (zoom(6.4) - 1.0).abs() < 1e-6,
            "The release must land on the overview: {}",
            zoom(6.4)
        );
        for frame in 1..=(0.5 * FPS as f64) as usize {
            let time = 4.5 + frame as f64 / FPS as f64;
            assert!(zoom(time) > zoom(time - 1.0 / FPS as f64), "Approach stalls at {time}");
        }
        for frame in 1..=(0.8 * FPS as f64) as usize {
            let time = 5.6 + frame as f64 / FPS as f64;
            assert!(zoom(time) < zoom(time - 1.0 / FPS as f64), "Release stalls at {time}");
        }
        for frame in 1..=(1.9 * FPS as f64) as usize {
            let time = 4.5 + frame as f64 / FPS as f64;
            let previous = plan.at(time - 1.0 / FPS as f64);
            assert!(
                (previous.zoom - plan.at(time).zoom).abs() < 0.05,
                "Zoom jumps at {time}"
            );
        }
    }

    #[test]
    fn a_burst_of_clicks_in_different_areas_stays_zoomed_and_follows() {
        let geometry = Geometry::new(1920, 1080);
        let area = CaptureArea {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let samples: Vec<_> = (0..=320)
            .map(|i| CursorSample {
                elapsed: Duration::from_millis(i * 25),
                x: if i < 100 { 400.0 } else { 1600.0 },
                y: 540.0,
                click: (40..=200).contains(&i) && (i - 40) % 32 == 0,
            })
            .collect();
        let plan = CameraPlan::new(geometry, area, &samples, 8.0, true, true);
        for time in [1.0, 1.5, 2.5, 3.5, 4.5, 5.5] {
            assert!(
                (plan.at(time).zoom - ZOOM).abs() < 1e-9,
                "The burst dropped out of the zoom at {time}s: {}",
                plan.at(time).zoom
            );
        }
        assert!(
            plan.at(4.0).x > plan.at(1.5).x + 300.0,
            "The camera must follow the clicks across the screen: {:?} -> {:?}",
            plan.at(1.5),
            plan.at(4.0)
        );
        assert!((plan.at(6.5).zoom - 1.0).abs() < 1e-6, "Must release after the last click");
    }

    #[test]
    fn camera_reaches_the_next_target_before_the_pointer_leaves() {
        let geometry = Geometry::new(1920, 1080);
        let area = CaptureArea {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let samples: Vec<_> = (0..400)
            .map(|i| CursorSample {
                click: false,
                elapsed: Duration::from_millis(i * 25),
                x: if i < 80 {
                    200.0
                } else if i < 160 {
                    1720.0
                } else {
                    200.0
                },
                y: 540.0,
            })
            .collect();
        let plan = CameraPlan::new(geometry, area, &samples, 10.0, true, false);
        let camera = plan.at(2.6);
        let target = geometry.padding + 1720.0;
        let screen_x = (target - camera.x) * camera.zoom + geometry.width as f64 / 2.0;
        assert!(
            screen_x < geometry.width as f64 * 0.95,
            "Camera is late: target is at {screen_x:.1}px on a {}px frame at 2.6s",
            geometry.width
        );
        assert!(
            camera.x > geometry.width as f64 / 2.0,
            "Camera still focuses the previous target at 2.6s: {camera:?}"
        );
    }

    #[test]
    fn follow_holds_inside_safe_zone_and_tracks_without_waiting_for_a_pause() {
        let geometry = Geometry::new(1920, 1080);
        let area = CaptureArea {
            x: -1920.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let samples: Vec<_> = (0..240)
            .map(|i| CursorSample {
                click: false,
                elapsed: Duration::from_millis(i * 25),
                x: -960.0
                    + if i < 80 {
                        0.0
                    } else if i < 120 {
                        80.0
                    } else {
                        ((i - 120) as f64 * 20.0).min(850.0)
                    },
                y: 540.0,
            })
            .collect();
        let plan = CameraPlan::new(geometry, area, &samples, 6.0, true, false);
        assert_eq!(
            plan.at(2.0).x,
            plan.at(2.9).x,
            "Small moves must not move the camera"
        );
        assert!(
            plan.at(3.8).x > plan.at(3.0).x + 150.0,
            "Camera must follow during movement, before a new pause qualifies"
        );
        let before_seek = plan.at(3.8);
        let _ = plan.at(1.0);
        assert_eq!(plan.at(3.8).x, before_seek.x);
        for i in 1..360 {
            let a = plan.at((i - 1) as f64 / 60.0);
            let b = plan.at(i as f64 / 60.0);
            assert!((a.x - b.x).abs() < 45.0, "Pan jumps at {i}: {a:?} -> {b:?}");
            assert!(b.x >= geometry.width as f64 / (2.0 * b.zoom) - 0.001);
            assert!(b.x <= geometry.width as f64 * (1.0 - 1.0 / (2.0 * b.zoom)) + 0.001);
        }
        for (duration, enabled) in [(2.0, true), (6.0, false)] {
            let plan = CameraPlan::new(geometry, area, &samples, duration, enabled, false);
            assert_eq!(plan.at(1.5).zoom, 1.0);
        }
    }

    #[test]
    fn zoom_out_ignores_new_pointer_movement() {
        let geometry = Geometry::new(1920, 1080);
        let area = CaptureArea {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let still: Vec<_> = (0..240)
            .map(|i| CursorSample {
                click: false,
                elapsed: Duration::from_millis(i * 25),
                x: 400.0,
                y: 400.0,
            })
            .collect();
        let mut moving = still.clone();
        for (i, sample) in moving.iter_mut().enumerate().skip(172) {
            sample.x = 200.0 + (i % 20) as f64 * 75.0;
        }
        let a = CameraPlan::new(geometry, area, &still, 6.0, true, false);
        let b = CameraPlan::new(geometry, area, &moving, 6.0, true, false);
        for i in 260..360 {
            let t = i as f64 / 60.0;
            assert_eq!(a.at(t).x, b.at(t).x, "Zoom-out chases the cursor at {t}");
        }
    }

    #[test]
    fn camera_has_a_still_intro_a_hold_and_a_still_outro() {
        let geometry = Geometry::new(1920, 1080);
        let area = CaptureArea {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let samples: Vec<_> = (0..240)
            .map(|i| CursorSample {
                click: false,
                elapsed: Duration::from_millis(i * 25),
                x: 1200.0,
                y: 700.0,
            })
            .collect();
        let plan = CameraPlan::new(geometry, area, &samples, 6.0, true, false);
        assert_eq!(plan.at(0.0).zoom, 1.0);
        assert_eq!(plan.at(0.7).zoom, 1.0);
        assert_eq!(plan.at(3.0).zoom, ZOOM);
        assert_eq!(plan.at(3.2).zoom, ZOOM);
        assert_eq!(plan.at(5.9).zoom, 1.0);
        for i in 1..360 {
            let a = plan.at((i - 1) as f64 / 60.0);
            let b = plan.at(i as f64 / 60.0);
            assert!((a.zoom - b.zoom).abs() < 0.021, "Zoom jumps at frame {i}");
            assert!((a.x - b.x).abs() < 12.0, "Pan jumps at frame {i}");
        }
    }

    #[test]
    fn camera_does_not_chase_a_moving_pointer_or_bounce_during_a_pause() {
        let geometry = Geometry::new(1920, 1080);
        let area = CaptureArea {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let samples: Vec<_> = (0..400)
            .map(|i| CursorSample {
                click: false,
                elapsed: Duration::from_millis(i * 25),
                x: 200.0 + (i % 20) as f64 * 75.0,
                y: 300.0,
            })
            .collect();
        assert!(
            CameraPlan::new(geometry, area, &samples, 10.0, true, false)
                .frames
                .is_empty()
        );
        let still: Vec<_> = (0..1200)
            .map(|i| CursorSample {
                click: false,
                elapsed: Duration::from_millis(i * 25),
                x: 400.0,
                y: 300.0,
            })
            .collect();
        let plan = CameraPlan::new(geometry, area, &still, 30.0, true, false);
        assert_eq!(plan.at(3.0).zoom, ZOOM);
        assert_eq!(plan.at(10.0).zoom, 1.0);
        assert_eq!(plan.at(20.0).zoom, 1.0);
    }
}
