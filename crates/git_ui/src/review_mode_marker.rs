//! Marker global set by the claude-review fork's review-mode bootstrap
//! (`zed --review`). Lives in `git_ui` so the Project Diff toolbar can
//! check it without taking a dep on the `zed` crate (which is
//! downstream).

use gpui::{App, Global};
use std::sync::Arc;

type SendReviewCallback = Arc<dyn Fn(&mut App) + Send + Sync + 'static>;

struct ReviewModeMarker {
    callback: Option<SendReviewCallback>,
}

impl Global for ReviewModeMarker {}

/// Marks the running app as launched via `zed --review`. The
/// `send_review_callback` is invoked from the project-diff toolbar
/// when the user clicks "Send Review to Agent". Bypasses gpui's
/// action dispatch (which proved racy with the toolbar's focus
/// model) and just calls the closure directly on `&mut App`.
pub fn activate(cx: &mut App, send_review_callback: SendReviewCallback) {
    cx.set_global(ReviewModeMarker {
        callback: Some(send_review_callback),
    });
}

/// True iff [`activate`] has been called this session.
pub fn is_active(cx: &App) -> bool {
    cx.try_global::<ReviewModeMarker>().is_some()
}

/// Calls the registered `Send Review` callback. No-op if review mode
/// isn't active.
pub fn dispatch_send_review(cx: &mut App) {
    let Some(callback) = cx
        .try_global::<ReviewModeMarker>()
        .and_then(|m| m.callback.clone())
    else {
        return;
    };
    callback(cx);
}
