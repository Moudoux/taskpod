//! Job control for the commands taskpod runs, so a command that wants the
//! terminal never waits for an answer out of sight.
//!
//! Commands run in their own process group in taskpod's session, in the
//! background like a shell's `cmd &`. The session is unchanged, so credentials
//! sudo cached for this terminal still apply to them. When something in a
//! background group reads the terminal (a password, a question) the kernel stops
//! it with SIGTTIN; changing the terminal's modes, or writing to it while
//! `tostop` is set (taskpod sets it while it runs), stops it with SIGTTOU.
//!
//! A watcher thread looks for stopped groups. A stopped command is killed and
//! fails saying it wanted the terminal, unless its owner asked to be told
//! instead (`Job::on_stop`): a step that may ask is then given the terminal
//! (`Job::foreground`) until it exits.

use std::io;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex, mpsc};

/// What the owner of a job that may ask hears about it.
pub enum Event {
    /// A process in the job stopped to use the terminal.
    Stopped,
    /// The command exited (sent by the owner's own waiter).
    Exited(io::Result<ExitStatus>),
}

/// A running command and its process group.
pub struct Job {
    #[cfg(unix)]
    pgid: i32,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    /// The process that stopped to use the terminal.
    wanted: Option<String>,
    /// When the job was killed for it.
    #[cfg(unix)]
    killed: Option<std::time::Instant>,
    /// Tell the owner instead of killing the job.
    notify: Option<mpsc::Sender<Event>>,
    /// The owner was told about the current stop.
    notified: bool,
}

/// `process` wanting the terminal, for error messages.
pub fn wanted(process: &str) -> String {
    format!("{process} wanted the terminal (for a password, a security key or a question)")
}

impl Job {
    /// Start `command` in its own process group and watch it.
    pub fn spawn(command: &mut Command) -> io::Result<(Child, Job)> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            let child = clean(command).spawn()?;
            let job = Job {
                pgid: child.id() as i32,
                state: Arc::default(),
            };
            imp::register(job.pgid, &job.state);
            Ok((child, job))
        }
        #[cfg(not(unix))]
        {
            let child = command.spawn()?;
            Ok((
                child,
                Job {
                    state: Arc::default(),
                },
            ))
        }
    }

    /// Send `Event::Stopped` to `sender` when the job stops to use the
    /// terminal, instead of killing it.
    pub fn on_stop(&self, sender: mpsc::Sender<Event>) {
        self.lock().notify = Some(sender);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The process that stopped to use the terminal, if the job was killed for it.
    pub fn killed(&self) -> Option<String> {
        let state = self.lock();
        #[cfg(unix)]
        let killed = state.killed.is_some();
        #[cfg(not(unix))]
        let killed = false;
        state.wanted.clone().filter(|_| killed)
    }

    /// The process that stopped to use the terminal last.
    pub fn wanted(&self) -> Option<String> {
        self.lock().wanted.clone()
    }

    /// End the job, because it wants the terminal and can't have it.
    pub fn kill(&self) {
        #[cfg(unix)]
        imp::kill(self.pgid, &mut self.lock());
    }

    /// Let it go on after a stop, while it has the terminal.
    pub fn resume(&self) {
        #[cfg(unix)]
        {
            self.lock().notified = false;
            imp::signal(self.pgid, libc::SIGCONT);
        }
    }

    /// Give the job the terminal and let it go on, until the guard is
    /// dropped. `None` when taskpod has no terminal to give.
    pub fn foreground(&self) -> Option<Foreground> {
        #[cfg(unix)]
        {
            let tty = imp::tty()?;
            let saved = imp::termios(tty.fd);
            if !imp::set_foreground(tty.fd, self.pgid) {
                return None;
            }
            self.resume();
            Some(Foreground {
                fd: tty.fd,
                pgrp: tty.pgrp,
                saved,
            })
        }
        #[cfg(not(unix))]
        None
    }

    /// Stop watching. Returns the process that wanted the terminal, if the
    /// job was killed for it.
    pub fn finish(self) -> Option<String> {
        self.killed()
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        #[cfg(unix)]
        imp::unregister(self.pgid);
    }
}

