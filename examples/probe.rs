//! Protocol exploration helper (not part of the product).
//!
//!   probe cfg <from> <to>        read config ids one at a time
//!   probe raw <op> <app> <hex>   send one command, print the response
//!   probe listen <secs>          print every unsolicited frame
//!   probe info                   GET_TARGET_INFO / RUN_INFO per attribute bit

use redmi_buds::auth::{hex_decode, hex_encode};
use redmi_buds::client::{find_paired_buds, Buds};
use redmi_buds::rcsp;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = std::env::var("V").is_ok();
    let mac = std::env::var("MAC").ok().or_else(find_paired_buds).expect("no paired buds; set MAC=");
    let mut b = (0..10)
        .find_map(|_| {
            Buds::connect(&mac, Some(6), verbose)
                .map_err(|_| std::thread::sleep(std::time::Duration::from_millis(700)))
                .ok()
        })
        .expect("connect");
    b.authenticate().expect("auth");
    match args[0].as_str() {
        "cfg" => {
            let (from, to): (u16, u16) = (args[1].parse().unwrap(), args[2].parse().unwrap());
            for id in from..=to {
                match b.get_config(&[id]) {
                    Ok(v) if !v.is_empty() => {
                        for (cid, val) in v {
                            println!("{cid:4} {:24} {}", rcsp::config_id_name(cid), hex_encode(&val));
                        }
                    }
                    Ok(_) => {}
                    Err(e) => println!("{id:4} ERR {e}"),
                }
            }
        }
        "raw" => {
            let op = u8::from_str_radix(&args[1], 16).unwrap();
            let app: u8 = args[2].parse().unwrap();
            let params = hex_decode(args.get(3).map(String::as_str).unwrap_or("")).unwrap();
            let sn = b.send_command(op, &params, app).unwrap();
            match b.wait_response(sn, Some(op), 4000) {
                Ok(f) => println!("status={} data={}", rcsp::status_name(f.status()), hex_encode(f.data())),
                Err(e) => println!("ERR {e}"),
            }
        }
        "listen" => {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(args[1].parse().unwrap());
            while std::time::Instant::now() < end {
                if let Ok(Some(f)) = b.recv_frame(1000) {
                    println!("{} {} app={} {}", if f.is_command() {"cmd"} else {"rsp"}, f.opcode_name(), f.target_app(), hex_encode(&f.payload));
                }
            }
        }
        "info" => {
            for op in [0x02u8, 0x09] {
                for bit in 0..32u32 {
                    let mask = (1u32 << bit).to_be_bytes();
                    let sn = b.send_command(op, &mask, 4).unwrap();
                    if let Ok(f) = b.wait_response(sn, Some(op), 3000) {
                        let d = f.data();
                        if f.status() == 0 && !d.is_empty() {
                            println!("op {op:02x} bit {bit:2}: {}", hex_encode(d));
                        }
                    }
                }
            }
        }
        "try" => {
            // try <id> <hex>: write, read back, then restore the original value
            let id: u16 = args[1].parse().unwrap();
            let val = hex_decode(&args[2]).unwrap();
            let read = |b: &mut Buds| b.get_config(&[id]).ok().and_then(|v| v.into_iter().next()).map(|(_, v)| hex_encode(&v));
            let orig = b.get_config(&[id]).unwrap().into_iter().next().map(|(_, v)| v).unwrap_or_default();
            let w = b.set_config(&[(id, val.clone())]);
            let after = read(&mut b);
            let r = b.set_config(&[(id, orig.clone())]);
            let fin = read(&mut b);
            println!(
                "{id:4} orig={} write {} -> {:?}  readback={:?}  restore={:?} final={:?}",
                hex_encode(&orig), hex_encode(&val), w.map_err(|e| e.to_string()), after, r.map_err(|e| e.to_string()), fin
            );
        }
        _ => eprintln!("?"),
    }
}
