//! Deterministic fixtures describe audio and provider data, never endpoint decisions.
use crate::{
    audio::AudioFrame,
    clock::Clock,
    fake::FakeProviders,
    playback::CountingSink,
    session::{Session, SessionConfig, SessionError, SessionHandle, SessionReport},
};
use std::rc::Rc;

pub async fn feed(
    handle: &SessionHandle,
    clock: &dyn Clock,
    sequence: &mut u64,
    duration_ms: u64,
    amplitude: i16,
    truth: Option<bool>,
) -> Result<(), SessionError> {
    assert_eq!(duration_ms % 20, 0);
    for _ in 0..duration_ms / 20 {
        let timestamp = clock.now_ms();
        clock.sleep_until(timestamp + 20).await;
        handle
            .send_measured_audio(
                AudioFrame::new(*sequence, timestamp, vec![amplitude; 320])?,
                truth,
            )
            .await?;
        *sequence += 1;
    }
    Ok(())
}

pub async fn wait_until(
    handle: &mut SessionHandle,
    predicate: impl Fn(&crate::session::Snapshot) -> bool,
) -> Result<(), SessionError> {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let status = handle.snapshot();
            if status.closed {
                return Err(SessionError::Closed);
            }
            if predicate(&status) {
                return Ok(());
            }
            handle.changed().await?;
        }
    })
    .await
    .map_err(|_| SessionError::ScenarioDeadline)?
}

pub async fn run(name: &str, mut config: SessionConfig, clock: Rc<dyn Clock>) -> SessionReport {
    config.session_id = format!("scenario-{name}");
    let (session, mut handle) = Session::new(
        config,
        Rc::new(FakeProviders::default()),
        clock.clone(),
        Box::<CountingSink>::default(),
    )
    .expect("validated scenario configuration");
    let owner = tokio_util::task::AbortOnDropHandle::new(tokio::task::spawn_local(session.run()));
    // Fault-injected scenarios still return a joined, inspectable trace on early exit.
    let outcome: Result<(), SessionError> = async {
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
            .await?;
            feed(&handle, clock.as_ref(), &mut sequence, 700, 0, Some(false)).await?;
            feed(
                &handle,
                clock.as_ref(),
                &mut sequence,
                400,
                2000,
                Some(true),
            )
            .await?;
        } else {
            feed(
                &handle,
                clock.as_ref(),
                &mut sequence,
                800,
                2000,
                Some(true),
            )
            .await?;
        }
        feed(&handle, clock.as_ref(), &mut sequence, 500, 0, Some(false)).await?;
        wait_until(&mut handle, |s| s.started_replies == 1).await?;
        if ["C", "D", "E"].contains(&name) {
            clock
                .sleep_until(handle.snapshot().first_audio_ms.expect("playback started") + 1200)
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
                .await?;
            } else {
                feed(
                    &handle,
                    clock.as_ref(),
                    &mut sequence,
                    600,
                    2000,
                    Some(true),
                )
                .await?;
            }
            feed(&handle, clock.as_ref(), &mut sequence, 500, 0, Some(false)).await?;
        }
        wait_until(&mut handle, |s| s.completed_replies >= 1).await?;
        Ok(())
    }
    .await;
    if outcome.is_err() {
        handle.close_with_reason("scenario_incomplete");
    } else {
        handle.close();
    }
    owner.await.expect("supervised session owner")
}
