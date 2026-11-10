mod child;
mod concurrency;
mod existing;
mod harness;
mod lifecycle;
mod support;

#[test]
fn child_process_entry() {
    child::run_if_requested();
}
