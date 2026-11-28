//! Host-only Wise sandbox payment bridge for Beankeeper.

mod wise_sandbox;

fn main() -> std::process::ExitCode {
    match wise_sandbox::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
