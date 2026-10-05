use super::*;
use std::io::{self, BufRead, BufReader, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};

const BTN_LEFT: usize = 0x110;
const BTN_TASK: usize = 0x117;

pub(super) enum Event {
    Ready,
    Press(i64),
    Error(String),
    Stopped,
    Closed,
}

pub(super) struct Session {
    child: Option<Child>,
    control: Option<std::process::ChildStdin>,
    reader: Option<thread::JoinHandle<()>>,
    events: mpsc::Receiver<Event>,
    pub ready: bool,
    pub error: Option<String>,
    timestamps: Vec<i64>,
    stopped: bool,
}

impl Session {
    pub fn authorize() -> Result<Self, String> {
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        let child = Command::new("pkexec")
            .arg("--disable-internal-agent")
            .arg(executable)
            .arg("--click-helper")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Cannot open system authorization: {e}"))?;
        Ok(Self::from_child(child))
    }

    fn from_child(mut child: Child) -> Self {
        let control = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, events) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let event = match line {
                    Ok(line) if line == "READY" => Event::Ready,
                    Ok(line) if line == "STOPPED" => Event::Stopped,
                    Ok(line) if line.starts_with("ERROR ") => Event::Error(line[6..].to_owned()),
                    Ok(line) => match line.strip_prefix("PRESS ").and_then(|s| s.parse().ok()) {
                        Some(time) if time >= 0 => Event::Press(time),
                        _ => Event::Error("Invalid click-helper response.".into()),
                    },
                    Err(error) => Event::Error(error.to_string()),
                };
                if tx.send(event).is_err() {
                    return;
                }
            }
            let _ = tx.send(Event::Closed);
        });
        Self {
            child: Some(child),
            control,
            reader: Some(reader),
            events,
            ready: false,
            error: None,
            timestamps: Vec::new(),
            stopped: false,
        }
    }

    fn receive(&mut self, event: Event) {
        match event {
            Event::Ready => self.ready = true,
            Event::Press(time) => self.timestamps.push(time),
            Event::Stopped => self.stopped = true,
            Event::Error(error) => self.error = Some(error),
            Event::Closed if self.error.is_none() => {
                self.error = Some(if self.ready {
                    "Click detection stopped. Turn auto-camera off and on to reconnect."
                } else {
                    "Access was not granted. Using pause-based zooms. Toggle auto-camera to retry."
                }.into());
            }
            Event::Closed => (),
        }
    }

    pub fn poll(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.receive(event);
        }
    }

    pub fn start(&mut self) -> Result<(), String> {
        self.poll();
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        if !self.ready {
            return Err("Click detection is not ready.".into());
        }
        self.timestamps.clear();
        self.stopped = false;
        self.control
            .as_mut()
            .ok_or("Click detection is closed.")?
            .write_all(b"S")
            .map_err(|e| e.to_string())
    }

    pub fn stop(&mut self) -> Result<(), String> {
        self.control
            .as_mut()
            .ok_or("Click detection is closed.")?
            .write_all(b"P")
            .map_err(|e| e.to_string())
    }

    pub fn finish_recording(&mut self, end: i64) -> Result<Vec<i64>, String> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.stopped && self.error.is_none() {
            match self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(event) => self.receive(event),
                Err(_) => {
                    return Err(
                        "Click detection did not stop cleanly. Using pause-based zooms.".into(),
                    );
                }
            }
        }
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        self.timestamps.retain(|time| *time <= end);
        self.timestamps.sort_unstable();
        self.timestamps.dedup();
        Ok(std::mem::take(&mut self.timestamps))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.control.take();
        if let Some(mut child) = self.child.take() {
            // Closing stdin ends an authorized helper; kill also cancels a pending launcher.
            let _ = child.kill();
            let reader = self.reader.take();
            thread::spawn(move || {
                let _ = child.wait();
                if let Some(reader) = reader {
                    let _ = reader.join();
                }
            });
        }
    }
}

