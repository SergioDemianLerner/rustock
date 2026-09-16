//! Identify a Bridge method from its 4-byte ABI selector, using the node's own
//! table rather than a guess.
//!
//! Usage: bridge_method <selector-hex>
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let bytes = hex::decode(a[0].trim_start_matches("0x")).expect("hex selector");
    let sel: [u8; 4] = bytes[..4].try_into().expect("4 bytes");
    match rustock_execution::bridge::find_bridge_method(&sel) {
        Some(m) => println!("0x{}  {}\n  signature : {}\n  gas       : {:?}\n  permission: {:?}\n  enabled   : {:?}",
                            hex::encode(sel), m.name, m.signature, m.gas_cost, m.permission, m.enabled),
        None => println!("0x{}  <no Bridge method with this selector>", hex::encode(sel)),
    }
}
