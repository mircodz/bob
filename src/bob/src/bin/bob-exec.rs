#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = bob::native_worker::worker_main().await {
        eprintln!("bob-exec: {error:#}");
        std::process::exit(1);
    }
}