pub(super) fn attach(samples: &mut Vec<CursorSample>, timestamps: &[i64], first_frame: i64) {
    let mut index = 0;
    let mut presses = Vec::new();
    for time in timestamps
        .iter()
        .copied()
        .filter(|time| *time >= first_frame)
    {
        let elapsed = Duration::from_micros((time - first_frame) as u64);
        if let Some(mut sample) = interpolated_cursor(samples, elapsed, &mut index) {
            sample.elapsed = elapsed;
            sample.click = true;
            presses.push(sample);
        }
    }
    samples.extend(presses);
    samples.sort_by_key(|sample| sample.elapsed);
}

#[repr(C)]
struct InputMask {
    kind: u32,
    size: u32,
    bits: u64,
}

fn ioctl_request(direction: u32, number: u32, size: usize) -> libc::c_ulong {
    ((direction << 30) | ((size as u32) << 16) | (u32::from(b'E') << 8) | number) as libc::c_ulong
}

fn mouse_capabilities(bitmap: &str) -> bool {
    bitmap
        .split_whitespace()
        .rev()
        .nth(BTN_LEFT / usize::BITS as usize)
        .and_then(|word| usize::from_str_radix(word, 16).ok())
        .is_some_and(|word| word & (1 << (BTN_LEFT % usize::BITS as usize)) != 0)
}

