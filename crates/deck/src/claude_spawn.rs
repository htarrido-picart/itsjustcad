// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Spawning the `claude` CLI child, with a macOS-specific TCC fix.
//!
//! # Why this module exists
//!
//! On macOS, when the app spawns the `claude` CLI as a child process, any TCC
//! (privacy) permission request the child makes is attributed by the system to
//! the **responsible parent** — our app. Claude Code's notification/completion
//! path can brush the media library, which makes macOS raise an
//! "ItsJustCAD would like to access Apple Music" dialog *against our app*. Our
//! own bundle is clean (no MediaPlayer link, no `NSAppleMusicUsageDescription`),
//! so the fix is to stop being the responsible party for the child.
//!
//! macOS exposes exactly this through the private-but-ubiquitous libSystem
//! symbol `responsibility_spawnattrs_setdisclaim`: setting it on the
//! `posix_spawnattr_t` before spawn makes the child **disclaim** our
//! responsibility. A disclaimed, UI-less child can't raise a TCC prompt, so the
//! media access simply fails silently (we never want media) and NO DIALOG
//! appears.
//!
//! The disclaim MUST be set on the spawn attributes *before* `exec` — a
//! `pre_exec` closure (which runs post-fork, pre-exec, in the child) is too late
//! and has no equivalent syscall, so `CommandExt::pre_exec` cannot do it. That
//! forces a raw `posix_spawn` on macOS. Every other platform keeps the plain,
//! unchanged `tokio::process::Command` path.
//!
//! # Uniform interface
//!
//! Both platforms produce a [`ClaudeChild`] whose [`ClaudeChild::next_line`] and
//! [`ClaudeChild::wait`] the streaming loop consumes identically, so the
//! JSON-line parsing in `claude_code.rs` is platform-agnostic.

use std::path::Path;

use crate::deck::DeckError;

/// A spawned `claude` child whose stdout is delivered as decoded lines.
pub(crate) struct ClaudeChild {
    inner: Inner,
}

impl ClaudeChild {
    /// The next line of the child's stdout, or `None` at EOF. Mirrors
    /// `tokio::io::Lines::next_line` semantics (line terminator stripped).
    pub(crate) async fn next_line(&mut self) -> Option<String> {
        match &mut self.inner {
            #[cfg(target_os = "macos")]
            Inner::Macos(m) => m.rx.recv().await,
            #[cfg(not(target_os = "macos"))]
            Inner::Portable(p) => p.lines.next_line().await.ok().flatten(),
        }
    }

    /// Reap the child. Best-effort; errors are swallowed like the previous
    /// `let _ = child.wait().await;`.
    pub(crate) async fn wait(&mut self) {
        match &mut self.inner {
            #[cfg(target_os = "macos")]
            Inner::Macos(m) => m.wait().await,
            #[cfg(not(target_os = "macos"))]
            Inner::Portable(p) => {
                let _ = p.child.wait().await;
            }
        }
    }
}

enum Inner {
    #[cfg(target_os = "macos")]
    Macos(macos::MacChild),
    #[cfg(not(target_os = "macos"))]
    Portable(portable::PortableChild),
}

/// Spawn the `claude` CLI at `bin` with `args`, its `PATH` set to
/// `path_env`, stdin from `/dev/null`, stdout piped, stderr discarded.
///
/// On macOS the child is spawned with TCC responsibility disclaimed (see the
/// module docs); on every other platform this is an ordinary
/// `tokio::process::Command` spawn.
pub(crate) fn spawn_claude(
    bin: &Path,
    args: &[String],
    path_env: &str,
) -> Result<ClaudeChild, DeckError> {
    #[cfg(target_os = "macos")]
    {
        let inner = macos::spawn(bin, args, path_env)?;
        Ok(ClaudeChild { inner: Inner::Macos(inner) })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let inner = portable::spawn(bin, args, path_env)?;
        Ok(ClaudeChild { inner: Inner::Portable(inner) })
    }
}

#[cfg(not(target_os = "macos"))]
mod portable {
    use super::*;
    use tokio::io::{AsyncBufReadExt as _, BufReader, Lines};
    use tokio::process::{Child, ChildStdout};

    pub(super) struct PortableChild {
        pub(super) child: Child,
        pub(super) lines: Lines<BufReader<ChildStdout>>,
    }

