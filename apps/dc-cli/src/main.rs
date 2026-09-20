use dc_protocol::PROTOCOL_VERSION;

fn main() {
    println!(
        "dc-cli (protocol {}.{})",
        PROTOCOL_VERSION.major, PROTOCOL_VERSION.minor
    );
}
