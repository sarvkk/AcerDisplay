mod ddc;
mod layout;
mod mutter;
mod profiles;
mod ui;

use eframe::egui;
use std::time::{Duration, Instant};

fn main() -> eframe::Result {
    env_logger::init();
    if std::env::args().any(|a| a == "--probe") {
        return probe();
    }

    // Optional: `--tab image|layout|profiles` to open on a specific tab.
    let tab = std::env::args()
        .position(|a| a == "--tab")
        .and_then(|i| std::env::args().nth(i + 1))
        .map(|t| match t.as_str() {
            "layout" => ui::Tab::Layout,
            "profiles" => ui::Tab::Profiles,
            _ => ui::Tab::Image,
        });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 740.0])
            .with_min_inner_size([780.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Display Widget",
        options,
        Box::new(move |_cc| Ok(Box::new(ui::App::with_tab(tab)))),
    )
}

/// Headless diagnostics: `display-widget --probe`
fn probe() -> eframe::Result {
    println!("== i2c access ==");
    match ddc::i2c_permission_error() {
        Some(e) => println!("PERMISSION PROBLEM: {e}"),
        None => println!("ok"),
    }

    println!("\n== DDC monitors ==");
    let list = ddc::enumerate();
    if list.is_empty() {
        println!("(none found)");
    }
    for (id, worker) in &list {
        println!(
            "- {} | mfr={} model=\"{}\" serial=\"{}\" backend_id={}",
            id.key, id.manufacturer, id.model, id.serial, id.backend_id
        );
        // EDID alone does not prove the panel speaks DDC/CI (eDP laptop
        // panels answer EDID reads but reject VCP commands), so round-trip
        // one VCP read here.
        worker.get(ddc::BRIGHTNESS);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut result: Option<Result<(u16, u16), String>> = None;
        while result.is_none() && Instant::now() < deadline {
            if let Some(ev) = worker.try_recv() {
                match ev {
                    ddc::Ev::Value {
                        code, value, max, ..
                    } if code == ddc::BRIGHTNESS => result = Some(Ok((value, max))),
                    ddc::Ev::Failed { code, err, .. } if code == ddc::BRIGHTNESS => {
                        result = Some(Err(err));
                    }
                    _ => {}
                }
            }
            if result.is_none() {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        match result {
            Some(Ok((value, max))) => println!("    brightness: {value} (max {max})"),
            Some(Err(e)) => println!("    brightness: ERROR: {e}"),
            None => println!("    brightness: no reply"),
        }
    }

    println!("\n== GNOME (mutter) state ==");
    match mutter::get_state() {
        Ok(st) => {
            println!("serial={} layout_mode={}", st.serial, st.layout_mode);
            for m in &st.monitors {
                println!(
                    "- {} (\"{}\") vendor={} product=\"{}\" serial=\"{}\" modes={}",
                    m.connector,
                    m.display_name,
                    m.vendor,
                    m.product,
                    m.serial,
                    m.modes.len()
                );
                for mode in m.modes.iter().filter(|mm| mm.is_current) {
                    println!(
                        "    current: {}x{} @ {:.3} Hz scales={:?}",
                        mode.w, mode.h, mode.refresh, mode.scales
                    );
                }
            }
            for l in &st.logicals {
                println!(
                    "logical: pos=({},{}) scale={} transform={} primary={} monitors={}",
                    l.x,
                    l.y,
                    l.scale,
                    l.transform,
                    l.primary,
                    l.monitors
                        .iter()
                        .map(|m| m[0].clone())
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }
        }
        Err(e) => println!("ERROR: {e}"),
    }
    Ok(())
}
