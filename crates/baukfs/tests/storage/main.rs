mod child;
mod concurrency;
mod content;
mod existing;
mod harness;
mod lifecycle;
mod support;
mod upgrade;

#[test]
fn child_process_entry() {
    child::run_if_requested();
}
