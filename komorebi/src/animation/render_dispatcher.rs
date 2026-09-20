use color_eyre::eyre;

pub trait RenderDispatcher {
    fn get_animation_key(&self) -> String;
    fn pre_render(&self) -> eyre::Result<()>;
    fn render(&self, delta: f64) -> eyre::Result<()>;

    /// Called by the animation engine after the final frame when the runner
    /// has reached its target. `is_current` is true if this animation still
    /// owns the render slot; if false, a newer animation has taken over and
    /// the window may already be owned by it, so implementors must not touch
    /// the underlying window at all (only release their own resources).
    fn post_render(&self, is_current: bool) -> eyre::Result<()>;

    /// Called by the animation engine when an in-flight animation is cancelled
    /// before it could complete and it still owns the render slot (no newer
    /// animation can have started). Implementors should use this to release any
    /// resources allocated in `pre_render` and bring the underlying window
    /// back to a consistent visible state. Default: no-op.
    fn cleanup_on_cancel(&self) {}

    /// Called when an in-flight animation had its render slot taken away by a
    /// newer animation (force-release) and therefore must not perform any
    /// window surgery - the successor owns the cloak/uncloak and positioning.
    /// At most, discard resources owned solely by this animation. Default:
    /// no-op.
    fn on_superseded(&self) {}
}
