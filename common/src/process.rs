/// Exit status of a process-lifetime root task.
///
/// Panics surface through the `JoinError` yielded by the supervising
/// `JoinSet` (the caller resumes them); this value marks a task that ran to
/// completion on its own, which for a root-owned task is unexpected.
#[derive(Debug)]
pub enum ProcessTaskExit {
    Completed { task: &'static str },
    Failed { task: &'static str, detail: String },
}
