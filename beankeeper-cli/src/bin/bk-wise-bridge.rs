//! Host-only Wise payment bridge for Beankeeper.

mod wise_bridge;

fn main() -> std::process::ExitCode {
    match wise_bridge::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
