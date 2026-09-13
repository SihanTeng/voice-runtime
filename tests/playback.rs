use voice_runtime::{event::Identity, playback::*};

fn packet(id: Identity, sequence: u64, start: u64, word_offset: usize) -> Packet {
    Packet {
        chunk: AudioChunk {
            identity: id,
            sequence,
            text_range: 0..7,
            word_samples: 640,
            word_offset,
            sample_start: start,
            samples: vec![2; 320],
        },
        budget: None,
    }
}

#[test]
fn stop_between_ticks_counts_actual_samples_and_excludes_partial_word() {
    let id = Identity {
        turn_id: 1,
        generation_id: 1,
    };
    let mut p = Playback::new(Box::<CountingSink>::default(), 8000, 100, 2);
    p.activate(id).unwrap();
    p.append_text(id, "Friday ", 100).unwrap();
    p.enqueue(packet(id, 0, 0, 0)).unwrap();
    p.enqueue(packet(id, 1, 320, 320)).unwrap();
    p.start_ready(100);
    assert_eq!(p.settle(120).unwrap().unwrap().samples, 320);
    p.start_ready(120);
    assert_eq!(p.stop(125, true).unwrap().unwrap().samples, 80);
    assert_eq!(p.replies[0].played_samples(), 400);
    assert_eq!(p.replies[0].heard_text(), "");
    assert_eq!(p.replies[0].enqueued_text(), "Friday ");
    assert!(p.replies[0].chunks[1].truncated);
    assert!(p.is_idle());
    assert!(p.enqueue(packet(id, 2, 640, 0)).is_err());
    p.close(200).unwrap();
    assert!(p.activate(id).is_err());
    assert!(p.settle(1000).unwrap().is_none());
}

#[test]
fn complete_word_is_heard_and_duplicate_or_invalid_utf8_ranges_are_rejected() {
    let id = Identity {
        turn_id: 1,
        generation_id: 1,
    };
    let mut p = Playback::new(Box::<CountingSink>::default(), 8000, 100, 2);
    p.activate(id).unwrap();
    p.append_text(id, "Friday ", 100).unwrap();
    p.enqueue(packet(id, 0, 0, 0)).unwrap();
    assert!(p.enqueue(packet(id, 0, 0, 0)).is_err());
    p.enqueue(packet(id, 1, 320, 320)).unwrap();
    p.start_ready(0);
    p.settle(20).unwrap();
    p.start_ready(20);
    p.settle(40).unwrap();
    assert_eq!(p.replies[0].heard_text(), "Friday ");
}
