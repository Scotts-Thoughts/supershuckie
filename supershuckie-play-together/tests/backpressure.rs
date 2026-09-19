//! A peer that stops reading must not stall the publisher or the other peers, and the writer
//! must merge queued streams.

mod common;

use std::io::{Cursor, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use supershuckie_play_together::conn::{encode_outbound, Outbound, OutboundQueue, PushError, MAX_QUEUE_BYTES};
use supershuckie_play_together::protocol::Message;
use supershuckie_play_together::*;
use supershuckie_replay_recorder::replay_file::{ReplayConsoleType, REPLAY_VERSION};

#[test]
fn a_slow_peer_is_dropped_and_nobody_else_notices() {
    let _wd = watchdog(60, "a_slow_peer_is_dropped_and_nobody_else_notices");
    let host = HostSession::bind(HostConfig { idle_timeout: Duration::from_secs(2), ..host_config() }, local("Host")).unwrap();
    let mut hl = Vec::new();
    wait_connected(&host, &mut hl);

    // A healthy client that follows the host.
    let healthy = connect_client(&host, "Healthy");
    let mut cl = Vec::new();
    wait_connected(&healthy, &mut cl);
    wait_joined(&host, &mut hl);
    let (rec, sink) = counting_sink();
    healthy.subscribe(1, sink).unwrap();
    assert_eq!(wait_snapshot_requested(&host, &mut hl), vec![2]);
    host.publisher().publish_snapshot(snapshot_at(0), 2).unwrap();

    // A raw peer that handshakes and then never reads.
    let mut slow = TcpStream::connect_timeout(&host.local_addr(), Duration::from_secs(2)).unwrap();
    slow.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    slow.write_all(
        &Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            replay_version: REPLAY_VERSION,
            app_version: "raw".to_owned(),
            display_name: "Slow".to_owned(),
            color: 0,
            publisher: publisher_info(ReplayConsoleType::GameBoy),
        }
        .encoded(),
    )
    .unwrap();
    match Message::read(&mut slow) {
        Ok(Some(Ok(Message::Welcome { your_peer_id, .. }))) => assert_eq!(your_peer_id, 3),
        other => panic!("{other:?}"),
    }
    wait_joined(&host, &mut hl);
    // ...and never reads again.

    // Publish ~128 MiB in 64 KiB frames, timing every call.
    let publisher = host.publisher();
    let chunk = big_frame(64 << 10);
    let calls = (128usize << 20) / chunk.len();
    let mut worst = Duration::ZERO;
    let mut over_5ms = 0usize;
    let started = Instant::now();
    for i in 0..calls {
        let t = Instant::now();
        publisher.publish(i as u64, chunk.clone()).unwrap();
        let took = t.elapsed();
        worst = worst.max(took);
        if took > Duration::from_millis(5) {
            over_5ms += 1;
        }
        // Do not outrun the healthy client by more than the queue allows: pace to its progress.
        if i % 64 == 0 {
            while rec.lock().unwrap().frames + 512 < i as u64 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    eprintln!("{calls} publishes of {} bytes in {:?}; worst {worst:?}; {over_5ms} over 5 ms", chunk.len(), started.elapsed());
    assert!(over_5ms <= calls / 1000, "{over_5ms} of {calls} publish calls took more than 5 ms (worst {worst:?})");
    assert!(worst < Duration::from_millis(100), "publish blocked for {worst:?}");

    match wait_for(&host, &mut hl, "Left{TooSlow}", |e| matches!(e, SessionEvent::Left { .. })) {
        SessionEvent::Left { peer_id, reason } => assert_eq!((peer_id, reason), (3, LeaveReason::TooSlow)),
        other => panic!("{other:?}"),
    }
    wait_until("the healthy client received everything", || rec.lock().unwrap().frames == calls as u64);
    assert_eq!(rec.lock().unwrap().packet_bytes, calls as u64 * (64 << 10) + calls as u64);
    assert!(healthy.is_connected());
    assert!(host.stats().outbound_queued_bytes < MAX_QUEUE_BYTES as u64);

    // The slow socket was shut down by the host.
    slow.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut sink_buf = vec![0u8; 1 << 16];
    let eof = loop {
        match slow.read(&mut sink_buf) {
            Ok(0) => break true,
            Ok(_) => continue,
            Err(_) => break true,
        }
    };
    assert!(eof);
}

#[test]
fn queued_streams_are_merged_into_one_message() {
    let queue = OutboundQueue::new();
    let mut expected = Vec::new();
    for i in 0..50u64 {
        let bytes = frames(2);
        expected.extend_from_slice(&bytes);
        queue.try_push(Outbound::Stream { first_frame: 100 + i * 2, bytes: Arc::new(bytes) }).unwrap();
    }
    assert!(queue.queued_bytes() > 0);
    let drained = queue.wait_drain(Duration::ZERO);
    assert_eq!(drained.items.len(), 50);
    assert!(!drained.closed);
    let batch = encode_outbound(&drained.items, 7);
    assert_eq!(batch.messages, 1);
    let mut cursor = Cursor::new(batch.bytes);
    assert_eq!(Message::read(&mut cursor).unwrap(), Some(Ok(Message::Stream { from: 7, first_frame: 100, bytes: expected })));
    assert_eq!(Message::read(&mut cursor).unwrap(), None);
    assert_eq!(queue.queued_bytes(), 0);

    // Something in between splits the run; big items go out alone.
    let items = vec![
        Outbound::Stream { first_frame: 0, bytes: Arc::new(frames(1)) },
        Outbound::Encoded(Arc::new(Message::SyncHash { from: 7, frame: 1, hash: [0; 32] }.encoded())),
        Outbound::Stream { first_frame: 1, bytes: Arc::new(frames(1)) },
        Outbound::Stream { first_frame: 2, bytes: Arc::new(big_frame(300 << 10)) },
        Outbound::Stream { first_frame: 3, bytes: Arc::new(frames(1)) },
        Outbound::Snapshot { snapshot: Arc::new(supershuckie_play_together::conn::SnapshotItem::new(snapshot_at(4))), target: 0 },
    ];
    let batch = encode_outbound(&items, 7);
    assert_eq!(batch.messages, 5);
    let mut cursor = Cursor::new(batch.bytes);
    let mut kinds = Vec::new();
    while let Some(m) = Message::read(&mut cursor).unwrap() {
        let m = m.unwrap();
        kinds.push(match &m {
            Message::Stream { first_frame, bytes, .. } => format!("stream@{first_frame}:{}", bytes.len()),
            Message::SyncHash { .. } => "hash".to_owned(),
            Message::Snapshot(s) => format!("snapshot@{}", s.frame),
            other => panic!("{other:?}"),
        });
    }
    assert_eq!(
        kinds,
        vec![
            format!("stream@0:{}", frames(1).len()),
            "hash".to_owned(),
            format!("stream@1:{}", frames(1).len() + big_frame(300 << 10).len()),
            format!("stream@3:{}", frames(1).len()),
            "snapshot@4".to_owned(),
        ]
    );
}

#[test]
fn the_urgent_lane_goes_out_first_and_between_runs() {
    let queue = OutboundQueue::new();
    // Four 100 KiB streams: merged into a 300 KiB message and a 100 KiB one.
    for i in 0..4u64 {
        queue.try_push(Outbound::Stream { first_frame: i, bytes: Arc::new(vec![0u8; 100 << 10]) }).unwrap();
    }
    let frame = Arc::new(Message::LinkFrame { from: 2, target: 3, frame: 1, elapsed_millis: 0, events: vec![], pair_hash_frame: 0, pair_hash: [0; 32] }.encoded());
    queue.try_push_urgent(Arc::clone(&frame)).unwrap();
    assert_eq!(queue.queued_bytes(), 4 * ((100 << 10) + 16) + frame.len() as u64);
    let drained = queue.wait_drain(Duration::ZERO);
    assert_eq!(drained.urgent.len(), 1, "the urgent lane comes out with the drain");
    assert!(Arc::ptr_eq(&drained.urgent[0], &frame));
    assert_eq!(drained.items.len(), 4);
    assert_eq!(queue.queued_bytes(), 0);
    let batch = encode_outbound(&drained.items, 2);
    assert_eq!(batch.messages, 2);
    assert_eq!(batch.boundaries.len(), 2);
    assert_eq!(batch.boundaries[1], batch.bytes.len());
    // Runs end on message boundaries: the writer looks at the urgent lane between them.
    let runs = batch.runs();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0], 0..batch.boundaries[0]);
    assert_eq!(runs[1], batch.boundaries[0]..batch.bytes.len());
    // Many small messages are grouped into runs of about WRITE_RUN.
    let items: Vec<Outbound> = (0..100).map(|_| Outbound::Encoded(Arc::new(vec![0u8; 2 << 10]))).collect();
    let batch = encode_outbound(&items, 2);
    let runs = batch.runs();
    assert_eq!(runs.len(), 4, "{runs:?}");
    assert!(runs.iter().all(|r| r.len() % (2 << 10) == 0));
    assert_eq!(runs.last().unwrap().end, batch.bytes.len());
    // An empty batch has no runs.
    assert!(encode_outbound(&[], 2).runs().is_empty());
    // `take_urgent` between runs, without waiting.
    assert!(queue.take_urgent().is_empty());
    queue.try_push_urgent(Arc::clone(&frame)).unwrap();
    queue.try_push_urgent(Arc::clone(&frame)).unwrap();
    assert_eq!(queue.take_urgent().len(), 2);
    assert_eq!(queue.queued_bytes(), 0);
    // Closing with discard drops the urgent lane too.
    queue.try_push_urgent(frame).unwrap();
    queue.close(true);
    let drained = queue.wait_drain(Duration::ZERO);
    assert!(drained.closed && drained.urgent.is_empty() && drained.items.is_empty());
}

#[test]
fn the_queue_overflows_cleanly() {
    let queue = OutboundQueue::new();
    let big = Arc::new(vec![0u8; 1 << 20]);
    let mut pushed = 0;
    loop {
        match queue.try_push(Outbound::Encoded(Arc::clone(&big))) {
            Ok(()) => pushed += 1,
            Err(PushError::Full { queued_bytes }) => {
                assert_eq!(queued_bytes, pushed as u64 * (1 << 20));
                break;
            }
            Err(PushError::Closed) => panic!("not closed"),
        }
    }
    assert_eq!(pushed, MAX_QUEUE_BYTES >> 20);
    queue.close(true);
    assert!(queue.is_closed());
    assert_eq!(queue.queued_bytes(), 0);
    assert_eq!(queue.try_push(Outbound::Encoded(big)), Err(PushError::Closed));
    let drained = queue.wait_drain(Duration::from_millis(10));
    assert!(drained.closed && drained.items.is_empty());
}
