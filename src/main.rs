//! Signal Messenger client for terminal

mod app;
mod config;
mod environment;
mod signal;
mod storage;
mod ui;
mod update;
mod util;

use app::{App, Event};
use update::update;

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event as CEvent, EventStream},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use environment::Environment;
use log::info;
use structopt::StructOpt;
use tokio_stream::StreamExt;
use tui::{backend::CrosstermBackend, Terminal};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TARGET_FPS: u64 = 144;
const FRAME_BUDGET: Duration = Duration::from_millis(1000 / TARGET_FPS);
const MESSAGE_SCROLL_BACK: bool = false;

#[derive(Debug, StructOpt)]
struct Args {
    /// Enables logging to `gurk.log` in the current working directory
    #[structopt(short, long)]
    verbose: bool,
    /// Relinks the device (helpful when device was unlinked)
    #[structopt(long)]
    relink: bool,
}

fn init_file_logger() -> anyhow::Result<()> {
    use log::LevelFilter;
    use log4rs::append::file::FileAppender;
    use log4rs::config::{Appender, Config, Root};
    use log4rs::encode::pattern::PatternEncoder;

    let logfile = FileAppender::builder()
        .encoder(Box::new(PatternEncoder::new("[{d} {l} {M}] {m}\n")))
        .build("gurk.log")?;

    let config = Config::builder()
        .appender(Appender::builder().build("logfile", Box::new(logfile)))
        .build(Root::builder().appender("logfile").build(LevelFilter::Info))?;

    log4rs::init_config(config)?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::from_args();
    if args.verbose {
        init_file_logger()?;
    }
    log_panics::init();

    tokio::task::LocalSet::new()
        .run_until(run_single_threaded(args.relink))
        .await
}

async fn is_online() -> bool {
    tokio::net::TcpStream::connect("detectportal.firefox.com:80")
        .await
        .is_ok()
}

async fn run_single_threaded(relink: bool) -> anyhow::Result<()> {
    let mut app = App::try_new(relink).await?;

    enable_raw_mode()?;
    let _raw_mode_guard = scopeguard::guard((), |_| {
        disable_raw_mode().unwrap();
    });

    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(100);
    tokio::spawn({
        let tx = tx.clone();
        async move {
            let mut reader = EventStream::new().fuse();
            while let Some(event) = reader.next().await {
                match event {
                    Ok(CEvent::Key(key)) => tx.send(Event::Input(key)).await.unwrap(),
                    Ok(CEvent::Resize(cols, rows)) => {
                        tx.send(Event::Resize { cols, rows }).await.unwrap()
                    }
                    Ok(CEvent::Mouse(button)) => tx.send(Event::Click(button)).await.unwrap(),
                    _ => (),
                }
            }
        }
    });

    let backend = CrosstermBackend::new(stdout);

    let mut terminal = Terminal::new(backend)?;

    let inner_manager = app.signal_manager.clone();
    let inner_tx = tx.clone();
    tokio::task::spawn_local(async move {
        loop {
            let messages = if !is_online().await {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                continue;
            } else {
                match inner_manager.receive_messages().await {
                    Ok(messages) => {
                        info!("connected and listening for incoming messages");
                        messages
                    }
                    Err(e) => {
                        let e = anyhow::Error::from(e).context(
                            "failed to initialize the stream of Signal messages.\n\
                            Maybe the device was unlinked? Please try to restart with '--relink` flag.",
                        );
                        inner_tx
                            .send(Event::Quit(Some(e)))
                            .await
                            .expect("logic error: events channel closed");
                        return;
                    }
                }
            };

            tokio::pin!(messages);
            while let Some(message) = messages.next().await {
                inner_tx
                    .send(Event::Message(message))
                    .await
                    .expect("logic error: events channel closed")
            }
            info!("messages channel disconnected. trying to reconnect.")
        }
    });

    terminal.clear()?;

    let mut last_render_at = Instant::now();
    let is_render_spawned = Arc::new(AtomicBool::new(false));

    let mut env = Environment::with_terminal(terminal);

    loop {
        // render
        let left_frame_budget = FRAME_BUDGET.checked_sub(last_render_at.elapsed());
        if let Some(budget) = left_frame_budget {
            // skip frames that render too fast
            if !is_render_spawned.load(Ordering::Relaxed) {
                let tx = tx.clone();
                let is_render_spawned = is_render_spawned.clone();
                is_render_spawned.store(true, Ordering::Relaxed);
                tokio::spawn(async move {
                    // Redraw message is needed to make sure that we render the skipped frame
                    // if it was the last frame in the rendering budget window.
                    tokio::time::sleep(budget).await;
                    tx.send(Event::Redraw)
                        .await
                        .expect("logic error: events channel closed");
                    is_render_spawned.store(false, Ordering::Relaxed);
                });
            }
        } else {
            env.terminal.draw(|f| ui::draw(f, &mut app))?;
            last_render_at = Instant::now();
        }

        match rx.recv().await {
            Some(event) => {
                if let Some(next_app) = update(app, event, &mut env).await? {
                    app = next_app;
                } else {
                    break;
                }
            }
            None => break, // channel closed => quit
        }
    }

    execute!(
        env.terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .unwrap();
    env.terminal.show_cursor().unwrap();

    Ok(())
}
