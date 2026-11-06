// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Background threads give way to typing (0.11).
//!
//! Index, semantic and job threads call [`lower_priority`] when they
//! start: the lowest priority the system gives a normal thread without
//! privileges (nice 19 for the thread on Linux, the lowest scheduling
//! priority on macOS, `THREAD_PRIORITY_LOWEST` on Windows), through
//! `thread-priority`'s safe API. On an idle machine they run as fast as
//! before; while the UI thread is busy, it goes first.

/// Lower the calling thread's priority. Failing is harmless (the thread
/// keeps its priority), so the error is ignored.
pub fn lower_priority() {
    let _ = thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Min);
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_thread_can_lower_its_own_priority() {
        let t = std::thread::spawn(|| {
            super::lower_priority();
            thread_priority::get_current_thread_priority().is_ok()
        });
        assert!(t.join().unwrap());
    }
}