/// The terminal, lent to a job. Taken back, with its modes as they were, when dropped.
pub struct Foreground {
    #[cfg(unix)]
    fd: i32,
    #[cfg(unix)]
    pgrp: i32,
    #[cfg(unix)]
    saved: Option<libc::termios>,
}

impl Drop for Foreground {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            imp::set_foreground(self.fd, self.pgrp);
            if let Some(saved) = &self.saved {
                // SAFETY: fd is the open terminal, saved a termios read from it.
                unsafe { libc::tcsetattr(self.fd, libc::TCSADRAIN, saved) };
            }
        }
    }
}

/// Start `command` with no signals blocked: taskpod blocks SIGTTIN and
/// SIGTTOU, and children would inherit that. Every command taskpod starts
/// goes through this (`Job::spawn` does it).
pub fn clean(command: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(|| {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                Ok(())
            });
        }
    }
    command
}

/// Whether a step that asks can be given the terminal in this run.
pub fn can_hand_over() -> bool {
    #[cfg(unix)]
    return imp::tty().is_some_and(|t| t.handover);
    #[cfg(not(unix))]
    false
}

/// Run `command` like `Command::output` (stdin closed, output captured),
/// failing if it stops to use the terminal.
pub fn output(command: &mut Command) -> io::Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (child, job) = Job::spawn(command)?;
    let output = child.wait_with_output()?;
    match job.finish() {
        Some(process) => Err(io::Error::other(wanted(&process))),
        None => Ok(output),
    }
}

/// Set up job control for this process: call first thing in `main`, before
/// any thread starts. With a terminal it's in the foreground of, `tostop` is
/// set until the guard is dropped; `handover` says whether steps that may
/// ask get the terminal (not for `--json`, which never asks).
pub fn init(handover: bool) -> Guard {
    #[cfg(unix)]
    imp::init(handover);
    #[cfg(not(unix))]
    let _ = handover;
    Guard
}

/// Puts the terminal back as it was when dropped.
pub struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(unix)]
        imp::restore();
    }
}

#[cfg(unix)]
mod imp {
    use super::{Event, State};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    /// How often stopped jobs are looked for.
    const POLL: Duration = Duration::from_millis(200);
    /// How long a killed job gets to exit before SIGKILL.
    const GRACE: Duration = Duration::from_secs(2);

    pub struct Tty {
        pub fd: i32,
        /// taskpod's own process group, the terminal's foreground.
        pub pgrp: i32,
        pub handover: bool,
        /// Whether `tostop` was set before taskpod set it.
        tostop: bool,
    }

    static TTY: OnceLock<Tty> = OnceLock::new();
    static JOBS: Mutex<Vec<(i32, Arc<Mutex<State>>)>> = Mutex::new(Vec::new());

