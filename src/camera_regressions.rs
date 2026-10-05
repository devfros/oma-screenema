use super::*;

// A stationary, lossless green card makes the camera's edges measurable without a GUI.
fn fixture() -> (PathBuf, PathBuf, CaptureArea, Vec<CursorSample>) {
    let root = std::env::temp_dir().join(format!(
        "screenema-camera-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let source = root.join("source.mp4");
    let result = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=0x10e030:s=320x180:r=60",
            "-t",
            "6",
            "-c:v",
            "libx264rgb",
            "-crf",
            "0",
        ])
        .arg(&source)
        .output()
        .unwrap();
    assert!(result.status.success());
    let area = CaptureArea {
        x: 0.0,
        y: 0.0,
        width: 320.0,
        height: 180.0,
    };
    let samples = (0..=240)
        .map(|i| CursorSample {
            click: false,
            elapsed: Duration::from_millis(i * 25),
            x: 160.0,
            y: 90.0,
        })
        .collect();
    (root, source, area, samples)
}

fn green_width(path: &Path, time: &str) -> usize {
    let info = video_info(path).unwrap();
    let result = Command::new("ffmpeg")
        .args(["-v", "error", "-ss", time, "-i"])
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "pipe:1",
        ])
        .output()
        .unwrap();
    assert!(result.status.success());
    let row = &result.stdout[info.width as usize * (info.height as usize / 3) * 3..]
        [..info.width as usize * 3];
    row.chunks_exact(3)
        .filter(|p| p[1] > 170 && p[0] < 60 && p[2] < 90)
        .count()
}

