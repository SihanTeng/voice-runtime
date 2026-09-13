use voice_runtime::audio::*;

#[test]
fn validates_shape_order_and_explicit_gaps() {
    assert!(AudioFrame::new(0, 0, vec![0; 319]).is_err());
    let mut validator = FrameValidator::default();
    let first = AudioFrame::new(1, 20, vec![0; 320]).unwrap();
    assert_eq!(validator.accept(&first), Ok(0));
    assert_eq!(validator.accept(&first), Err(AudioError::NonMonotonic));
    assert_eq!(
        validator.accept(&AudioFrame::new(4, 80, vec![0; 320]).unwrap()),
        Ok(2)
    );
}
