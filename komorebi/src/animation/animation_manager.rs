use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use super::ANIMATION_CONDVAR;
use super::prefix::AnimationPrefix;

#[derive(Debug, Clone, Copy, PartialEq)]
enum AnimationPhase {
    /// Claimed but not yet rendering.
    Waiting,
    /// Rendering frames. A stuck runner in this phase may be force-released
    /// by a waiting newer animation once it exceeds the takeover budget.
    Running,
    /// Final positioning / uncloak / crossfade. Never force-released: the
    /// runner is unbounded only if one of its (now async) Win32 calls blocks,
    /// which would otherwise clobber a successor's cloak/ghost state.
    PostRender,
}

#[derive(Debug, Clone, Copy)]
struct AnimationState {
    /// Monotonic claim token for this key, bumped by every `request()`.
    /// Only the animation holding the latest token may run; a newer request
    /// invalidates every older token, waker or runner alike.
    request_seq: u64,
    /// The claim token that is currently being rendered (0 when idle).
    running_request: u64,
    in_progress: bool,
    /// Set when the runner claims the slot, used to detect wedged runners.
    started_at: Option<Instant>,
    phase: AnimationPhase,
}

#[derive(Debug)]
pub struct AnimationManager {
    animations: HashMap<String, AnimationState>,
}

impl Default for AnimationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl AnimationManager {
    pub fn new() -> Self {
        Self {
            animations: HashMap::new(),
        }
    }

    /// Register a new animation request for `animation_key` and return the
    /// caller's claim token. Must be called before any other coordination so a
    /// newer request always supersedes older ones, even ones already waiting.
    pub fn request(&mut self, animation_key: &str) -> u64 {
        let entry = self
            .animations
            .entry(animation_key.to_string())
            .or_insert(AnimationState {
                request_seq: 0,
                running_request: 0,
                in_progress: false,
                started_at: None,
                phase: AnimationPhase::Waiting,
            });

        entry.request_seq += 1;
        entry.request_seq
    }

    /// Returns `(is_latest, running)`: whether `seq` is still the newest claim
    /// for this key, and whether an animation is currently rendering it.
    pub fn observe(&self, animation_key: &str, seq: u64) -> (bool, bool) {
        match self.animations.get(animation_key) {
            Some(animation_state) => (
                animation_state.request_seq == seq,
                animation_state.in_progress,
            ),
            None => (true, false),
        }
    }

    /// Atomically claim the render slot for `seq`. Only the newest claim may
    /// start; returns false if another claim already holds the slot.
    pub fn try_start(&mut self, animation_key: &str, seq: u64) -> bool {
        match self.animations.get_mut(animation_key) {
            Some(animation_state)
                if animation_state.request_seq == seq && !animation_state.in_progress =>
            {
                animation_state.in_progress = true;
                animation_state.running_request = seq;
                animation_state.started_at = Some(Instant::now());
                animation_state.phase = AnimationPhase::Running;
                true
            }
            _ => false,
        }
    }

    /// Whether `seq` still holds the render slot, regardless of whether newer
    /// claims have since superseded it. Unlike [`AnimationManager::is_current`],
    /// this remains true for a naturally-superseded runner until it releases
    /// the slot itself, which is what guarantees a successor cannot start (and
    /// therefore cannot race its cleanup) before then.
    pub fn is_owner(&self, animation_key: &str, seq: u64) -> bool {
        match self.animations.get(animation_key) {
            Some(animation_state) => {
                animation_state.in_progress && animation_state.running_request == seq
            }
            None => false,
        }
    }

    /// Mark the runner for `seq` as entering its final post-render phase so
    /// [`AnimationManager::takeover_if_stale`] will not force-release it
    /// mid-teardown. Returns whether `seq` was still the owner.
    pub fn mark_post_render(&mut self, animation_key: &str, seq: u64) -> bool {
        match self.animations.get_mut(animation_key) {
            Some(animation_state)
                if animation_state.in_progress && animation_state.running_request == seq =>
            {
                animation_state.phase = AnimationPhase::PostRender;
                true
            }
            _ => false,
        }
    }

    /// If the runner holding the slot for `animation_key` has exceeded the
    /// takeover budget while still rendering, force-release it so a waiting
    /// newer animation can proceed without waiting forever on a wedged thread
    /// (e.g. one blocked inside a marshalled Win32 call). Never releases a
    /// runner that has moved into its post-render phase. The superseded runner
    /// is left to observe that it lost ownership and must not touch the window.
    pub fn takeover_if_stale(&mut self, animation_key: &str, budget: Duration) -> bool {
        let Some(state) = self.animations.get_mut(animation_key) else {
            return false;
        };

        if state.in_progress
            && state.phase == AnimationPhase::Running
            && state
                .started_at
                .is_some_and(|started_at| started_at.elapsed() > budget)
        {
            state.in_progress = false;
            state.running_request = 0;
            state.started_at = None;
            state.phase = AnimationPhase::Waiting;
            ANIMATION_CONDVAR.notify_all();
            true
        } else {
            false
        }
    }

    /// Whether `seq` is still the current running animation. Becomes false as
    /// soon as a newer request bumps `request_seq`, or after it completes.
    pub fn is_current(&self, animation_key: &str, seq: u64) -> bool {
        match self.animations.get(animation_key) {
            Some(animation_state) => {
                animation_state.in_progress
                    && animation_state.request_seq == seq
                    && animation_state.running_request == seq
            }
            None => false,
        }
    }

    /// Finished rendering (either completed, aborted, or a pre_render error).
    /// Releases the slot and wakes any waiting animation. The entry is removed
    /// once the newest claim completes so idle keys don't linger.
    pub fn complete(&mut self, animation_key: &str, seq: u64) {
        let remove = match self.animations.get_mut(animation_key) {
            Some(animation_state) if animation_state.running_request == seq => {
                animation_state.in_progress = false;
                animation_state.running_request = 0;
                animation_state.started_at = None;
                animation_state.phase = AnimationPhase::Waiting;
                animation_state.request_seq == seq
            }
            _ => false,
        };

        if remove {
            self.animations.remove(animation_key);
        }

        ANIMATION_CONDVAR.notify_all();
    }

    pub fn count_in_progress(&self, animation_key_prefix: AnimationPrefix) -> usize {
        self.animations
            .iter()
            .filter(|(key, state)| {
                state.in_progress && key.starts_with(animation_key_prefix.to_string().as_str())
            })
            .count()
    }

    pub fn count(&self) -> usize {
        self.animations.len()
    }
}
