//! Deterministic fixtures describe audio and provider data, never endpoint decisions.
use crate::{
    audio::AudioFrame,
    clock::Clock,
    fake::FakeProviders,
    playback::CountingSink,
    session::{Session, SessionConfig, SessionHandle, SessionReport},
};
use std::rc::Rc;

pub async fn feed(
    handle: &SessionHandle,
    clock: &dyn Clock,
    sequence: &mut u64,
    duration_ms: u64,
    amplitude: i16,
    truth: Option<bool>,
) {
    assert_eq!(duration_ms % 20, 0);
    for _ in 0..duration_ms / 20 {
        let timestamp = clock.now_ms();
        clock.sleep_until(timestamp + 20).await;
        handle
            .send_measured_audio(
                AudioFrame::new(*sequence, timestamp, vec![amplitude; 320]).unwrap(),
                truth,
            )
            .await
            .unwrap();
        *sequence += 1;
    }
}

pub async fn wait_until(
    handle: &mut SessionHandle,
    predicate: impl Fn(&crate::session::Snapshot) -> bool,
) {
    loop {
        let status = handle.snapshot();
        assert!(!status.closed, "session closed before scenario completed");
        if predicate(&status) {
            break;
        }
        handle
            .changed()
            .await
            .expect("session ended before expected state");
    }
}

pub async fn run(name: &str, mut config: SessionConfig, clock: Rc<dyn Clock>) -> SessionReport {
    config.session_id = format!("scenario-{name}");
    let (session, mut handle) = Session::new(
        config,
        Rc::new(FakeProviders::default()),
        clock.clone(),
        Box::<CountingSink>::default(),
    )
    .unwrap();
    let owner = tokio::task::spawn_local(session.run());
    let mut sequence = 0;
    if name == "B" {
        feed(
            &handle,
            clock.as_ref(),
            &mut sequence,
            400,
            2000,
            Some(true),
        )
        .await;
        feed(&handle, clock.as_ref(), &mut sequence, 700, 0, Some(false)).await;
        assert_eq!(
            handle.snapshot().started_replies,
            0,
            "hesitation caused playback"
        );
        feed(
            &handle,
            clock.as_ref(),
            &mut sequence,
            400,
            2000,
            Some(true),
        )
        .await;
    } else {
        feed(
            &handle,
            clock.as_ref(),
            &mut sequence,
            800,
            2000,
            Some(true),
        )
        .await;
    }
    feed(&handle, clock.as_ref(), &mut sequence, 500, 0, Some(false)).await;
    wait_until(&mut handle, |s| s.started_replies == 1).await;
    if ["C", "D", "E"].contains(&name) {
        clock
            .sleep_until(handle.snapshot().first_audio_ms.unwrap() + 1200)
            .await;
        if name == "D" {
            feed(
                &handle,
                clock.as_ref(),
                &mut sequence,
                80,
                12_000,
                Some(false),
            )
            .await;
        } else {
            feed(
                &handle,
                clock.as_ref(),
                &mut sequence,
                600,
                2000,
                Some(true),
            )
            .await;
        }
        feed(&handle, clock.as_ref(), &mut sequence, 500, 0, Some(false)).await;
    }
    wait_until(&mut handle, |s| s.completed_replies >= 1).await;
    handle.close();
    owner.await.unwrap()
}
