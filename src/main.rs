use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use gtk::{glib, prelude::*};
use serde::Deserialize;

const APP_ID: &str = "org.omarchy.OmaScreenema";
const FPS: u32 = 60;
const CURSOR_SAMPLE_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, PartialEq)]
struct CaptureArea {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Clone, Copy, Debug)]
struct CursorSample {
    click: bool,
    elapsed: Duration,
    x: f64,
    y: f64,
}

struct ActiveRecording {
    child: Child,
    cursor_running: Arc<AtomicBool>,
    cursor_sampler: thread::JoinHandle<Vec<CursorSample>>,
    source: PathBuf,
    area: CaptureArea,
    started: Instant,
    cursor_epoch: i64,
    clicks: Option<clicks::Session>,
}

#[derive(Deserialize)]
struct HyprMonitor {
    name: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    focused: bool,
    #[serde(default = "default_scale")]
    scale: f64,
    #[serde(default)]
    transform: u32,
}

#[derive(Deserialize)]
struct CursorPosition {
    x: f64,
    y: f64,
}

struct FocusedMonitor {
    name: String,
    area: CaptureArea,
}

struct VideoInfo {
    width: u32,
    height: u32,
    duration: f64,
}

fn default_scale() -> f64 {
    1.0
}

fn video_directory() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
    PathBuf::from(home).join("Videos").join("OmaScreenema")
}

fn next_recording_path() -> std::io::Result<PathBuf> {
    fs::create_dir_all(video_directory())?;
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".cache")
        });
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let directory = cache.join("oma-screenema").join(format!("take-{stamp}"));
    fs::create_dir_all(&directory)?;
    Ok(directory.join("capture.mp4"))
}

fn cinematic_path(source: &Path) -> PathBuf {
    let take = source
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("take-"))
        .unwrap_or("recording");
    video_directory().join(format!("screenema-{take}.mp4"))
}

fn cleanup_capture(source: &Path) -> std::io::Result<()> {
    // Only these files belong to the completed take. Never sweep the recordings folder.
    for path in [
        source.to_path_buf(),
        source.with_extension("log"),
        source.with_extension("mp4.ts"),
    ] {
        match fs::remove_file(path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
    }
    if let Some(directory) = source.parent() {
        fs::remove_dir(directory)?;
    }
    Ok(())
}

fn audio_source(system_audio: bool, microphone: bool) -> Option<&'static str> {
    match (system_audio, microphone) {
        (true, true) => Some("default_output|default_input"),
        (true, false) => Some("default_output"),
        (false, true) => Some("default_input"),
        (false, false) => None,
    }
}

fn elapsed_label(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    format!("● REC {:02}:{:02}", seconds / 60, seconds % 60)
}

fn parse_focused_monitor(json: &str) -> Result<FocusedMonitor, String> {
    let monitors: Vec<HyprMonitor> =
        serde_json::from_str(json).map_err(|error| error.to_string())?;
    let monitor = monitors
        .into_iter()
        .find(|monitor| monitor.focused)
        .ok_or_else(|| "Hyprland has no focused monitor.".to_owned())?;

    if !monitor.scale.is_finite()
        || monitor.scale <= 0.0
        || monitor.width <= 0.0
        || monitor.height <= 0.0
    {
        return Err("Monitor has invalid dimensions or scale.".into());
    }
    let (width, height) = if monitor.transform % 2 == 1 {
        (monitor.height, monitor.width)
    } else {
        (monitor.width, monitor.height)
    };
    Ok(FocusedMonitor {
        name: monitor.name,
        area: CaptureArea {
            x: monitor.x,
            y: monitor.y,
            width: width / monitor.scale,
            height: height / monitor.scale,
        },
    })
}

