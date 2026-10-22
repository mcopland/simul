//! Stand-in agent for integration tests: prints a recorded screen, then idles. Tests copy
//! the binary to a file named after the agent so tmux reports that name as the command.

use std::io::Write;
use std::time::Duration;

fn main() {
    let Some(screen) = std::env::args().nth(1) else {
        eprintln!("usage: fake_agent <screen-file>");
        std::process::exit(2);
    };
    match std::fs::read(&screen) {
        Ok(bytes) => {
            if let Err(e) = std::io::stdout().write_all(&bytes) {
                eprintln!("fake_agent: failed to write screen: {e}");
            }
            if let Err(e) = std::io::stdout().flush() {
                eprintln!("fake_agent: failed to flush screen: {e}");
            }
        }
        Err(e) => eprintln!("fake_agent: failed to read {screen}: {e}"),
    }
    std::thread::sleep(Duration::from_secs(600));
}
