//! Thread quality-of-service classes (#234).
//!
//! Scheduling only: a class changes which cores run a thread and at what
//! priority, never what it computes. Ordered rayon `collect`s keep results
//! identical whatever pool or thread count runs them.
//!
//! - [`Qos::UserInteractive`]: UI, viewport and render threads.
//! - [`Qos::UserInitiated`]: commands the user waits on, including the
//!   analysis verbs. The global rayon pool runs at this class.
//! - [`Qos::Utility`]: work the user does not wait on (undo-history rebuild,
//!   optimize jobs, data updates, FFI background jobs). Start it with
//!   [`spawn_utility`], so its rayon work runs on [`utility_pool`].
//!
//! macOS and iOS map these to `pthread_set_qos_class_self_np`. Other
//! platforms do nothing for now.

use std::sync::OnceLock;

/// A thread's quality-of-service class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Qos {
    UserInteractive,
    UserInitiated,
    Utility,
}

#[cfg(target_vendor = "apple")]
impl Qos {
    fn class(self) -> libc::qos_class_t {
        match self {
            Qos::UserInteractive => libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE,
            Qos::UserInitiated => libc::qos_class_t::QOS_CLASS_USER_INITIATED,
            Qos::Utility => libc::qos_class_t::QOS_CLASS_UTILITY,
        }
    }
}

/// Set the calling thread's class. Returns `true` when the platform applied
/// it, `false` where it is a no-op (off Apple) or the call failed.
pub fn set_current_thread(qos: Qos) -> bool {
    #[cfg(target_vendor = "apple")]
    {
        // SAFETY: plain syscall wrapper on the calling thread; no pointers.
        unsafe { libc::pthread_set_qos_class_self_np(qos.class(), 0) == 0 }
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = qos;
        false
    }
}

/// The calling thread's class, when it is one of [`Qos`]. Always `None` off
/// Apple.
pub fn current_thread() -> Option<Qos> {
    #[cfg(target_vendor = "apple")]
    {
        // Read into a u32 rather than the enum: the kernel may report a
        // class the binding's enum does not list.
        let mut class: u32 = 0;
        // SAFETY: `qos_class_t` is `repr(u32)`, so the out-pointer is valid
        // for writes of its size; a null priority pointer is allowed.
        let rc = unsafe {
            libc::pthread_get_qos_class_np(
                libc::pthread_self(),
                (&raw mut class).cast(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return None;
        }
        [Qos::UserInteractive, Qos::UserInitiated, Qos::Utility]
            .into_iter()
            .find(|q| q.class() as u32 == class)
    }
    #[cfg(not(target_vendor = "apple"))]
    None
}

/// Build the global rayon pool with [`Qos::UserInitiated`] worker threads.
/// Call once at startup, before any rayon use; later calls (or a pool that
/// already exists) leave the global pool as it is.
pub fn init_global_pool() {
    let _ = rayon::ThreadPoolBuilder::new()
        .start_handler(|_| {
            set_current_thread(Qos::UserInitiated);
        })
        .build_global();
}

/// The rayon pool for parallel work started from utility jobs. Same thread
/// count as the global pool; workers run at [`Qos::Utility`].
pub fn utility_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .thread_name(|i| format!("utility-{i}"))
            .start_handler(|_| {
                set_current_thread(Qos::Utility);
            })
            .build()
            .expect("utility rayon pool")
    })
}

/// Spawn a [`Qos::Utility`] thread that runs `f` inside [`utility_pool`], so
/// any rayon work in `f` stays off the interactive pool.
pub fn spawn_utility<F>(f: F) -> std::thread::JoinHandle<()>
where
    F: FnOnce() + Send + 'static,
{
    std::thread::spawn(move || {
        set_current_thread(Qos::Utility);
        utility_pool().install(f);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayon::prelude::*;

    #[test]
    fn set_reports_platform_support() {
        let applied = std::thread::spawn(|| set_current_thread(Qos::UserInitiated))
            .join()
            .unwrap();
        assert_eq!(applied, cfg!(target_vendor = "apple"));
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn set_class_reads_back() {
        for q in [Qos::UserInteractive, Qos::UserInitiated, Qos::Utility] {
            let got = std::thread::spawn(move || {
                assert!(set_current_thread(q));
                current_thread()
            })
            .join()
            .unwrap();
            assert_eq!(got, Some(q));
        }
    }

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn noop_off_apple() {
        assert_eq!(current_thread(), None);
    }

    #[test]
    fn utility_job_runs_rayon_on_utility_pool() {
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_utility(move || {
            let sum: u64 = (0..10_000u64).into_par_iter().map(|x| x * x).sum();
            let workers: Vec<_> = (0..64)
                .into_par_iter()
                .map(|_| (rayon::current_thread_index().is_some(), current_thread()))
                .collect();
            tx.send((sum, workers)).unwrap();
        })
        .join()
        .unwrap();
        let (sum, workers) = rx.recv().unwrap();
        assert_eq!(sum, (0..10_000u64).map(|x| x * x).sum::<u64>());
        let want = cfg!(target_vendor = "apple").then_some(Qos::Utility);
        assert!(workers.iter().all(|&(in_pool, q)| in_pool && q == want));
    }

    #[test]
    fn utility_pool_matches_global_thread_count() {
        assert_eq!(
            utility_pool().current_num_threads(),
            rayon::current_num_threads()
        );
    }
}