fn focused_monitor() -> Result<FocusedMonitor, String> {
    let output = Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
        .map_err(|error| format!("Could not ask Hyprland for monitor details: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    parse_focused_monitor(&String::from_utf8_lossy(&output.stdout))
}

fn stop_recorder(child: &mut Child) -> std::io::Result<()> {
    Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(
                    "Recorder did not accept the stop signal.",
                ))
            }
        })
}

fn sample_cursor(_area: CaptureArea, running: Arc<AtomicBool>, epoch: i64) -> Vec<CursorSample> {
    let mut samples = Vec::new();

    while running.load(Ordering::Relaxed) {
        if let Ok(output) = Command::new("hyprctl").args(["cursorpos", "-j"]).output()
            && let Ok(position) = serde_json::from_slice::<CursorPosition>(&output.stdout)
        {
            samples.push(CursorSample {
                click: false,
                elapsed: Duration::from_micros((glib::monotonic_time() - epoch).max(0) as u64),
                x: position.x,
                y: position.y,
            });
        }
        thread::sleep(CURSOR_SAMPLE_INTERVAL);
    }

    samples
}

fn cursor_sampler(
    area: CaptureArea,
    running: Arc<AtomicBool>,
    epoch: i64,
) -> thread::JoinHandle<Vec<CursorSample>> {
    thread::spawn(move || sample_cursor(area, running, epoch))
}

fn align_cursor_samples(samples: &mut Vec<CursorSample>, offset: Duration) {
    let anchor = samples
        .iter()
        .rev()
        .find(|sample| sample.elapsed <= offset)
        .copied();
    samples.retain(|sample| sample.elapsed > offset);
    for sample in samples.iter_mut() {
        sample.elapsed -= offset;
    }
    if let Some(mut anchor) = anchor {
        anchor.elapsed = Duration::ZERO;
        samples.insert(0, anchor);
    }
}

fn synchronize_cursor(
    source: &Path,
    epoch: i64,
    samples: &mut Vec<CursorSample>,
) -> Result<i64, String> {
    let timestamp_path = source.with_extension("mp4.ts");
    let timestamps = fs::read_to_string(&timestamp_path)
        .map_err(|e| format!("Cannot synchronize cursor: {e}"))?;
    let first_frame = timestamps
        .lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().next())
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value >= epoch)
        .ok_or_else(|| "Recorder returned an invalid first-frame timestamp.".to_owned())?;
    align_cursor_samples(samples, Duration::from_micros((first_frame - epoch) as u64));
    Ok(first_frame)
}

fn video_info(source: &Path) -> Result<VideoInfo, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,duration",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(source)
        .output()
        .map_err(|error| format!("Could not inspect recording: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }

    let mut width = None;
    let mut height = None;
    let mut duration = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(value) = line.strip_prefix("width=") {
            width = value.parse().ok();
        } else if let Some(value) = line.strip_prefix("height=") {
            height = value.parse().ok();
        } else if let Some(value) = line.strip_prefix("duration=") {
            duration = value.parse().ok();
        }
    }

    match (width, height, duration) {
        (Some(width), Some(height), Some(duration))
            if duration > 0.0 && f64::is_finite(duration) && width >= 16 && height >= 16 =>
        {
            Ok(VideoInfo {
                width,
                height,
                duration,
            })
        }
        _ => Err("Recording has no readable video stream.".to_owned()),
    }
}

fn source_position(sample: CursorSample, area: CaptureArea, width: u32, height: u32) -> (f64, f64) {
    (
        ((sample.x - area.x) / area.width).clamp(0.0, 1.0) * width as f64,
        ((sample.y - area.y) / area.height).clamp(0.0, 1.0) * height as f64,
    )
}

