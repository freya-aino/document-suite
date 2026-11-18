//! Stage failure.

/// A stage's failure, and whether retrying it could help.
///
/// Transient errors are retried inside the process a bounded number of times; permanent
/// ones (bad input, an unsupported format) fail immediately. There is no dead-letter
/// queue any more, and so nothing redelivers a document later: a failure leaves the job
/// row `failed` with this message, and the caller re-POSTs the upload to try again. That
/// retry is cheap because the parts already done are not redone — rendered pages are
/// skipped when their signals are present, and every vault write is idempotent.
#[derive(Debug)]
pub struct TaskError {
    pub permanent: bool,
    pub error: anyhow::Error,
}

impl TaskError {
    pub fn permanent(error: impl Into<anyhow::Error>) -> Self {
        Self { permanent: true, error: error.into() }
    }
}

/// Anything `?`-converted is transient unless marked otherwise.
impl<E: Into<anyhow::Error>> From<E> for TaskError {
    fn from(e: E) -> Self {
        Self { permanent: false, error: e.into() }
    }
}
