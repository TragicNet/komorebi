use color_eyre::eyre;

use serde::Deserialize;
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use super::ANIMATION_CONDVAR;
use super::ANIMATION_DURATION_GLOBAL;
use super::ANIMATION_FPS;
use super::ANIMATION_MANAGER;
use super::RenderDispatcher;

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct AnimationEngine;

enum WaitOutcome {
    /// This animation claimed the render slot and may start.
    Started,
    /// The slot is busy; keep waiting.
    Pending,
    /// A newer animation superseded this one before it could start.
    Superseded,
}

impl AnimationEngine {
    pub fn wait_for_all_animations() {
        let max_duration = Duration::from_secs(20);
        let spent_duration = Instant::now();

        while ANIMATION_MANAGER.lock().count() > 0 {
            if spent_duration.elapsed() >= max_duration {
                break;
            }

            std::thread::sleep(Duration::from_millis(
                ANIMATION_DURATION_GLOBAL.load(Ordering::SeqCst),
            ));
        }
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn animate(
        render_dispatcher: impl RenderDispatcher + Send + 'static,
        duration: Duration,
    ) -> eyre::Result<()> {
        std::thread::spawn(move || {
            let animation_key = render_dispatcher.get_animation_key();

            // Claim the newest token for this animation key. Any running or
            // waiting animation for the same key is now superseded; only the
            // holder of the newest token may eventually render.
            let my_seq = {
                let mut manager = ANIMATION_MANAGER.lock();
                let seq = manager.request(animation_key.as_str());
                // Wake waiters so they detect that they were superseded.
                ANIMATION_CONDVAR.notify_all();
                seq
            };

            // Wait until we own the render slot: no animation is currently
            // rendering and we are still the newest claim. Concede (without
            // starting) if a newer animation superseded us while we waited.
            loop {
                let outcome = {
                    let mut manager = ANIMATION_MANAGER.lock();
                    let (is_latest, running) = manager.observe(animation_key.as_str(), my_seq);

                    if !is_latest {
                        WaitOutcome::Superseded
                    } else if running {
                        ANIMATION_CONDVAR.wait(&mut manager);
                        WaitOutcome::Pending
                    } else if manager.try_start(animation_key.as_str(), my_seq) {
                        WaitOutcome::Started
                    } else {
                        WaitOutcome::Pending
                    }
                };

                match outcome {
                    WaitOutcome::Superseded => return Ok(()),
                    WaitOutcome::Started => break,
                    WaitOutcome::Pending => {}
                }
            }

            if let Err(error) = render_dispatcher.pre_render() {
                ANIMATION_MANAGER
                    .lock()
                    .complete(animation_key.as_str(), my_seq);
                return Err(error);
            }

            let target_frame_time =
                Duration::from_millis(1000 / ANIMATION_FPS.load(Ordering::Relaxed));
            let mut progress = 0.0;
            let animation_start = Instant::now();

            // start animation
            while progress < 1.0 {
                // A newer animation superseded us; snap the renderer back to a
                // consistent visible state so the successor can take over
                // cleanly.
                if !ANIMATION_MANAGER
                    .lock()
                    .is_current(animation_key.as_str(), my_seq)
                {
                    render_dispatcher.cleanup_on_cancel();
                    ANIMATION_MANAGER
                        .lock()
                        .complete(animation_key.as_str(), my_seq);
                    return Ok(());
                }

                let frame_start = Instant::now();
                // calculate progress
                progress =
                    animation_start.elapsed().as_millis() as f64 / duration.as_millis() as f64;
                render_dispatcher.render(progress).ok();

                // sleep until next frame
                let frame_time_elapsed = frame_start.elapsed();

                if frame_time_elapsed < target_frame_time {
                    std::thread::sleep(target_frame_time - frame_time_elapsed);
                }
            }

            // Ensure the final frame sets the target position, in case the
            // elapsed time never produced a clean 1.0 progress step.
            if progress != 1.0 {
                progress = 1.0;

                // process animation for 1.0 to set target position
                render_dispatcher.render(progress).ok();
            }

            // Release the slot only after post-render/cleanup finishes so a
            // successor never overlaps cloak/uncloak or ghost teardown.
            let post_result = render_dispatcher.post_render();
            ANIMATION_MANAGER
                .lock()
                .complete(animation_key.as_str(), my_seq);
            post_result
        });

        Ok(())
    }
}