fn interpolated_cursor(
    samples: &[CursorSample],
    frame_time: Duration,
    index: &mut usize,
) -> Option<CursorSample> {
    let first = *samples.first()?;
    while *index + 1 < samples.len() && samples[*index + 1].elapsed <= frame_time {
        *index += 1;
    }
    let current = samples.get(*index).copied().unwrap_or(first);
    let Some(next) = samples.get(*index + 1).copied() else {
        return Some(current);
    };
    let span = next.elapsed.saturating_sub(current.elapsed).as_secs_f64();
    let ratio = if span == 0.0 {
        0.0
    } else {
        ((frame_time.as_secs_f64() - current.elapsed.as_secs_f64()) / span).clamp(0.0, 1.0)
    };
    Some(CursorSample {
        click: false,
        elapsed: frame_time,
        x: current.x + (next.x - current.x) * ratio,
        y: current.y + (next.y - current.y) * ratio,
    })
}

#[derive(Clone, Copy, Default)]
struct Style {
    background: usize,
    follow_cursor: bool,
    click_camera: bool,
}

const BACKGROUNDS: [(&str, [u8; 3], [u8; 3]); 12] = [
    ("Dusk", [105, 100, 186], [231, 161, 132]),
    ("Ocean", [25, 70, 100], [89, 190, 176]),
    ("Graphite", [38, 42, 51], [104, 110, 125]),
    ("Aurora", [34, 68, 92], [129, 196, 155]),
    ("Ember", [104, 40, 66], [240, 153, 91]),
    ("Iris", [57, 43, 111], [166, 151, 219]),
    ("Rose", [132, 65, 102], [239, 174, 180]),
    ("Sand", [151, 115, 81], [234, 213, 173]),
    ("Forest", [24, 57, 48], [111, 150, 101]),
    ("Glacier", [57, 104, 141], [174, 218, 232]),
    ("Midnight", [17, 24, 48], [66, 73, 120]),
    ("Pearl", [190, 196, 205], [243, 235, 224]),
];

mod clicks;
mod scene;
use scene::render_cinematic;

mod ui;

