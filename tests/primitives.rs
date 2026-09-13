use std::rc::Rc;
use tokio_util::sync::CancellationToken;
use voice_runtime::{
    clock::{Clock, TokioClock},
    provider::{Pacer, ProviderError, Timing},
    queue,
};

#[tokio::test(start_paused = true)]
async fn idle_and_total_timeouts_exclude_downstream_waits() {
    let clock: Rc<dyn Clock> = Rc::new(TokioClock::default());
    let soft = CancellationToken::new();
    let hard = CancellationToken::new();
    let mut pacer = Pacer::new(
        Timing {
            first_ms: 10,
            interval_ms: 80,
            idle_timeout_ms: 50,
            ..Default::default()
        },
        clock.clone(),
    );
    pacer.next(&soft, &hard).await.unwrap();
    clock.sleep_until(1010).await; // Simulated downstream backpressure, not provider work.
    assert_eq!(pacer.next(&soft, &hard).await, Err(ProviderError::Timeout));
    assert_eq!(clock.now_ms(), 1060);
    let mut pacer = Pacer::new(
        Timing {
            first_ms: 40,
            interval_ms: 40,
            total_timeout_ms: 60,
            ..Default::default()
        },
        clock.clone(),
    );
    pacer.next(&soft, &hard).await.unwrap();
    assert_eq!(pacer.next(&soft, &hard).await, Err(ProviderError::Timeout));
    assert_eq!(clock.now_ms(), 1120);
}

#[tokio::test(start_paused = true)]
async fn soft_cancel_delivers_exactly_two_late_packets_but_hard_cancel_stops() {
    let clock: Rc<dyn Clock> = Rc::new(TokioClock::default());
    let mut pacer = Pacer::new(
        Timing {
            late_chunks: 2,
            ..Timing::default()
        },
        clock.clone(),
    );
    let soft = CancellationToken::new();
    let hard = CancellationToken::new();
    pacer.next(&soft, &hard).await.unwrap();
    assert_eq!(clock.now_ms(), 40);
    soft.cancel();
    pacer.next(&soft, &hard).await.unwrap();
    pacer.next(&soft, &hard).await.unwrap();
    assert_eq!(
        pacer.next(&soft, &hard).await,
        Err(ProviderError::Cancelled)
    );
    let mut pacer = Pacer::new(
        Timing {
            late_chunks: 10,
            ..Timing::default()
        },
        clock,
    );
    hard.cancel();
    assert_eq!(
        pacer.next(&soft, &hard).await,
        Err(ProviderError::Cancelled)
    );
}

#[tokio::test(start_paused = true)]
async fn a_stalled_provider_times_out_and_queue_backpressures() {
    let clock: Rc<dyn Clock> = Rc::new(TokioClock::default());
    let mut pacer = Pacer::new(
        Timing {
            stall_at: Some(0),
            ..Timing::default()
        },
        clock.clone(),
    );
    assert_eq!(
        pacer
            .next(&CancellationToken::new(), &CancellationToken::new())
            .await,
        Err(ProviderError::Timeout)
    );
    assert_eq!(clock.now_ms(), 1000);
    let (tx, mut rx, meter) = queue::channel("test", 1);
    tx.try_send(1).unwrap();
    assert!(tx.try_send(2).is_err());
    assert_eq!(rx.recv().await, Some(1));
    tx.send(3).await.unwrap();
    assert_eq!(meter.snapshot().peak, 1);
}
