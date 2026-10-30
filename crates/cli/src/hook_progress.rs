//! Progress while startup hooks run before the normal turn display exists.
pub fn during<'a, F: std::future::Future + Send + 'a>(
    session: &'a str,
    interactive: bool,
    future: F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = F::Output> + Send + 'a>> {
    // Bound the outer CLI/worker future size on Windows debug builds. Hook
    // startup and compaction futures can otherwise overflow the main stack.
    let mut future = Box::pin(future);
    Box::pin(async move {
        let mut subscription = deepseek_code_core::services::hook_progress::subscribe(session);
        let mut progress = crate::terminal::progress::TurnProgress::new(false);
        let mut refresh = tokio::time::interval(std::time::Duration::from_secs(1));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                result = &mut future => return result,
                _ = refresh.tick() => progress.tick(),
                hooks = subscription.changed() => {
                    // Do not display a waiting spinner for sessions without statuses.
                    progress.clear();
                    let message = hooks.iter().map(|(_, text)| text.as_str()).collect::<Vec<_>>().join(" · ");
                    progress = crate::terminal::progress::TurnProgress::for_hook(interactive && !hooks.is_empty(), &message);
                }
            }
        }
    })
}
