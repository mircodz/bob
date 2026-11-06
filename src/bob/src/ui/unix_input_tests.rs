use super::{state::*, TerminalGuard};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    ptr::null_mut,
    time::{Duration, Instant},
};
use tokio::{task::JoinSet, time::timeout};

const DIRECTORY: &str = "BOB_UNIX_INPUT_FIXTURE_DIR";
const CYCLES: usize = 200;
const STEP_TIMEOUT: Duration = Duration::from_secs(5);
const CHILD: &str = "ui::unix_input_tests::unix_input_fixture";

fn input(step: usize) -> (u8, KeyCode, Focus) {
    match step {
        n if n == CYCLES * 2 => (b'\r', KeyCode::Enter, Focus::Composer),
        n if n % 2 == 0 => (b'\t', KeyCode::Tab, Focus::Sidebar),
        _ => (0x1b, KeyCode::Esc, Focus::Composer),
    }
}

fn syscall(result: libc::c_int) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[tokio::test]
async fn unix_input_fixture() {
    let Some(directory) = std::env::var_os(DIRECTORY) else {
        return;
    };
    let directory = PathBuf::from(directory);
    let guard = TerminalGuard::activate().unwrap();
    let mut events = guard.events();
    let mut app = App::new(80);
    app.sidebar = true;
    app.draft_mut().insert("/quit");
    let revision = app.draft_mut().revision;
    let status = app.status.clone();
    let mut jobs = JoinSet::new();
    for step in 0..=CYCLES * 2 {
        let (_, expected, focus) = input(step);
        let mut next = Box::pin(async {
            let mut key = None;
            let mut resized = false;
            loop {
                match events.next().await.expect("input stream ended").unwrap() {
                    Event::Resize(width, height) => {
                        assert_eq!((width, height), (100, 30));
                        resized = true;
                    }
                    Event::Key(event) => {
                        assert!(key.is_none(), "duplicate key at step {step}");
                        assert_eq!(event.code, expected, "step {step}");
                        assert_eq!(event.modifiers, KeyModifiers::NONE);
                        assert_eq!(event.kind, KeyEventKind::Press);
                        key = Some(event);
                    }
                    event => panic!("unexpected event at step {step}: {event:?}"),
                }
                if let Some(key) = key.filter(|_| expected != KeyCode::Esc || resized) {
                    return key;
                }
            }
        });
        assert!(
            futures::poll!(&mut next).is_pending(),
            "unsolicited input at step {step}"
        );
        fs::write(directory.join("pending"), step.to_string()).unwrap();
        fs::rename(directory.join("pending"), directory.join("ready")).unwrap();
        let key = timeout(STEP_TIMEOUT, next)
            .await
            .unwrap_or_else(|_| panic!("input timed out at step {step}: {expected:?}"));
        if expected == KeyCode::Enter {
            assert!(app.key(key, &mut jobs), "native Enter must dispatch /quit");
        } else {
            assert!(!app.key(key, &mut jobs), "unexpected exit at step {step}");
            assert_eq!(app.focus, focus);
            assert_eq!(app.draft_mut().text, "/quit");
            assert_eq!(app.draft_mut().revision, revision);
            assert_eq!(app.status, status, "unexpected submission at step {step}");
        }
        assert!(jobs.is_empty(), "input must not launch work");
    }
}

struct Fixture {
    directory: PathBuf,
    master: File,
    child: Option<Child>,
    output: Vec<u8>,
    deadline: Instant,
}

impl Fixture {
    fn launch() -> io::Result<Self> {
        let (mut master, mut slave) = (-1, -1);
        let mut size = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        syscall(unsafe {
            libc::openpty(&mut master, &mut slave, null_mut(), null_mut(), &mut size)
        })?;
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        for file in [&master, &slave] {
            syscall(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
        }
        syscall(unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) })?;
        let directory = std::env::temp_dir().join(format!("bob-pty-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory)?;
        let mut fixture = Self {
            directory,
            master,
            child: None,
            output: Vec::new(),
            deadline: Instant::now() + Duration::from_secs(30),
        };
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["--exact", CHILD, "--nocapture", "--test-threads=1"])
            .env(DIRECTORY, &fixture.directory)
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        unsafe {
            command.pre_exec(|| {
                syscall(libc::setsid())?;
                syscall(libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0))
            });
        }
        fixture.child = Some(command.spawn()?);
        Ok(fixture)
    }

    fn drain(&mut self) {
        let mut buffer = [0; 4096];
        for _ in 0..64 {
            match self.master.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let keep = n.min(32768usize.saturating_sub(self.output.len()));
                    self.output.extend_from_slice(&buffer[..keep]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }

    fn wait(&mut self, step: Option<usize>) -> io::Result<()> {
        let deadline = self.deadline.min(Instant::now() + STEP_TIMEOUT);
        loop {
            self.drain();
            if let Some(status) = self.child.as_mut().unwrap().try_wait()? {
                self.child = None;
                if step.is_some() || !status.success() {
                    return Err(io::Error::other(format!("exit {status} awaiting {step:?}")));
                }
                return Ok(());
            }
            let ready = fs::read_to_string(self.directory.join("ready")).ok();
            if step.is_some() && ready.and_then(|s| s.parse::<usize>().ok()) == step {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!("timeout awaiting {step:?}")));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn exercise(&mut self) -> io::Result<()> {
        for step in 0..=CYCLES * 2 {
            self.wait(Some(step))?;
            let (byte, _, _) = input(step);
            let pid = self.child.as_ref().unwrap().id() as libc::pid_t;
            let signal = || syscall(unsafe { libc::kill(pid, libc::SIGWINCH) });
            if byte == 0x1b && step % 4 == 1 {
                signal()?;
            }
            if self.master.write(&[byte])? != 1 {
                return Err(io::Error::other("short PTY write"));
            }
            if byte == 0x1b && step % 4 == 3 {
                signal()?;
            }
        }
        self.wait(None)
    }

    fn stop(&mut self) -> io::Result<()> {
        if let Some(child) = self.child.as_mut() {
            if child.try_wait()?.is_none() {
                unsafe {
                    libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
                }
                let _ = child.kill();
            }
            let deadline = Instant::now() + STEP_TIMEOUT;
            loop {
                self.drain();
                if self.child.as_mut().unwrap().try_wait()?.is_some() {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::other("could not reap PTY child"));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            self.child = None;
        }
        self.drain();
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("PTY cleanup: {error}");
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn unix_pty_sigwinch_preserves_escape_tab_and_draft() {
    let mut fixture = Fixture::launch().expect("launch Unix PTY fixture");
    let result = fixture.exercise();
    let cleanup = fixture.stop();
    assert!(
        result.is_ok() && cleanup.is_ok(),
        "{result:?}; cleanup: {cleanup:?}\nPTY output:\n{}",
        String::from_utf8_lossy(&fixture.output)
    );
}
