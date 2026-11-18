//! conchy-lock: ext-session-lock screen locker for Conchy OS.

mod app;
mod auth;
mod background;
mod scene;
mod theme;

use clap::Parser;

#[derive(Parser)]
#[command(version, about = "Conchy OS screen locker")]
struct Cli {
    /// Safe preview: render in a regular window instead of locking the
    /// session. Accepts the password "demo".
    #[arg(long)]
    demo: bool,

    /// Wallpaper to blur (overrides ~/.config/conchy/lock.kdl).
    #[arg(long, value_name = "PATH")]
    wallpaper: Option<std::path::PathBuf>,

    /// Render a preview to a PNG file and exit (no Wayland needed).
    #[arg(long, value_name = "FILE")]
    png: Option<std::path::PathBuf>,

    /// With --png: idle | typing | checking | failed | caps
    #[arg(long, default_value = "idle")]
    state: String,

    /// With --png: logical size, e.g. 1440x900
    #[arg(long, default_value = "1440x900")]
    size: String,

    /// With --png: integer scale
    #[arg(long, default_value_t = 2)]
    scale: u32,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    if let Some(out) = &cli.png {
        let (w, h) = cli.size.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))).unwrap_or((1440, 900));
        if let Err(e) = app::render_png(out, w, h, cli.scale, cli.wallpaper.clone(), &cli.state) {
            eprintln!("conchy-lock: {e}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = app::run(app::Options { demo: cli.demo, wallpaper: cli.wallpaper }) {
        eprintln!("conchy-lock: {e}");
        std::process::exit(1);
    }
}
