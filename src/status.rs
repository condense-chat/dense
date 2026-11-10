use crate::api::auth;
use crate::config::Config;
use crate::{daemon, desktop, persist, ui};

pub fn run(cfg: &Config, details: bool) {
    let logged_in = auth::load_creds(cfg).is_authenticated();
    let paused = daemon::managed::switched_off(cfg);
    let desktop_configured = desktop::in_use(cfg);
    let desktop_ready = desktop_configured && desktop::routes_every_engine(cfg);
    let editor_updates = persist::watching(cfg);
    let service_running = daemon::running(cfg);
    let autostart = daemon::autostart::installed(cfg).is_some();

    ui::title("Status");
    if logged_in {
        ui::success("Login saved on this computer");
    } else {
        ui::note("You're not logged in yet");
    }
    if paused {
        ui::note("Your organization has paused Condense on this computer");
    }
    if cfg.profile() != "prod" {
        ui::field("Profile", cfg.profile());
    }

    ui::section("Your tools");
    let record = persist::load_record(cfg);
    for (name, label) in [
        ("claude", "Claude Code"),
        ("codex", "Codex"),
        ("opencode", "OpenCode"),
    ] {
        let configured = record.tools.contains_key(name) && persist::shim_path(cfg, name).is_file();
        let value = if paused {
            "Paused by your organization".into()
        } else if configured && !logged_in {
            "Automatic routing saved; log in to use it".into()
        } else if configured {
            "Automatic routing configured".into()
        } else if record.tools.contains_key(name) {
            format!("Setup missing: run `dense persist {name}`")
        } else {
            format!("On demand: `dense {name}`")
        };
        ui::field(label, value);
    }
    ui::field(
        "Claude Desktop",
        match (desktop_configured, desktop_ready, paused, logged_in) {
            (true, _, true, _) => "Paused by your organization",
            (true, true, _, false) => "Local Code setup saved; log in to use it",
            (true, true, _, true) => "Local Code sessions configured",
            (true, false, _, _) => "Setup needs attention",
            _ => "Not connected",
        },
    );
    if desktop_configured {
        ui::field(
            "Desktop scope",
            "Local Code and local-engine Cowork only; Chat, cloud and VM sessions are not connected",
        );
    }

    ui::section("Background service");
    ui::field(
        "Service",
        match (service_running, autostart) {
            (true, true) => "Running; starts when you log in",
            (true, false) => "Running; does not start automatically",
            (false, true) => "Stopped; automatic startup is configured",
            (false, false) => "Not installed",
        },
    );
    if editor_updates {
        ui::field(
            "Editor updates",
            if service_running {
                "Claude Code settings are repaired automatically"
            } else {
                "Waiting for the background service"
            },
        );
    }
    if !service_running && !desktop_configured && !editor_updates && !autostart {
        ui::field(
            "",
            "On-demand sessions work without the background service.",
        );
    }
    if desktop_configured && !service_running {
        ui::check(
            false,
            "Desktop updates need attention",
            "New Desktop versions will not connect automatically while the service is stopped.",
        );
        ui::hint("Repair Desktop setup", "dense claude-desktop install");
    } else if editor_updates && !service_running {
        ui::check(
            false,
            "Editor updates need attention",
            "Claude Code settings will not be repaired automatically while the service is stopped.",
        );
        ui::hint("Repair editor setup", "dense persist claude");
    } else if autostart && !service_running {
        ui::hint("Check background service", "dense doctor");
    }

    if details {
        ui::section("Technical details");
        ui::field("Server", &cfg.api_base_url);
        ui::field("Profile", cfg.profile());
        ui::field("Credentials", cfg.cred_dir().display());
        for (label, value) in daemon::status_lines(cfg) {
            ui::field(
                match label {
                    "daemon" => "Service",
                    "device" => "Device",
                    "switch" => "Organization",
                    "mode" => "Routing",
                    "desktop" => "Desktop",
                    "queue" => "Usage uploads",
                    label => label,
                },
                value,
            );
        }
    }
    if !logged_in {
        ui::hint("Log in", "dense login");
    } else {
        ui::hint("View savings", "dense info");
    }
    if desktop_configured {
        ui::hint("Desktop details", "dense claude-desktop status");
    }
    if !details {
        ui::hint("Technical details", "dense status --details");
    }
}