fn open_mice() -> io::Result<Vec<fs::File>> {
    let mut devices = Vec::new();
    for entry in fs::read_dir("/sys/class/input")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| {
            name.strip_prefix("event").is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
            })
        }) else {
            continue;
        };
        let bitmap =
            fs::read_to_string(entry.path().join("device/capabilities/key")).unwrap_or_default();
        if !mouse_capabilities(&bitmap) {
            continue;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(Path::new("/dev/input").join(name))?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_char_device() || libc::major(metadata.rdev()) != 13 {
            return Err(io::Error::other("Invalid mouse device."));
        }
        let mut bits = [0u8; 96];
        // SAFETY: the fd is owned and the ioctl buffer is exactly the advertised size.
        if unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                ioctl_request(2, 0x21, bits.len()),
                bits.as_mut_ptr(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        if bits[BTN_LEFT / 8] & (1 << (BTN_LEFT % 8)) == 0 {
            continue;
        }
        // Filter keyboard keys and every other payload at the kernel boundary, even on combo devices.
        for kind in 1..32 {
            let mut mask_bits = [0u8; 96];
            if kind == 1 {
                for button in BTN_LEFT..=BTN_TASK {
                    mask_bits[button / 8] |= 1 << (button % 8);
                }
            }
            let mask = InputMask {
                kind,
                size: mask_bits.len() as u32,
                bits: mask_bits.as_ptr() as u64,
            };
            // SAFETY: mask and its backing buffer remain live for the ioctl.
            if unsafe {
                libc::ioctl(
                    file.as_raw_fd(),
                    ioctl_request(1, 0x93, size_of::<InputMask>()),
                    &mask,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        let clock = libc::CLOCK_MONOTONIC;
        // SAFETY: EVIOCSCLOCKID takes a pointer to a live integer.
        if unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                ioctl_request(1, 0xa0, size_of::<i32>()),
                &clock,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        devices.push(file);
    }
    if devices.is_empty() {
        return Err(io::Error::other(
            "No mouse-button devices found. Connect a mouse and retry.",
        ));
    }
    Ok(devices)
}

fn drop_privileges() -> io::Result<()> {
    let uid = std::env::var("PKEXEC_UID")
        .ok()
        .and_then(|s| s.parse::<libc::uid_t>().ok())
        .filter(|uid| *uid != 0)
        .ok_or_else(|| io::Error::other("Launch click detection through Screenema."))?;
    // SAFETY: this helper is single-threaded; getpwuid returns a checked pointer.
    unsafe {
        let user = libc::getpwuid(uid);
        if user.is_null() {
            return Err(io::Error::other("Unknown requesting user."));
        }
        let gid = (*user).pw_gid;
        if libc::setgroups(0, std::ptr::null()) != 0
            || libc::setgid(gid) != 0
            || libc::setuid(uid) != 0
            || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn press_timestamp(event: &libc::input_event) -> Option<i64> {
    if event.type_ != 1
        || !(BTN_LEFT..=BTN_TASK).contains(&(event.code as usize))
        || event.value != 1
    {
        return None;
    }
    if event.time.tv_sec < 0 || !(0..1_000_000).contains(&event.time.tv_usec) {
        return None;
    }
    event
        .time
        .tv_sec
        .checked_mul(1_000_000)?
        .checked_add(event.time.tv_usec)
}

// No GTK, shell, file paths, or commands supplied by the caller enter the privileged path.
pub(super) fn helper() -> io::Result<()> {
    let mut parent = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: parent points to one initialized pollfd. A cancelled prompt must not open devices.
    if unsafe { libc::poll(&mut parent, 1, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if parent.revents & libc::POLLHUP != 0 {
        return Ok(());
    }
    let devices = open_mice()?;
    drop_privileges()?;
    // SAFETY: stdin is a live inherited pipe; duplicate it so reads stay unbuffered.
    let input =
        unsafe { std::os::fd::BorrowedFd::borrow_raw(libc::STDIN_FILENO) }.try_clone_to_owned()?;
    collect(devices, fs::File::from(input), io::stdout().lock())
}

fn collect(devices: Vec<fs::File>, mut input: fs::File, mut output: impl Write) -> io::Result<()> {
    writeln!(output, "READY")?;
    output.flush()?;
    let mut start = None;
    let mut polls = vec![libc::pollfd {
        fd: input.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    polls.extend(devices.iter().map(|file| libc::pollfd {
        fd: file.as_raw_fd(),
        events: 0,
        revents: 0,
    }));
    let mut dropped = vec![false; devices.len()];
    loop {
        // SAFETY: poll owns no fds; all entries remain live until this function returns.
        if unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, -1) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(io::Error::last_os_error());
        }
        for (index, file) in devices.iter().enumerate() {
            if polls[index + 1].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(io::Error::other(
                    "Mouse disconnected. Enable click detection again.",
                ));
            }
            if polls[index + 1].revents & libc::POLLIN == 0 {
                continue;
            }
            loop {
                let mut event = std::mem::MaybeUninit::<libc::input_event>::uninit();
                // SAFETY: read initializes exactly one input_event before assume_init below.
                let count = unsafe {
                    libc::read(
                        file.as_raw_fd(),
                        event.as_mut_ptr().cast(),
                        size_of::<libc::input_event>(),
                    )
                };
                if count < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::WouldBlock {
                        break;
                    }
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                if count as usize != size_of::<libc::input_event>() {
                    return Err(io::Error::other("Incomplete mouse event."));
                }
                // SAFETY: checked the full initialized byte count above.
                let event = unsafe { event.assume_init() };
                if event.type_ == 0 && event.code == 3 {
                    dropped[index] = true;
                    continue;
                }
                if dropped[index] {
                    if event.type_ == 0 && event.code == 0 {
                        dropped[index] = false;
                    }
                    continue;
                }
                if let Some(time) =
                    press_timestamp(&event).filter(|time| start.is_some_and(|start| *time >= start))
                {
                    writeln!(output, "PRESS {time}")?;
                }
            }
        }
        output.flush()?;
        if polls[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return Ok(());
        }
        if polls[0].revents & libc::POLLIN != 0 {
            let mut command = [0u8; 1];
            if input.read(&mut command)? == 0 {
                return Ok(());
            }
            match command[0] {
                b'S' if start.is_none() => {
                    start = Some(glib::monotonic_time());
                    for device in &mut polls[1..] {
                        device.events = libc::POLLIN;
                    }
                }
                b'P' if start.is_some() => {
                    start = None;
                    for device in &mut polls[1..] {
                        device.events = 0;
                    }
                    writeln!(output, "STOPPED")?;
                    output.flush()?;
                }
                _ => return Err(io::Error::other("Invalid click-helper command.")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::{UnixDatagram, UnixStream};

    fn event(kind: u16, code: u16, value: i32, time: i64) -> libc::input_event {
        // SAFETY: input_event is an integer-only C structure; initialize padding as well.
        let mut event: libc::input_event = unsafe { std::mem::zeroed() };
        event.type_ = kind;
        event.code = code;
        event.value = value;
        event.time.tv_sec = time / 1_000_000;
        event.time.tv_usec = time % 1_000_000;
        event
    }

    #[test]
    fn only_mouse_button_presses_are_clicks() {
        for (kind, code, value) in [(1, 30, 1), (1, 272, 0), (1, 272, 2), (2, 0, 1), (1, 330, 1)] {
            assert!(press_timestamp(&event(kind, code, value, 1_234_567)).is_none());
        }
        for code in 272..=279 {
            assert_eq!(
                press_timestamp(&event(1, code, 1, 1_234_567)),
                Some(1_234_567)
            );
        }
        assert!(!mouse_capabilities("0"));
        assert!(!mouse_capabilities("not a bitmap"));
        let mut words = vec![0usize; BTN_LEFT / usize::BITS as usize + 1];
        words[BTN_LEFT / usize::BITS as usize] = 1 << (BTN_LEFT % usize::BITS as usize);
        let bitmap = words
            .iter()
            .rev()
            .map(|word| format!("{word:x}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(mouse_capabilities(&bitmap));
    }

    #[test]
    fn collector_is_idle_between_takes_and_exits_when_parent_closes() {
        let (mut control, input) = UnixStream::pair().unwrap();
        let (events, device) = UnixDatagram::pair().unwrap();
        device.set_nonblocking(true).unwrap();
        let (output, readout) = UnixStream::pair().unwrap();
        readout
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut lines = BufReader::new(readout);
        let worker = thread::spawn(move || {
            collect(
                vec![fs::File::from(OwnedFd::from(device))],
                fs::File::from(OwnedFd::from(input)),
                output,
            )
        });
        let read_line = |lines: &mut BufReader<UnixStream>| {
            let mut line = String::new();
            lines.read_line(&mut line).unwrap();
            line
        };
        let send = |event: libc::input_event| {
            // SAFETY: the fully initialized C record remains alive through send.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    (&event as *const libc::input_event).cast(),
                    size_of::<libc::input_event>(),
                )
            };
            events.send(bytes).unwrap();
        };
        assert_eq!(read_line(&mut lines), "READY\n");
        for _ in 0..2 {
            // An idle click is buffered by evdev but must not leak into the next take.
            send(event(1, 272, 1, 0));
            control.write_all(b"S").unwrap();
            let time = glib::monotonic_time() + 10_000_000;
            send(event(1, 30, 1, time)); // keyboard
            send(event(1, 272, 0, time)); // release
            send(event(1, 272, 2, time)); // repeat
            send(event(0, 3, 0, time)); // overflow: ignore until next SYN_REPORT
            send(event(1, 272, 1, time));
            send(event(0, 0, 0, time));
            send(event(1, 272, 1, time));
            assert_eq!(read_line(&mut lines), format!("PRESS {time}\n"));
            control.write_all(b"P").unwrap();
            assert_eq!(read_line(&mut lines), "STOPPED\n");
        }
        drop(control);
        assert!(worker.join().unwrap().is_ok());
        assert_eq!(read_line(&mut lines), "");
    }

    #[test]
    fn authorization_denial_never_starts_collection() {
        let child = Command::new("sh")
            .args(["-c", "exit 126"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut session = Session::from_child(child);
        let event = session.events.recv_timeout(Duration::from_secs(2)).unwrap();
        session.receive(event);
        assert!(!session.ready);
        assert!(session.start().is_err());
        assert!(session.error.as_ref().unwrap().contains("not granted"));
    }

    #[test]
    fn click_alignment_discards_pre_video_events_and_interpolates_positions() {
        let mut samples = vec![
            CursorSample {
                click: false,
                elapsed: Duration::ZERO,
                x: 0.0,
                y: 10.0,
            },
            CursorSample {
                click: false,
                elapsed: Duration::from_millis(100),
                x: 100.0,
                y: 30.0,
            },
        ];
        attach(&mut samples, &[999_999, 1_000_000, 1_050_000], 1_000_000);
        let clicks: Vec<_> = samples.iter().filter(|sample| sample.click).collect();
        assert_eq!(clicks.len(), 2);
        assert_eq!(clicks[0].elapsed, Duration::ZERO);
        assert_eq!((clicks[1].x, clicks[1].y), (50.0, 20.0));
        assert_eq!(clicks[1].elapsed, Duration::from_millis(50));
        let pointer = interpolated_cursor(&samples, Duration::from_millis(50), &mut 0).unwrap();
        assert!(
            !pointer.click,
            "Interpolation must not duplicate click events"
        );
    }
}
