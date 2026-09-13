use voice_runtime::{
    provider::ProviderError,
    transcript::{AsrUpdate, Transcript},
};

fn update(revision: u64, text: &str) -> AsrUpdate {
    AsrUpdate {
        revision,
        through_sequence: revision * 10,
        text: text.into(),
        ..Default::default()
    }
}
#[test]
fn revisions_replace_text_without_rewriting_committed_segments() {
    let mut transcript = Transcript::default();
    assert!(transcript.apply(&update(1, "Wednesday."), 128).unwrap());
    assert!(transcript.apply(&update(3, "Friday…"), 128).unwrap());
    assert!(!transcript.apply(&update(2, "Wednesday."), 128).unwrap());
    assert_eq!(transcript.text(), "Friday…");
    let mut final_segment = update(4, "Friday ");
    final_segment.is_final = true;
    final_segment.stable_prefix_bytes = final_segment.text.len();
    transcript.apply(&final_segment, 128).unwrap();
    assert!(transcript.apply(&update(5, "Monday "), 128).is_err());
    let mut next = update(1, "afternoon.");
    next.segment_id = 1;
    next.through_sequence = 50;
    transcript.apply(&next, 128).unwrap();
    assert_eq!(transcript.text(), "Friday afternoon.");
    let before = transcript.text().to_owned();
    let mut too_large = next.clone();
    too_large.revision = 2;
    too_large.text = "x".repeat(200);
    assert!(matches!(
        transcript.apply(&too_large, 128),
        Err(ProviderError::Protocol(_))
    ));
    assert_eq!(transcript.text(), before);
}
#[test]
fn stable_prefix_and_metadata_are_validated_transactionally() {
    let mut transcript = Transcript::default();
    let mut first = update(1, "周五 afternoon");
    first.stable_prefix_bytes = 6;
    transcript.apply(&first, 128).unwrap();
    for (text, prefix, stability) in [
        ("周四 afternoon", 6, None),
        ("周五 afternoon", 1, None),
        ("周五 afternoon", 6, Some(f32::NAN)),
    ] {
        let mut next = update(2, text);
        next.stable_prefix_bytes = prefix;
        next.stability = stability;
        assert!(transcript.apply(&next, 128).is_err());
        assert_eq!(transcript.text(), first.text);
    }
}
