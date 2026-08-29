//! Fuzz targets for the IPC ring: modelled against a reference queue,
//! verifying FIFO order, capacity and wrap-around behaviour.

use proptest::prelude::*;

use kairos_core::ipc::{ChannelCore, Message, MSG_WORDS};

#[derive(Clone, Copy, Debug)]
enum Op {
    Push(u64),
    Pop,
    Peek,
    Clear,
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    #[test]
    fn channel_matches_reference_model(ops in prop::collection::vec(
        prop_oneof![
            any::<u64>().prop_map(Op::Push),
            Just(Op::Pop),
            Just(Op::Peek),
            Just(Op::Clear),
        ],
        0..2048,
    )) {
        let mut ch = ChannelCore::new();
        let mut model: std::collections::VecDeque<u64> = std::collections::VecDeque::new();

        for op in ops {
            match op {
                Op::Push(v) => {
                    let msg = Message::data(v as u16, [v; MSG_WORDS]);
                    let ok = ch.push(msg).is_ok();
                    let model_ok = model.len() < kairos_core::ipc::CHANNEL_CAPACITY;
                    assert_eq!(ok, model_ok, "push result must match capacity model");
                    if ok {
                        model.push_back(v);
                    }
                }
                Op::Pop => {
                    let got = ch.pop().map(|m| m.words[0]);
                    let want = model.pop_front();
                    assert_eq!(got, want, "FIFO pop must match model");
                }
                Op::Peek => {
                    let got = ch.peek().map(|m| m.words[0]);
                    let want = model.front().copied();
                    assert_eq!(got, want, "peek must match model head");
                }
                Op::Clear => {
                    ch.clear();
                    model.clear();
                }
            }
            assert_eq!(ch.len(), model.len(), "length must match model");
            assert_eq!(ch.is_full(), model.len() == kairos_core::ipc::CHANNEL_CAPACITY);
        }
    }
}

#[test]
fn wrap_around_never_loses_messages() {
    let mut ch = ChannelCore::new();
    for i in 0..kairos_core::ipc::CHANNEL_CAPACITY {
        assert!(ch.push(Message::data(i as u16, [i as u64; MSG_WORDS])).is_ok());
    }
    assert!(ch.push(Message::data(999, [0; MSG_WORDS])).is_err());
    for i in 0..kairos_core::ipc::CHANNEL_CAPACITY {
        let m = ch.pop().expect("msg");
        assert_eq!(m.words[0], i as u64);
    }
    assert!(ch.pop().is_none());
}