    pub fn tty() -> Option<&'static Tty> {
        TTY.get()
    }

    fn jobs() -> std::sync::MutexGuard<'static, Vec<(i32, Arc<Mutex<State>>)>> {
        JOBS.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn register(pgid: i32, state: &Arc<Mutex<State>>) {
        static WATCHER: OnceLock<()> = OnceLock::new();
        WATCHER.get_or_init(|| {
            std::thread::spawn(watch);
        });
        jobs().push((pgid, state.clone()));
    }

    pub fn unregister(pgid: i32) {
        jobs().retain(|(p, _)| *p != pgid);
    }

    pub fn signal(pgid: i32, signal: i32) {
        // SAFETY: plain syscall; a negative pid sends to the process group.
        unsafe { libc::kill(-pgid, signal) };
    }

    pub fn kill(pgid: i32, state: &mut State) {
        if state.killed.is_none() {
            state.killed = Some(Instant::now());
            signal(pgid, libc::SIGTERM);
            signal(pgid, libc::SIGCONT);
        }
    }

    pub fn termios(fd: i32) -> Option<libc::termios> {
        // SAFETY: termios is plain data, filled in by tcgetattr.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            (libc::tcgetattr(fd, &mut t) == 0).then_some(t)
        }
    }

    pub fn set_foreground(fd: i32, pgrp: i32) -> bool {
        // SIGTTOU is blocked in every taskpod thread, so this works from the background.
        // SAFETY: plain syscall.
        unsafe { libc::tcsetpgrp(fd, pgrp) == 0 }
    }

    /// Look for stopped jobs, kill those that can't have the terminal and
    /// tell the owners of the others.
    fn watch() {
        loop {
            std::thread::sleep(POLL);
            let jobs: Vec<_> = jobs().clone();
            if jobs.is_empty() {
                continue;
            }
            let groups = scan(&jobs.iter().map(|(p, _)| *p).collect::<Vec<_>>());
            for (pgid, state) in jobs {
                let Some(group) = groups.get(&pgid) else {
                    continue;
                };
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(when) = state.killed {
                    if when.elapsed() > GRACE {
                        signal(pgid, libc::SIGKILL);
                    }
                    continue;
                }
                let Some(process) = &group.stopped else {
                    continue;
                };
                state.wanted = Some(process.clone());
                if state.notify.is_none() {
                    kill(pgid, &mut state);
                } else if !state.notified {
                    state.notified = true;
                    if let Some(sender) = &state.notify {
                        let _ = sender.send(Event::Stopped);
                    }
                }
            }
        }
    }

    /// A process group that still has processes.
    #[derive(Default)]
    struct Group {
        /// The name of a stopped process in it.
        stopped: Option<String>,
    }

    #[cfg(target_os = "linux")]
    fn scan(pgids: &[i32]) -> HashMap<i32, Group> {
        let mut groups = HashMap::new();
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return groups;
        };
        for entry in dir.flatten() {
            let name = entry.file_name();
            if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // `pid (comm) state ppid pgrp ...`; comm may hold spaces and parentheses.
            let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
                continue;
            };
            let mut fields = stat[close + 1..].split_whitespace();
            let (Some(state), Some(_ppid), Some(pgrp)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let Ok(pgrp) = pgrp.parse::<i32>() else {
                continue;
            };
            if pgids.contains(&pgrp) {
                let group: &mut Group = groups.entry(pgrp).or_default();
                if state == "T" {
                    group.stopped = Some(stat[open + 1..close].to_string());
                }
            }
        }
        groups
    }

    #[cfg(not(target_os = "linux"))]
    fn scan(pgids: &[i32]) -> HashMap<i32, Group> {
        let mut groups = HashMap::new();
        let Ok(output) = std::process::Command::new("ps")
            .args(["-A", "-o", "pgid=", "-o", "stat=", "-o", "comm="])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        else {
            return groups;
        };
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut fields = line.split_whitespace();
            let (Some(pgrp), Some(state)) = (fields.next(), fields.next()) else {
                continue;
            };
            let Ok(pgrp) = pgrp.parse::<i32>() else {
                continue;
            };
            if pgids.contains(&pgrp) {
                let group: &mut Group = groups.entry(pgrp).or_default();
                if state.starts_with('T') {
                    let comm = fields.collect::<Vec<_>>().join(" ");
                    let name = comm.rsplit('/').next().unwrap_or_default().to_string();
                    group.stopped = Some(name);
                }
            }
        }
        groups
    }

    fn sigset(signals: &[i32]) -> libc::sigset_t {
        // SAFETY: sigset_t is plain data, initialized by sigemptyset.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for &s in signals {
                libc::sigaddset(&mut set, s);
            }
            set
        }
    }

    /// Signals that end taskpod, passed on to the jobs first.
    const ENDING: [i32; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

    /// Where the signal handler writes the signals that end taskpod.
    static SIGNALS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

    extern "C" fn on_signal(sig: i32) {
        let fd = SIGNALS.load(std::sync::atomic::Ordering::Relaxed);
        let byte = sig as u8;
        // SAFETY: write is async-signal-safe.
        unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }

    pub fn init(handover: bool) {
        // Every thread blocks SIGTTIN and SIGTTOU (threads inherit it), so
        // taskpod can take the terminal back and print while a job has it.
        // Children get a clean mask (`clean`).
        // SAFETY: plain syscall on an initialized set.
        unsafe {
            let blocked = sigset(&[libc::SIGTTIN, libc::SIGTTOU]);
            libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut());
        }
        // The signals that end taskpod are passed on to the jobs, which are in
        // process groups of their own, by a thread the handler wakes up.
        let mut fds = [0; 2];
        // SAFETY: pipe fills fds; sigaction gets a zeroed struct with a handler.
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) == 0 {
                for fd in fds {
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
                SIGNALS.store(fds[1], std::sync::atomic::Ordering::Relaxed);
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as extern "C" fn(i32) as usize;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                for sig in ENDING {
                    libc::sigaction(sig, &action, std::ptr::null_mut());
                }
                let read = fds[0];
                std::thread::spawn(move || wait_for_signals(read));
            }
        }

        // SAFETY: open and terminal syscalls on a path and the fd they return.
        unsafe {
            let fd = libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
            if fd < 0 {
                return;
            }
            let pgrp = libc::getpgrp();
            if libc::tcgetpgrp(fd) != pgrp {
                // Running in the background itself: leave the terminal alone.
                libc::close(fd);
                return;
            }
            let Some(mut modes) = termios(fd) else {
                libc::close(fd);
                return;
            };
            let tostop = modes.c_lflag & libc::TOSTOP != 0;
            modes.c_lflag |= libc::TOSTOP;
            libc::tcsetattr(fd, libc::TCSANOW, &modes);
            let _ = TTY.set(Tty {
                fd,
                pgrp,
                handover,
                tostop,
            });
        }
    }

    fn wait_for_signals(fd: i32) {
        let mut byte = 0u8;
        loop {
            // SAFETY: reads one byte into a local.
            let n = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
            if n == 1 {
                end(i32::from(byte));
            } else if n == 0 {
                return;
            }
        }
    }

    /// Put the terminal back: taskpod's in the foreground, `tostop` as it was.
    pub fn restore() {
        let Some(tty) = tty() else {
            return;
        };
        set_foreground(tty.fd, tty.pgrp);
        if let Some(mut modes) = termios(tty.fd)
            && !tty.tostop
        {
            modes.c_lflag &= !libc::TOSTOP;
            // SAFETY: modes was read from this fd.
            unsafe { libc::tcsetattr(tty.fd, libc::TCSANOW, &modes) };
        }
    }

    /// taskpod was told to stop: pass it on to the jobs, put the terminal back, and exit.
    fn end(sig: i32) {
        for (pgid, _) in jobs().iter() {
            signal(*pgid, sig);
            signal(*pgid, libc::SIGCONT);
        }
        restore();
        let _ = console::Term::stderr().show_cursor();
        std::process::exit(128 + sig);
    }
}

