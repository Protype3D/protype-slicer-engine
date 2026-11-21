//! Request-local attribution. Tokio tasks and blocking workers do not inherit it.
use slicer_types::StepTag;
use std::future::Future;

tokio::task_local! { static STEP_TAG: StepTag; }

pub async fn with_tag<F: Future>(tag: StepTag, future: F) -> F::Output {
    STEP_TAG.scope(tag, future).await
}

pub fn with_tag_sync<R>(tag: StepTag, f: impl FnOnce() -> R) -> R {
    STEP_TAG.sync_scope(tag, f)
}

pub fn current_tag() -> StepTag {
    STEP_TAG.try_with(Clone::clone).unwrap_or_default()
}
