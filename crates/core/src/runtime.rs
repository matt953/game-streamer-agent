//! Task and timer primitives that behave the same on native and wasm32.
//!
//! The shared client code runs on tokio natively and on the browser's event
//! loop under wasm-bindgen; neither `tokio::spawn` nor `tokio::time` exist
//! on the latter. Code that must build for both goes through these instead
//! of naming a runtime.

use std::future::Future;
use std::time::Duration;

/// Run `future` to completion in the background on the current runtime.
///
/// Natively this is `tokio::spawn` and requires a tokio runtime; on wasm32 it
/// is `spawn_local` on the browser event loop, so the future need not be
/// `Send` there. The `Send` bound is required on both targets so a caller's
/// code compiles identically for each.
pub fn spawn<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio::spawn(future);
    }
    #[cfg(target_arch = "wasm32")]
    {
        wasm_bindgen_futures::spawn_local(future);
    }
}

/// Sleep for `duration` without blocking the runtime.
pub async fn sleep(duration: Duration) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio::time::sleep(duration).await;
    }
    #[cfg(target_arch = "wasm32")]
    {
        // The browser timer takes whole milliseconds; anything shorter is a
        // yield to the event loop, which is the closest a page can get.
        #[allow(clippy::cast_possible_truncation)]
        let millis = duration.as_millis().min(u128::from(u32::MAX)) as u32;
        gloo_timers::future::TimeoutFuture::new(millis).await;
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sleep_waits_at_least_the_duration() {
        let start = crate::time::Instant::now();
        sleep(Duration::from_millis(20)).await;
        assert!(start.elapsed() >= Duration::from_millis(20));
    }

    #[tokio::test]
    async fn spawn_runs_the_future() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        spawn(async move {
            let _ = tx.send(7u8);
        });
        assert_eq!(rx.await.unwrap(), 7);
    }
}