#[test]
fn camera_moves_the_whole_card_and_preserves_native_pixels() {
    let (root, source, area, samples) = fixture();
    let output = root.join("finished.mp4");
    render_cinematic(
        &source,
        &output,
        area,
        &samples,
        Style {
            background: 2,
            follow_cursor: true,
            click_camera: false,
        },
        |_| {},
    )
    .unwrap();
    let overview = green_width(&output, "0");
    let close_up = green_width(&output, "3");
    let outro = green_width(&output, "5.95");
    assert!(
        overview >= 318,
        "Native 320px source was shrunk to {overview}px"
    );
    assert!(
        close_up > overview + 20,
        "Card stayed fixed: overview={overview}, close-up={close_up}"
    );
    assert!(
        outro.abs_diff(overview) < 4,
        "Camera must return to the overview before ending"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn exported_camera_zooms_at_the_click_and_not_at_an_idle_pause() {
    let (root, source, area, mut samples) = fixture();
    clicks::attach(&mut samples, &[4_000_000], 1_000_000);
    let output = root.join("click.mp4");
    render_cinematic(
        &source,
        &output,
        area,
        &samples,
        Style {
            background: 2,
            follow_cursor: true,
            click_camera: true,
        },
        |_| {},
    )
    .unwrap();
    let idle = green_width(&output, "2");
    let click = green_width(&output, "3");
    let dwell = green_width(&output, "3.4");
    let after = green_width(&output, "5");
    assert!(
        click > idle + 20,
        "Click did not zoom the exported frame: {idle} -> {click}"
    );
    assert!(
        dwell >= click,
        "The zoom reset immediately after the click: {click} -> {dwell}"
    );
    assert!(
        after.abs_diff(idle) < 4,
        "Camera did not leave the isolated click region"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn exported_camera_keeps_the_new_target_visible() {
    let (root, source, area, mut samples) = fixture();
    for sample in &mut samples {
        sample.x = if sample.elapsed.as_secs_f64() < 2.0 {
            32.0
        } else {
            288.0
        };
    }
    let output = root.join("follow.mp4");
    render_cinematic(
        &source,
        &output,
        area,
        &samples,
        Style {
            background: 0,
            follow_cursor: true,
            click_camera: false,
        },
        |_| {},
    )
    .unwrap();
    let frame = Command::new("ffmpeg")
        .args(["-v", "error", "-ss", "2.6", "-i"])
        .arg(&output)
        .args([
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "pipe:1",
        ])
        .output()
        .unwrap();
    assert!(frame.status.success());
    let cursor_pixels = frame
        .stdout
        .chunks_exact(3)
        .filter(|p| p.iter().all(|c| *c > 235))
        .count();
    assert!(
        cursor_pixels > 20,
        "The new target's white cursor is outside the exported frame: {cursor_pixels} pixels"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn fine_text_strokes_survive_export_without_resampling() {
    let root = std::env::temp_dir().join(format!(
        "screenema-detail-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let mut ppm = b"P6\n320 180\n255\n".to_vec();
    for y in 0..180 {
        for x in 0..320 {
            let value = if (70..110).contains(&y) && (60..260).contains(&x) && x % 2 == 0 {
                0
            } else {
                255
            };
            ppm.extend_from_slice(&[value, value, value]);
        }
    }
    fs::write(root.join("detail.ppm"), ppm).unwrap();
    let source = root.join("capture.mp4");
    let result = Command::new("ffmpeg")
        .args(["-v", "error", "-loop", "1", "-framerate", "60", "-i"])
        .arg(root.join("detail.ppm"))
        .args(["-t", "0.2", "-c:v", "libx264rgb", "-crf", "0"])
        .arg(&source)
        .output()
        .unwrap();
    assert!(result.status.success());
    let output = root.join("finished.mp4");
    let area = CaptureArea {
        x: 0.0,
        y: 0.0,
        width: 320.0,
        height: 180.0,
    };
    render_cinematic(&source, &output, area, &[], Style::default(), |_| {}).unwrap();
    let result = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&output)
        .args([
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "pipe:1",
        ])
        .output()
        .unwrap();
    assert!(result.status.success());
    let geometry = scene::Geometry::new(320, 180);
    let p = geometry.padding as usize;
    let mut error = 0u64;
    for y in 75..105 {
        for x in 65..255 {
            let actual = result.stdout[((y + p) * geometry.width as usize + x + p) * 3] as i32;
            let expected = if x % 2 == 0 { 0 } else { 255 };
            error += (actual - expected).unsigned_abs() as u64;
        }
    }
    let mean = error as f64 / (30.0 * 190.0);
    assert!(
        mean < 4.0,
        "One-pixel strokes lost detail: mean pixel error {mean:.2}/255"
    );
    println!("Native one-pixel detail: mean error {mean:.2}/255");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cleanup_removes_only_the_completed_takes_temporary_files() {
    let root = std::env::temp_dir().join(format!(
        "screenema-cleanup-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let capture = root.join("take").join("capture.mp4");
    fs::create_dir_all(capture.parent().unwrap()).unwrap();
    for path in [
        &capture,
        &capture.with_extension("log"),
        &capture.with_extension("mp4.ts"),
    ] {
        fs::write(path, b"temporary").unwrap();
    }
    let finished = root.join("finished.mp4");
    fs::write(&finished, b"verified export").unwrap();
    cleanup_capture(&capture).unwrap();
    assert!(!capture.parent().unwrap().exists());
    assert_eq!(fs::read(&finished).unwrap(), b"verified export");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_export_preserves_capture_and_existing_finished_video() {
    let root = std::env::temp_dir().join(format!(
        "screenema-failure-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let source = root.join("capture.mp4");
    let output = root.join("finished.mp4");
    fs::write(&source, b"invalid video").unwrap();
    fs::write(&output, b"previous export").unwrap();
    assert!(
        render_cinematic(
            &source,
            &output,
            CaptureArea {
                x: 0.0,
                y: 0.0,
                width: 320.0,
                height: 180.0
            },
            &[],
            Style::default(),
            |_| {}
        )
        .is_err()
    );
    assert_eq!(fs::read(source).unwrap(), b"invalid video");
    assert_eq!(fs::read(output).unwrap(), b"previous export");
    fs::remove_dir_all(root).unwrap();
}
