use dc_common::{init_logging, log, LogLevel};
use dc_protocol::PROTOCOL_VERSION;

fn main() {
    init_logging();
    log(
        LogLevel::Info,
        "direct-computing",
        &format!(
            "Direct Computing bootstrap (protocol {}.{})",
            PROTOCOL_VERSION.major, PROTOCOL_VERSION.minor
        ),
    );
}
