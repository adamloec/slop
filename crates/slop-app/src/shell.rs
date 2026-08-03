//! The event loop every windowed application was writing out by hand.
//!
//! `docs/CONVENTIONS.md` §2.3's rule is that the third copy is the trigger to
//! extract. That rule fired on the frame loop and produced
//! `slop_render::FrameRenderer` and [`Gpu`](crate::gpu::Gpu) — and then stopped
//! halfway. This crate owned `gpu`, `window`, `timing` and `logging`;
//! everything *except* the loop, which is the part four examples were still
//! copying. `CONSIDERATIONS.md` item 4 is that finding.
//!
//! What was duplicated, verbatim, four times:
//!
//! - an `App`/`Renderer` struct pair, the first holding `Option<Renderer>` and
//!   `Option<String>`
//! - `resumed` guarding against firing twice, which it does on some platforms
//! - `window_event` matching close, resize and redraw
//! - `about_to_wait` requesting the next redraw
//! - `SLOP_FRAMES` parsed out of the environment by hand
//! - a `main` that inits logging, runs the loop, logs the failure, drops
//!   explicitly and sets an exit code
//!
//! # This is not a framework
//!
//! `docs/DESIGN.md` §1.2 principle 4 says the game owns `main()`. It still
//! does: [`run`] is a function a `main` calls, not a harness it lives inside,
//! and an application that wants the loop for itself can ignore this module and
//! implement `winit`'s `ApplicationHandler` directly — which is all this does.
//!
//! What is deliberately *not* abstracted is anything above the loop. There is no
//! notion of a scene, an asset, a camera or an update step here. The trait's
//! whole surface is "build me", "here is your window", "draw", and the engine
//! learns nothing about what is being drawn.

use std::fmt::Display;

use slop_core::diagnostics::tracing::{error, info};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

/// The environment variable that ends the loop after a fixed number of frames.
///
/// Makes shutdown verifiable without a human closing a window, and is the shape
/// `docs/DESIGN.md` §5's deterministic headless mode needs — run a fixed number
/// of frames, then stop.
pub const FRAME_LIMIT: &str = "SLOP_FRAMES";

/// A windowed application: something that builds, draws, and knows its window.
///
/// Implement this on the type that owns the GPU objects — the thing every
/// example calls `Renderer` — and pass it to [`run`].
pub trait Application: Sized {
    /// What a failure to build or draw is reported as.
    ///
    /// An associated type rather than a fixed error, because the layer that
    /// owns `main` is the layer allowed to decide how it reports — and the
    /// examples currently use `String`, which `CONSIDERATIONS.md` item 7 has
    /// opinions about but which is not this trait's business to forbid.
    type Error: Display;

    /// Build everything, once the event loop can hand out windows.
    ///
    /// Called from `resumed`. It may fire more than once on some platforms;
    /// [`run`] guards that, so this is called exactly once.
    ///
    /// # Errors
    ///
    /// Whatever bring-up failed. [`run`] logs it and exits non-zero.
    fn new(event_loop: &ActiveEventLoop) -> Result<Self, Self::Error>;

    /// The window being drawn into.
    fn window(&self) -> &Window;

    /// Draw one frame.
    ///
    /// # Errors
    ///
    /// Whatever the frame failed on. [`run`] logs it and exits non-zero.
    fn render(&mut self) -> Result<(), Self::Error>;

    /// How many frames have been drawn, for [`FRAME_LIMIT`].
    fn frame_number(&self) -> u64;

    /// The window changed size.
    ///
    /// The swapchain is stale from here; what to do about it belongs to
    /// whatever owns it.
    fn resized(&mut self) {}

    /// Every window event, before the default handling.
    ///
    /// This is where a debug UI gets first refusal. Returning `true` means the
    /// event was consumed and the default handling is skipped — though close
    /// and resize are always handled, because an interface that can swallow a
    /// close request is an interface that traps the window open.
    fn on_window_event(&mut self, _event: &WindowEvent) -> bool {
        false
    }

    /// Whether to ask for another frame as soon as the last one is done.
    ///
    /// True by default, which drives the loop continuously rather than only on
    /// damage — the way a game's would run, and what makes the frame loop
    /// exercised rather than merely present. An application that draws nothing
    /// returns `false` and gets a window that idles.
    fn redraws_continuously(&self) -> bool {
        true
    }
}

/// Initialise logging, run `A` until it exits, and report how it went.
///
/// Never returns: the process exits 1 if the application reported a failure and
/// 0 otherwise, after the application has been dropped so that shutdown
/// finishes before the process does.
///
/// # Panics
///
/// Panics if the event loop cannot be created or fails while running, both of
/// which mean the platform is not in a state any of this can proceed from.
pub fn run<A: Application>() -> ! {
    crate::logging::init();

    let event_loop = EventLoop::new().expect("an event loop must be creatable");
    let mut shell = Shell::<A> {
        application: None,
        failure: None,
        frame_limit: std::env::var(FRAME_LIMIT)
            .ok()
            .and_then(|value| value.parse().ok()),
    };

    event_loop.run_app(&mut shell).expect("the event loop failed");

    if let Some(failure) = &shell.failure {
        error!(error = %failure, "the application failed");
    }

    // Dropped explicitly so shutdown finishes — and logs that it finished —
    // before the process exits. Letting it fall out of scope after the exit
    // check would work, but "shutdown complete" is only trustworthy if it is
    // printed after the teardown it describes.
    let failed = shell.failure.is_some();
    drop(shell);

    info!("shutdown complete");

    std::process::exit(i32::from(failed));
}

/// The `ApplicationHandler` every example was writing.
struct Shell<A: Application> {
    application: Option<A>,
    failure: Option<String>,
    frame_limit: Option<u64>,
}

impl<A: Application> ApplicationHandler for Shell<A> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // `resumed` fires more than once on some platforms.
        if self.application.is_some() {
            return;
        }

        match A::new(event_loop) {
            Ok(application) => self.application = Some(application),
            Err(error) => self.fail(event_loop, &error),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(application) = self.application.as_mut() else {
            return;
        };

        let consumed = application.on_window_event(&event);

        match event {
            // Handled whether or not the application consumed the event: an
            // interface that can swallow a close request traps the window open.
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => application.resized(),
            WindowEvent::RedrawRequested if !consumed => {
                if let Err(error) = application.render() {
                    self.fail(event_loop, &error);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(application) = self.application.as_ref() else {
            return;
        };

        if let Some(limit) = self.frame_limit
            && application.frame_number() >= limit
        {
            println!("rendered {limit} frames; exiting");
            // Cleared so this fires once: `about_to_wait` runs again before the
            // loop actually unwinds.
            self.frame_limit = None;
            event_loop.exit();
            return;
        }

        if application.redraws_continuously() {
            application.window().request_redraw();
        }
    }
}

impl<A: Application> Shell<A> {
    /// Record a failure and start unwinding the loop.
    ///
    /// Stringified here rather than stored typed, because this is the boundary
    /// where the failure stops being something to handle and becomes something
    /// to report.
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: &A::Error) {
        self.failure = Some(error.to_string());
        event_loop.exit();
    }
}
