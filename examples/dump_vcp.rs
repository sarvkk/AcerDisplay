//! Diagnostic: read/set raw VCP values on a monitor.
//! Usage:
//!   cargo run --example dump_vcp            # dump caps + current values (i2c-9)
//!   cargo run --example dump_vcp -- get E2  # read one code
//!   cargo run --example dump_vcp -- set E2 00  # write one code, read back
use ddc::Ddc;

fn hex(s: &str) -> u8 {
    let s = s.trim_start_matches("0x").trim_start_matches("0X");
    u8::from_str_radix(s, 16).expect("bad hex")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = "/dev/i2c-9";
    let mut dev = ddc_i2c::from_i2c_device(path).expect("open i2c device");

    match args.first().map(|s| s.as_str()) {
        Some("get") => {
            let code = hex(&args[1]);
            match dev.get_vcp_feature(code) {
                Ok(v) => println!("0x{code:02X}: value={} max={}", v.value(), v.maximum()),
                Err(e) => println!("0x{code:02X}: ERR {e}"),
            }
        }
        Some("set") => {
            let code = hex(&args[1]);
            let value =
                u16::from_str_radix(args[2].trim_start_matches("0x"), 16).expect("bad value");
            let before = dev.get_vcp_feature(code).map(|v| v.value()).ok();
            dev.set_vcp_feature(code, value).expect("set failed");
            std::thread::sleep(std::time::Duration::from_millis(300));
            let after = dev.get_vcp_feature(code).map(|v| v.value()).ok();
            println!("0x{code:02X}: {before:?} -> set {value} -> read {after:?}");
        }
        _ => {
            let caps = dev.capabilities_string().expect("capabilities string");
            println!("caps: {:?}", String::from_utf8_lossy(&caps));
            for c in [
                0x04u8, 0x10, 0x12, 0x14, 0x16, 0x18, 0x1A, 0x59, 0x5A, 0x5B, 0x5C, 0x5D, 0x5E,
                0x60, 0x62, 0x8D, 0x9B, 0x9C, 0x9D, 0x9E, 0x9F, 0xA0, 0xCC, 0xD6, 0xE0, 0xE1, 0xE2,
                0xE3, 0xE4, 0xE5, 0xE7, 0xE8,
            ] {
                match dev.get_vcp_feature(c) {
                    Ok(v) => println!("0x{c:02X}: value={:5} max={:5}", v.value(), v.maximum()),
                    Err(e) => println!("0x{c:02X}: ERR {e}"),
                }
            }
        }
    }
}
