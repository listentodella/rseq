//! 集成测试:经进程内回环管道(MockTransport)在主机 HostLink 与模拟 MCU 之间
//! 跑完整 LOAD→EXEC 流程,并比对回传的 BusOp 轨迹。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rseq::link::HostLink;
use rseq::trace::BusOp;
use rseq_link::MockTransport;
use rseq_link::wire::ExecStatus;
use rseq_mcu_sim::{SimBus, mcu_loop, run_self_test};

/// 复用库内的端到端自检:编译→下发→执行→比对轨迹。
#[test]
fn loopback_self_test_matches_expected_traces() {
    run_self_test().expect("loopback self-test should pass");
}

/// 直接驱动 mcu_loop + HostLink,显式比对一条 write+delay+write 轨迹。
#[test]
fn loopback_exec_traces_match() {
    let src = "write!(0x40, [0x01, 0x02, 0x03], 500);\nwrite!(0x100, 0xaa);\n";
    let program = rseq::parse(src).expect("parse");
    let bytecode = rseq::compile(&program).expect("compile");

    let (host_t, mcu_t) = MockTransport::pair();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_mcu = stop.clone();
    let _mcu = std::thread::Builder::new()
        .name("mcu-sim".into())
        .spawn(move || {
            let _ = mcu_loop(mcu_t, SimBus::new(), stop_mcu);
        })
        .expect("spawn mcu thread");

    let mut host = HostLink::new(host_t);
    host.load(&bytecode).expect("load");
    let res = host.exec().expect("exec");

    stop.store(true, Ordering::SeqCst);

    assert_eq!(res.status, ExecStatus::Ok);
    assert_eq!(
        res.traces,
        vec![
            BusOp::Write {
                addr: 0x40,
                data: vec![0x01, 0x02, 0x03]
            },
            BusOp::Delay { us: 500 },
            BusOp::Write {
                addr: 0x100,
                data: vec![0xaa]
            },
        ]
    );
}

/// Ping/Pong 经回环管道往返。
#[test]
fn loopback_ping_pong() {
    let (host_t, mcu_t) = MockTransport::pair();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_mcu = stop.clone();
    let _mcu = std::thread::spawn(move || {
        let _ = mcu_loop(mcu_t, SimBus::new(), stop_mcu);
    });
    let mut host = HostLink::new(host_t);
    host.ping().expect("ping should get pong");
    stop.store(true, Ordering::SeqCst);
}
