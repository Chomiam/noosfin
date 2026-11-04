//! Noosfin — tableau de bord HTPC (session KWin HDR).

mod aio;
mod anilist;
mod app;
mod config;
mod disc;
mod gamepad;
mod images;
mod iptv;
mod jellyfin;
mod player;
mod qr;
mod scraper;
mod segments;
mod session;
mod state;
mod tmdb;
mod update;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    let windowed = args.iter().any(|a| a == "--windowed");
    let start_media = args.iter().any(|a| a == "--media");

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build()?;

    // Mode console : `--search movie tt0816692 "Interstellar" 2014 [--resolve N]`,
    // `--search episode tt0944947 "Game of Thrones" 1 2`, `--search anime "Sousou no Frieren" 5`.
    if args.iter().any(|a| a == "--sources-web") {
        return rt.block_on(async {
            let w = scraper::web::start(std::sync::Arc::new(|c: scraper::config::Sources| println!("enregistré : {}", c.summary()))).await?;
            println!("{}", w.url);
            tokio::signal::ctrl_c().await?;
            Ok(())
        });
    }
    // `--test-trackers [id…]` : teste chaque tracker intégré.
    if let Some(i) = args.iter().position(|a| a == "--test-trackers") {
        let ids: Vec<String> = args[i + 1..].to_vec();
        return rt.block_on(async move {
            let engine = scraper::Engine::new(scraper::config::Sources::load());
            let mut all: Vec<String> = ["knaben", "torrentscsv", "subsplease", "yts", "tpb"].iter().map(|s| s.to_string()).collect();
            all.extend(scraper::cardigann::all().iter().map(|d| d.id.clone()));
            let mut set = tokio::task::JoinSet::new();
            for id in all.into_iter().filter(|id| ids.is_empty() || ids.contains(id)) {
                let e = engine.clone();
                set.spawn(async move {
                    let t = std::time::Instant::now();
                    let r = tokio::time::timeout(std::time::Duration::from_secs(90), e.test_tracker(&id)).await;
                    let msg = match r {
                        Ok(Ok(m)) => format!("OK   {m}"),
                        Ok(Err(e)) => format!("ERR  {e:#}"),
                        Err(_) => "ERR  délai dépassé".into(),
                    };
                    println!("{id:<22} {:>5.1}s  {msg}", t.elapsed().as_secs_f32());
                });
            }
            while set.join_next().await.is_some() {}
            Ok(())
        });
    }

    if let Some(i) = args.iter().position(|a| a == "--search") {
        return rt.block_on(search_cli(&args[i + 1..]));
    }

    // Noosfin s'appuie sur Bazzite (KWin, SDDM, steamos-manager, Flatpak mpv).
    if session::Edition::detect().is_none() {
        eprintln!("Noosfin ne fonctionne que sur Bazzite (https://bazzite.gg).");
        std::process::exit(1);
    }

    let ui = AppWindow::new()?;
    if !windowed {
        ui.window().set_fullscreen(true);
        // Session TV : pilotée à la manette ou au clavier, le curseur de la souris est masqué.
        let weak = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
            use slint::winit_030::WinitWindowAccessor;
            if let Some(ui) = weak.upgrade() {
                ui.window().with_winit_window(|w| w.set_cursor_visible(false));
            }
        });
    }
    app::install(&ui, rt.handle().clone(), start_media);
    if let Some(i) = args.iter().position(|a| a == "--script") {
        run_script(&ui, args.get(i + 1).cloned().unwrap_or_default());
    }
    ui.run()?;
    app::shutdown();
    rt.shutdown_timeout(std::time::Duration::from_millis(300));
    Ok(())
}

/// Mode test : `--script "wait:3;key:right;snap:/tmp/a.png;quit"`.
fn run_script(ui: &AppWindow, script: String) {
    let steps: std::collections::VecDeque<String> = script.split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    next_step(ui.as_weak(), steps);
}

fn next_step(weak: slint::Weak<AppWindow>, mut steps: std::collections::VecDeque<String>) {
    let Some(step) = steps.pop_front() else { return };
    let (cmd, arg) = step.split_once(':').unwrap_or((step.as_str(), ""));
    let mut delay = std::time::Duration::from_millis(250);
    match cmd {
        "wait" => delay = std::time::Duration::from_secs_f64(arg.parse().unwrap_or(1.0)),
        "key" => app::script_key(arg),
        "snap" => {
            if let Some(ui) = weak.upgrade() {
                match ui.window().take_snapshot() {
                    Ok(buf) => {
                        let img = image::RgbaImage::from_raw(buf.width(), buf.height(), buf.as_bytes().to_vec());
                        if let Some(img) = img {
                            let _ = img.save(arg);
                        }
                    }
                    Err(e) => log::error!("snapshot : {e}"),
                }
            }
        }
        "quit" => {
            let _ = slint::quit_event_loop();
            return;
        }
        _ => log::warn!("étape inconnue : {step}"),
    }
    slint::Timer::single_shot(delay, move || next_step(weak, steps));
}

async fn search_cli(a: &[String]) -> anyhow::Result<()> {
    use scraper::{Engine, Query, config::Sources};
    let arg = |i: usize| a.get(i).cloned().unwrap_or_default();
    let q = match arg(0).as_str() {
        "movie" => Query { imdb: Some(arg(1)), title: arg(2), year: arg(3).parse().ok(), ..Default::default() },
        "episode" => Query { imdb: Some(arg(1)), title: arg(2), season: arg(3).parse().ok(), episode: arg(4).parse().ok(), ..Default::default() },
        _ => Query { title: arg(1), season: scraper::parse::anime_season(&arg(1)), episode: arg(2).parse().ok(), anime: true, ..Default::default() },
    };
    let engine = Engine::new(Sources::load());
    let t = std::time::Instant::now();
    let r = engine.search(&q).await;
    println!("{} résultats en {:.1}s (débrideur : {})", r.candidates.len(), t.elapsed().as_secs_f64(), engine.debrid_name());
    for e in &r.errors {
        println!("  ! {e}");
    }
    for (i, c) in r.candidates.iter().take(25).enumerate() {
        println!(
            "{i:>2} {} {:>5} {:<9} {:<18} {:>6.1} Go  {:<22} {}",
            match c.cached { Some(true) => "⚡", Some(false) => "·", None => "?" },
            c.info.resolution.unwrap_or("?"),
            c.info.languages.join(","),
            c.info.hdr.join(","),
            c.size as f64 / 1e9,
            c.source.chars().take(22).collect::<String>(),
            c.title.chars().take(90).collect::<String>()
        );
    }
    if let Some(n) = a.iter().position(|x| x == "--resolve").and_then(|i| a.get(i + 1)).and_then(|n| n.parse::<usize>().ok()) {
        if let Some(c) = r.candidates.get(n) {
            match engine.resolve(c, &q).await {
                Ok(res) => println!("→ {} ({})", res.url.split('/').take(3).collect::<Vec<_>>().join("/"), res.filename),
                Err(e) => println!("→ échec : {e}"),
            }
        }
    }
    Ok(())
}
