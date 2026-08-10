use std::io;

use rvalt::App;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use winit::event_loop::EventLoop;

pub fn main() {
    dotenv::dotenv().ok();
    let layer = fmt::layer().with_writer(io::stdout);

    tracing_subscriber::registry()
        .with(layer)
        .with(EnvFilter::from_default_env())
        .init();

    info!("Starging Rvalt");

    let event_loop = EventLoop::new().expect("failed to create event loop");
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
    let mut app = App::new();
    event_loop.run_app(&mut app).expect("event loop error");
}
