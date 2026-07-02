//! `rseq-mcu-sim` 二进制入口:作为"模拟 MCU"在串口或回环管道上接收字节码并执行。
//!
//! 用法:
//! - `rseq-mcu-sim --self-test`        进程内端到端自检(编译→下发→执行→比对轨迹)
//! - `rseq-mcu-sim --serial PATH [BAUD]`  打开串口,进入 `mcu_loop`(需 `serial` feature)

#[cfg(feature = "serial")]
use std::sync::Arc;
#[cfg(feature = "serial")]
use std::sync::atomic::AtomicBool;

use rseq_mcu_sim::run_self_test;
#[cfg(feature = "serial")]
use rseq_mcu_sim::{SimBus, mcu_loop};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--self-test") => match run_self_test() {
            Ok(()) => println!("rseq-mcu-sim: self-test passed"),
            Err(e) => {
                eprintln!("rseq-mcu-sim: self-test FAILED: {e}");
                std::process::exit(1);
            }
        },
        Some("--serial") => {
            #[cfg(feature = "serial")]
            {
                let path = match args.get(2) {
                    Some(p) => p.as_str(),
                    None => {
                        eprintln!("usage: rseq-mcu-sim --serial PATH [BAUD]");
                        std::process::exit(2);
                    }
                };
                let baud: u32 = args
                    .get(3)
                    .map(|s| s.parse().unwrap_or(115_200))
                    .unwrap_or(115_200);
                let transport = match rseq_link::SerialTransport::open(path, baud) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("open serial {path} failed: {e}");
                        std::process::exit(1);
                    }
                };
                // stop 仅占位:当前由 Ctrl-C 终止进程;真实部署可接信号写入此标志。
                let stop = Arc::new(AtomicBool::new(false));
                if let Err(e) = mcu_loop(transport, SimBus::new(), stop) {
                    eprintln!("rseq-mcu-sim: mcu_loop error: {e}");
                    std::process::exit(1);
                }
            }
            #[cfg(not(feature = "serial"))]
            {
                eprintln!(
                    "rseq-mcu-sim: --serial 需要以 `serial` feature 编译 \
                     (cargo run -p rseq-mcu-sim --features serial -- --serial ...)"
                );
                std::process::exit(2);
            }
        }
        _ => {
            eprintln!("usage: rseq-mcu-sim --self-test | --serial PATH [BAUD]");
            std::process::exit(2);
        }
    }
}