fn main() -> glib::ExitCode {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "--click-helper") {
        if args.len() != 2 {
            return glib::ExitCode::FAILURE;
        }
        return match clicks::helper() {
            Ok(()) => glib::ExitCode::SUCCESS,
            Err(error) => {
                let _ = writeln!(std::io::stdout(), "ERROR {error}");
                glib::ExitCode::FAILURE
            }
        };
    }
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| {
        if let Some(window) = app.active_window() {
            window.present();
        } else {
            ui::build_ui(app);
        }
    });
    app.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_focused_monitor_and_its_global_area() {
        let monitor = parse_focused_monitor(
            r#"[{"name":"left","x":-1920,"y":0,"width":1920,"height":1080,"focused":false},{"name":"main","x":0,"y":0,"width":2560,"height":1440,"focused":true}]"#,
        )
        .unwrap();
        assert_eq!(monitor.name, "main");
        assert_eq!(monitor.area.width, 2560.0);
    }

    #[test]
    fn interpolates_cursor_movement_between_samples() {
        let samples = [
            CursorSample {
                click: false,
                elapsed: Duration::ZERO,
                x: 0.0,
                y: 0.0,
            },
            CursorSample {
                click: false,
                elapsed: Duration::from_secs(1),
                x: 100.0,
                y: 50.0,
            },
        ];
        let mut index = 0;
        let cursor = interpolated_cursor(&samples, Duration::from_millis(500), &mut index).unwrap();
        assert_eq!((cursor.x, cursor.y), (50.0, 25.0));
    }

    #[test]
    fn chooses_the_requested_audio_tracks() {
        assert_eq!(
            audio_source(true, true),
            Some("default_output|default_input")
        );
        assert_eq!(audio_source(true, false), Some("default_output"));
        assert_eq!(audio_source(false, true), Some("default_input"));
        assert_eq!(audio_source(false, false), None);
    }

    #[test]
    fn formats_recording_elapsed_time() {
        assert_eq!(elapsed_label(Duration::from_secs(65)), "● REC 01:05");
    }

    #[test]
    fn aligns_cursor_to_first_frame_after_gpu_startup() {
        let mut samples = vec![
            CursorSample {
                click: false,
                elapsed: Duration::from_millis(100),
                x: 10.0,
                y: 20.0,
            },
            CursorSample {
                click: false,
                elapsed: Duration::from_millis(400),
                x: 30.0,
                y: 40.0,
            },
            CursorSample {
                click: false,
                elapsed: Duration::from_millis(600),
                x: 50.0,
                y: 60.0,
            },
        ];
        align_cursor_samples(&mut samples, Duration::from_millis(500));
        assert_eq!(samples.len(), 2);
        assert_eq!((samples[0].elapsed, samples[0].x), (Duration::ZERO, 30.0));
        assert_eq!(samples[1].elapsed, Duration::from_millis(100));
    }

    #[test]
    fn scales_and_rotates_monitor_coordinates() {
        let monitor = parse_focused_monitor(r#"[{"name":"portrait","x":-720,"y":0,"width":2560,"height":1440,"scale":2,"transform":1,"focused":true}]"#).unwrap();
        assert_eq!(
            monitor.area,
            CaptureArea {
                x: -720.0,
                y: 0.0,
                width: 720.0,
                height: 1280.0
            }
        );
        assert!(
            parse_focused_monitor(
                r#"[{"name":"bad","x":0,"y":0,"width":100,"height":100,"scale":0,"focused":true}]"#
            )
            .is_err()
        );
    }

    #[test]
    fn renders_a_cinematic_video_with_a_visible_cursor() {
        let root = std::env::temp_dir().join(format!(
            "oma-screenema-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let source = root.join("source.mp4");
        let output = root.join("cinematic.mp4");
        let created = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=0x172554:s=320x180:r=60",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "0.4",
                "-c:v",
                "libx264",
            ])
            .arg(&source)
            .output()
            .unwrap();
        assert!(created.status.success());

        let samples = [
            CursorSample {
                click: false,
                elapsed: Duration::ZERO,
                x: 80.0,
                y: 60.0,
            },
            CursorSample {
                click: false,
                elapsed: Duration::from_millis(120),
                x: 250.0,
                y: 125.0,
            },
        ];
        let area = CaptureArea {
            x: 0.0,
            y: 0.0,
            width: 320.0,
            height: 180.0,
        };
        for (background, (_, expected, _)) in BACKGROUNDS.iter().enumerate() {
            render_cinematic(
                &source,
                &output,
                area,
                &samples,
                Style {
                    background,
                    follow_cursor: background != 0,
                    click_camera: false,
                },
                |_| {},
            )
            .unwrap();
            assert!(output.exists());
            assert!(!output.with_extension("rendering.mp4").exists());
            assert!(!output.with_extension("ppm").exists());
            let probe = Command::new("ffprobe")
                .args(["-v", "error", "-show_streams", "-of", "json"])
                .arg(&output)
                .output()
                .unwrap();
            let metadata: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
            let streams = metadata["streams"].as_array().unwrap();
            let video = streams.iter().find(|s| s["codec_type"] == "video").unwrap();
            assert_eq!(video["r_frame_rate"], "60/1");
            assert_eq!(video["pix_fmt"], "yuv420p");
            assert_eq!(video["nb_frames"], "24");
            assert!(streams.iter().any(|s| s["codec_type"] == "audio"));
            let pixels = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&output)
                .args([
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgba",
                    "pipe:1",
                ])
                .output()
                .unwrap();
            assert!(pixels.status.success());
            assert!(
                pixels
                    .stdout
                    .chunks_exact(4)
                    .any(|p| p[0] > 240 && p[1] > 240)
            );
            // The top-left corner is the chosen backdrop, not the source's blue frame.
            for (channel, value) in expected.iter().enumerate() {
                assert!((pixels.stdout[channel] as i32 - *value as i32).abs() < 18);
            }
        }

        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod camera_regressions;
