mod child;
mod concurrency;
mod content;
mod existing;
mod git_import;
mod harness;
mod history;
mod lifecycle;
mod restoration;
mod support;
mod upgrade;

#[test]
fn child_process_entry() {
    child::run_if_requested();
}