/// How long to wait between looking at a job, for tests.
#[cfg(all(test, unix))]
pub const SETTLE: std::time::Duration = std::time::Duration::from_millis(1500);

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_command_that_wants_the_terminal_is_stopped_and_killed() {
        // Stopping itself the way reading the terminal from the background does.
        let start = Instant::now();
        let error =
            output(Command::new("sh").args(["-c", "kill -TTIN $$; echo resumed"])).unwrap_err();
        assert_eq!(error.to_string(), wanted("sh"));
        assert!(start.elapsed() < SETTLE * 2, "took {:?}", start.elapsed());
    }

    #[test]
    fn other_commands_run_as_usual() {
        let output = output(Command::new("sh").args(["-c", "echo hi; exit 3"])).unwrap();
        assert_eq!(output.stdout, b"hi\n");
        assert_eq!(output.status.code(), Some(3));
    }

    #[test]
    fn the_owner_is_told_instead_when_it_asks() {
        let mut command = Command::new("sh");
        command.args(["-c", "kill -TTOU $$; exit 4"]);
        let (mut child, job) = Job::spawn(&mut command).unwrap();
        let (tx, rx) = mpsc::channel();
        job.on_stop(tx);
        assert!(matches!(rx.recv_timeout(SETTLE * 2), Ok(Event::Stopped)));
        assert_eq!(job.wanted().as_deref(), Some("sh"));
        // No terminal in tests, so it can't be given one; resuming lets it finish.
        assert!(job.foreground().is_none());
        job.resume();
        assert_eq!(child.wait().unwrap().code(), Some(4));
        assert_eq!(job.finish(), None);
    }
}