    pub(super) fn spawn(
        bin: &Path,
        args: &[String],
        path_env: &str,
    ) -> Result<PortableChild, DeckError> {
        let mut child = tokio::process::Command::new(bin)
            .args(args)
            .env("PATH", path_env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| DeckError::Stream(format!("cannot launch claude CLI: {e}")))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let lines = BufReader::new(stdout).lines();
        Ok(PortableChild { child, lines })
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::ffi::CString;
    use std::io::{BufRead as _, BufReader};
    use std::os::unix::io::FromRawFd as _;
    use tokio::sync::mpsc::{self, UnboundedReceiver};

    // libSystem's private (but decades-stable, App-Store-safe in practice)
    // responsibility API: setting disclaim=1 on the spawn attrs makes the
    // spawned child NOT route its TCC/permission requests through us.
    unsafe extern "C" {
        fn responsibility_spawnattrs_setdisclaim(
            attr: *mut libc::posix_spawnattr_t,
            disclaim: libc::c_int,
        ) -> libc::c_int;
    }

    pub(super) struct MacChild {
        pub(super) pid: libc::pid_t,
        pub(super) rx: UnboundedReceiver<String>,
        reaped: bool,
    }

    impl MacChild {
        /// Reap the child so it doesn't linger as a zombie. Runs the blocking
        /// `waitpid` off the async runtime.
        pub(super) async fn wait(&mut self) {
            if self.reaped {
                return;
            }
            self.reaped = true;
            let pid = self.pid;
            let _ = tokio::task::spawn_blocking(move || {
                let mut status: libc::c_int = 0;
                // SAFETY: reaping a pid we created; status is a valid out-param.
                unsafe { libc::waitpid(pid, &mut status, 0) };
            })
            .await;
        }
    }

    impl Drop for MacChild {
        fn drop(&mut self) {
            // Belt-and-suspenders: if the child outlives us (dropped without a
            // completed `wait`), kill it and reap so we don't leak a process —
            // mirrors the portable path's `kill_on_drop(true)`.
            if !self.reaped {
                // SAFETY: signalling/reaping a pid we created.
                unsafe {
                    libc::kill(self.pid, libc::SIGKILL);
                    let mut status: libc::c_int = 0;
                    libc::waitpid(self.pid, &mut status, 0);
                }
            }
        }
    }

    /// Spawn `claude` via raw `posix_spawn` with TCC responsibility disclaimed,
    /// piping its stdout back through a background line reader.
    pub(super) fn spawn(
        bin: &Path,
        args: &[String],
        path_env: &str,
    ) -> Result<MacChild, DeckError> {
        // Build argv: [bin, args...] as NUL-terminated C strings.
        let bin_c = cstr(bin.to_string_lossy().as_ref())?;
        let mut argv_owned: Vec<CString> = Vec::with_capacity(args.len() + 1);
        argv_owned.push(bin_c.clone());
        for a in args {
            argv_owned.push(cstr(a)?);
        }
        let mut argv: Vec<*mut libc::c_char> =
            argv_owned.iter().map(|s| s.as_ptr() as *mut libc::c_char).collect();
        argv.push(std::ptr::null_mut());

        // Env: just PATH (matches the portable path, which only overrides PATH
        // and otherwise inherits — but posix_spawn with an explicit envp does
        // NOT inherit, so we forward the current env with PATH overridden).
        let env_owned = build_env(path_env)?;
        let mut envp: Vec<*mut libc::c_char> =
            env_owned.iter().map(|s| s.as_ptr() as *mut libc::c_char).collect();
        envp.push(std::ptr::null_mut());

        // Pipe for the child's stdout.
        let mut fds = [0i32; 2];
        // SAFETY: fds is a valid 2-int buffer.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(DeckError::Stream(format!(
                "cannot launch claude CLI: pipe() failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);

        // Open /dev/null for the child's stdin.
        let devnull = cstr("/dev/null")?;

        // SAFETY: all pointers below are valid for the duration of the call; we
        // check every return code and clean up on any failure.
        let pid = unsafe {
            let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
            if libc::posix_spawnattr_init(&mut attr) != 0 {
                close_pair(read_fd, write_fd);
                return Err(spawn_err("posix_spawnattr_init"));
            }
            // THE FIX: disclaim TCC responsibility for the child.
            let disclaim_rc = responsibility_spawnattrs_setdisclaim(&mut attr, 1);
            // A non-zero rc means the OS wouldn't take the disclaim; the child
            // is still perfectly usable, so we log-and-continue rather than
            // fail the turn. (There is no tracing target here; the outcome is
            // simply "prompt might reappear", never a broken spawn.)
            let _ = disclaim_rc;

            let mut fa: libc::posix_spawn_file_actions_t = std::mem::zeroed();
            if libc::posix_spawn_file_actions_init(&mut fa) != 0 {
                libc::posix_spawnattr_destroy(&mut attr);
                close_pair(read_fd, write_fd);
                return Err(spawn_err("posix_spawn_file_actions_init"));
            }
            // stdin <- /dev/null, stdout -> pipe write end, stderr -> /dev/null.
            libc::posix_spawn_file_actions_addopen(
                &mut fa,
                0,
                devnull.as_ptr(),
                libc::O_RDONLY,
                0,
            );
            libc::posix_spawn_file_actions_adddup2(&mut fa, write_fd, 1);
            libc::posix_spawn_file_actions_addopen(
                &mut fa,
                2,
                devnull.as_ptr(),
                libc::O_WRONLY,
                0,
            );
            // The child doesn't need either raw pipe fd once stdout is dup'd.
            libc::posix_spawn_file_actions_addclose(&mut fa, read_fd);
            libc::posix_spawn_file_actions_addclose(&mut fa, write_fd);

            let mut pid: libc::pid_t = 0;
            let rc = libc::posix_spawn(
                &mut pid,
                bin_c.as_ptr(),
                &fa,
                &attr,
                argv.as_ptr(),
                envp.as_ptr(),
            );
            libc::posix_spawn_file_actions_destroy(&mut fa);
            libc::posix_spawnattr_destroy(&mut attr);
            if rc != 0 {
                close_pair(read_fd, write_fd);
                return Err(DeckError::Stream(format!(
                    "cannot launch claude CLI: posix_spawn failed: {}",
                    std::io::Error::from_raw_os_error(rc)
                )));
            }
            pid
        };

        // Parent keeps only the read end.
        // SAFETY: write_fd is a valid, open fd owned by us.
        unsafe { libc::close(write_fd) };

        // Drain stdout on a blocking thread, forwarding lines to the async loop.
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        std::thread::spawn(move || {
            // SAFETY: read_fd is a valid, open fd we own; File takes ownership
            // and closes it on drop.
            let file = unsafe { std::fs::File::from_raw_fd(read_fd) };
            let reader = BufReader::new(file);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break; // receiver gone; stop reading.
                        }
                    }
                    Err(_) => break,
                }
            }
            // Dropping `reader`/`file` closes read_fd; dropping `tx` ends the
            // stream so `next_line` returns None.
        });

        Ok(MacChild { pid, rx, reaped: false })
    }

    fn cstr(s: &str) -> Result<CString, DeckError> {
        CString::new(s).map_err(|_| {
            DeckError::Stream("cannot launch claude CLI: argument contains NUL byte".into())
        })
    }

    /// Forward the current process env with `PATH` overridden — posix_spawn with
    /// an explicit `envp` does not inherit, so we must reconstruct it.
    fn build_env(path_env: &str) -> Result<Vec<CString>, DeckError> {
        let mut out = Vec::new();
        for (k, v) in std::env::vars_os() {
            if k == "PATH" {
                continue; // replaced below
            }
            let (k, v) = (k.to_string_lossy(), v.to_string_lossy());
            // Skip any pair we can't render as a C string rather than failing.
            if let Ok(c) = CString::new(format!("{k}={v}")) {
                out.push(c);
            }
        }
        out.push(cstr(&format!("PATH={path_env}"))?);
        Ok(out)
    }

    fn close_pair(a: libc::c_int, b: libc::c_int) {
        // SAFETY: both are fds we just created and still own.
        unsafe {
            libc::close(a);
            libc::close(b);
        }
    }

    fn spawn_err(what: &str) -> DeckError {
        DeckError::Stream(format!(
            "cannot launch claude CLI: {what} failed: {}",
            std::io::Error::last_os_error()
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The disclaim symbol links and returns success on the current macOS —
        /// a build-time + runtime proof that the FFI is wired correctly.
        #[test]
        fn disclaim_symbol_links_and_succeeds() {
            // SAFETY: init/destroy a local attr; call the FFI in between.
            unsafe {
                let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
                assert_eq!(libc::posix_spawnattr_init(&mut attr), 0);
                let rc = responsibility_spawnattrs_setdisclaim(&mut attr, 1);
                assert_eq!(rc, 0, "setdisclaim should succeed on macOS");
                libc::posix_spawnattr_destroy(&mut attr);
            }
        }

        /// End-to-end: a disclaimed child spawned via our `posix_spawn` path
        /// runs and its stdout comes back line-by-line through the channel.
        /// Proves the spawn + pipe + reader wiring that the claude turn relies
        /// on, without needing the real CLI.
        #[tokio::test]
        async fn spawns_disclaimed_child_and_reads_its_stdout() {
            let bin = Path::new("/bin/sh");
            let args = vec![
                "-c".to_string(),
                "printf 'line-one\\nline-two\\n'".to_string(),
            ];
            let mut child = spawn(bin, &args, "/usr/bin:/bin").expect("spawn");
            let l1 = child.rx.recv().await;
            let l2 = child.rx.recv().await;
            let l3 = child.rx.recv().await;
            assert_eq!(l1.as_deref(), Some("line-one"));
            assert_eq!(l2.as_deref(), Some("line-two"));
            assert_eq!(l3, None, "stream should end after the last line");
            child.wait().await;
        }

        /// The child sees the PATH we hand it (env forwarding + PATH override
        /// works through the raw envp).
        #[tokio::test]
        async fn child_receives_overridden_path() {
            let bin = Path::new("/bin/sh");
            let args = vec!["-c".to_string(), "printf '%s' \"$PATH\"".to_string()];
            let mut child = spawn(bin, &args, "/custom/marker/bin:/bin").expect("spawn");
            let got = child.rx.recv().await;
            assert_eq!(got.as_deref(), Some("/custom/marker/bin:/bin"));
            child.wait().await;
        }
    }
}
