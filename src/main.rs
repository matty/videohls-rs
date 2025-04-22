use serde_derive::Deserialize;
use notify::{RecommendedWatcher, RecursiveMode, Result as NotifyResult, Event, EventKind, recommended_watcher, Watcher};
use std::time::Duration;
use std::process::Command;
use log::{info, warn, error, debug};
use env_logger;
use rayon::prelude::*;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use reqwest;
use serde_json;

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Directory to watch for webm files
    pub watch_dir: Option<String>,
    /// Output directory for TS and m3u8 files
    pub output_dir: String,
    /// Prefix for TS file URLs in m3u8
    pub ts_url_prefix: Option<String>,
    /// Run in background and watch directory
    pub background: bool,
    /// Supported video file extensions (comma separated, no spaces)
    pub video_extensions: Option<String>,
    /// Discord webhook URL for notifications
    pub discord_webhook_url: Option<String>,
}

impl Config {
    pub fn from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}

fn get_video_extensions(cfg: &Config) -> Vec<String> {
    cfg.video_extensions
        .as_ref()
        .map(|s| s.split(',').map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()).collect())
        .unwrap_or_else(|| vec!["webm".to_string()])
}

fn process_webm_to_hls(
    input_path: &str,
    output_dir: &str,
    ts_url_prefix: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Ensure output directory exists
    std::fs::create_dir_all(output_dir)?;
    let file_stem = std::path::Path::new(input_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("Invalid input file name")?;
    let m3u8_path = format!("{}/{}.m3u8", output_dir, file_stem);
    let ts_pattern = format!("{}/{}_%03d.ts", output_dir, file_stem);
    let mut args = vec![
        "-i", input_path,
        "-c:v", "copy",
        "-c:a", "aac",
        "-f", "hls",
        "-hls_time", "5",
        "-hls_playlist_type", "vod",
        "-hls_segment_filename", &ts_pattern,
        &m3u8_path,
    ];
    if let Some(prefix) = ts_url_prefix {
        args.insert(args.len() - 1, "-hls_base_url");
        args.insert(args.len() - 1, prefix);
    }
    let output = Command::new("ffmpeg").args(&args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!("ffmpeg failed: {}", stderr);
        return Err(format!("ffmpeg failed: {}", stderr).into());
    }
    Ok(())
}

fn is_file_stable(path: &std::path::Path, checks: u32, delay_ms: u64) -> bool {
    use std::{thread, fs};
    let mut last_size = None;
    for _ in 0..checks {
        let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if let Some(last) = last_size {
            if last != size {
                last_size = Some(size);
                thread::sleep(Duration::from_millis(delay_ms));
                continue;
            }
        } else {
            last_size = Some(size);
            thread::sleep(Duration::from_millis(delay_ms));
            continue;
        }
        // If size hasn't changed, continue checking
        thread::sleep(Duration::from_millis(delay_ms));
    }
    // After checks, if size is stable, return true
    true
}

/// Send a message to a Discord webhook if the URL is set
fn send_discord_webhook(webhook_url: &str, content: &str) {
    let client = reqwest::blocking::Client::new();
    let payload = serde_json::json!({"content": content});
    let res = client.post(webhook_url)
        .json(&payload)
        .send();
    if let Err(e) = res {
        error!("Failed to send Discord webhook: {}", e);
    }
}

fn main() {
    env_logger::init();
    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || {
            running.store(false, Ordering::SeqCst);
            info!("Received Ctrl+C, shutting down gracefully...");
        }).expect("Error setting Ctrl+C handler");
    }
    // Try to load config from config.toml
    let mut config = Config::from_file("config.toml");
    // Check for TS_URL_PREFIX env var and override if present
    let ts_url_prefix_env = std::env::var("TS_URL_PREFIX").ok();
    // Check for DISCORD_WEBHOOK_URL env var and override if present
    let discord_webhook_url_env = std::env::var("DISCORD_WEBHOOK_URL").ok();
    if let Ok(ref mut cfg) = config {
        if let Some(env_prefix) = ts_url_prefix_env {
            cfg.ts_url_prefix = Some(env_prefix);
            info!("Overriding ts_url_prefix from environment variable");
        }
        if let Some(env_webhook) = discord_webhook_url_env {
            cfg.discord_webhook_url = Some(env_webhook);
            info!("Overriding discord_webhook_url from environment variable");
        }
    }
    match config {
        Ok(cfg) => {
            info!("Loaded config: {:?}", cfg);
            let video_exts = get_video_extensions(&cfg);
            if cfg.background {
                let watch_dir = cfg.watch_dir.clone().expect("watch_dir required in background mode");
                info!("Will watch directory: {}", watch_dir);

                // Process existing .webm files in the directory before starting the watcher
                let input_dir = std::path::Path::new(&watch_dir);
                if let Ok(entries) = std::fs::read_dir(input_dir) {
                    let files: Vec<_> = entries.flatten().collect();
                    files.par_iter().for_each(|entry| {
                        let path = entry.path();
                        if let Some(fname) = path.file_name().and_then(|s| s.to_str()) {
                            if fname.ends_with(".complete") || fname.ends_with(".failed") {
                                debug!("Skipping processed file on startup: {:?}", path);
                                return;
                            }
                        }
                        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                            if video_exts.iter().any(|v| v.eq_ignore_ascii_case(ext)) {
                                info!("Found existing video file on startup: {:?}", path);
                                // Wait for file to be stable
                                if !is_file_stable(&path, 3, 500) {
                                    warn!("File {:?} is not stable on startup, skipping", path);
                                    return;
                                }
                                let input = path.to_string_lossy();
                                info!("Processing file on startup: {}", input);
                                let file_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
                                let output_subdir = format!("{}/{}", cfg.output_dir, file_stem);
                                let ts_url_prefix = cfg.ts_url_prefix.as_ref().map(|prefix| {
                                    let mut prefix = prefix.trim_end_matches('/').to_string();
                                    prefix.push('/');
                                    prefix.push_str(file_stem);
                                    prefix.push('/');
                                    prefix
                                });
                                // Discord webhook: file processing started
                                if let Some(ref webhook_url) = cfg.discord_webhook_url {
                                    let msg = format!("Processing file: {}", input);
                                    send_discord_webhook(webhook_url, &msg);
                                }
                                let result = process_webm_to_hls(
                                    &input,
                                    &output_subdir,
                                    ts_url_prefix.as_deref(),
                                );
                                // Discord webhook: error or complete
                                if let Some(ref webhook_url) = cfg.discord_webhook_url {
                                    if let Err(ref e) = result {
                                        let msg = format!("Error processing file '{}': {}", input, e);
                                        send_discord_webhook(webhook_url, &msg);
                                    } else {
                                        let m3u8_name = format!("{}.m3u8", file_stem);
                                        let url = ts_url_prefix.as_deref().unwrap_or("").to_string() + &m3u8_name;
                                        let msg = format!("Processing complete: {}", url);
                                        send_discord_webhook(webhook_url, &msg);
                                    }
                                }
                                let mut processed_path = path.clone();
                                let new_ext = if result.is_ok() { ".complete" } else { ".failed" };
                                let complete_name = format!("{}{}", path.file_name().unwrap().to_string_lossy(), new_ext);
                                processed_path.set_file_name(complete_name);
                                if let Err(e) = std::fs::rename(&path, &processed_path) {
                                    error!("Failed to rename processed file on startup: {}", e);
                                } else {
                                    info!("Renamed processed file to: {:?}", processed_path);
                                }
                            }
                        }
                    });
                }

                // Use the new notify API
                let mut watcher: RecommendedWatcher = recommended_watcher(move |res: NotifyResult<Event>| {
                    match res {
                        Ok(event) => {
                            info!("Received event: {:?}", event);
                            if let EventKind::Create(_) | EventKind::Modify(_) = event.kind {
                                for path in event.paths {
                                    info!("Checking path: {:?}", path);
                                    if let Some(fname) = path.file_name().and_then(|s| s.to_str()) {
                                        if fname.ends_with(".complete") || fname.ends_with(".failed") {
                                            debug!("Skipping processed file: {:?}", path);
                                            continue;
                                        }
                                    }
                                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                                        if video_exts.iter().any(|v| v.eq_ignore_ascii_case(ext)) {
                                            info!("Detected video file: {:?}", path);
                                            if !is_file_stable(&path, 3, 500) {
                                                warn!("File {:?} is not stable, skipping", path);
                                                continue;
                                            }
                                            let input = path.to_string_lossy();
                                            info!("Processing file: {}", input);
                                            let file_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
                                            let output_subdir = format!("{}/{}", cfg.output_dir, file_stem);
                                            let ts_url_prefix = cfg.ts_url_prefix.as_ref().map(|prefix| {
                                                let mut prefix = prefix.trim_end_matches('/').to_string();
                                                prefix.push('/');
                                                prefix.push_str(file_stem);
                                                prefix.push('/');
                                                prefix
                                            });
                                            // Discord webhook: file processing started
                                            if let Some(ref webhook_url) = cfg.discord_webhook_url {
                                                let msg = format!("Processing file: {}", input);
                                                send_discord_webhook(webhook_url, &msg);
                                            }
                                            let result = process_webm_to_hls(
                                                &input,
                                                &output_subdir,
                                                ts_url_prefix.as_deref(),
                                            );
                                            // Discord webhook: error or complete
                                            if let Some(ref webhook_url) = cfg.discord_webhook_url {
                                                if let Err(ref e) = result {
                                                    let msg = format!("Error processing file '{}': {}", input, e);
                                                    send_discord_webhook(webhook_url, &msg);
                                                } else {
                                                    let m3u8_name = format!("{}.m3u8", file_stem);
                                                    let url = ts_url_prefix.as_deref().unwrap_or("").to_string() + &m3u8_name;
                                                    let msg = format!("Processing complete: {}", url);
                                                    send_discord_webhook(webhook_url, &msg);
                                                }
                                            }
                                            let mut processed_path = path.clone();
                                            let new_ext = if result.is_ok() { ".complete" } else { ".failed" };
                                            let complete_name = format!("{}{}", path.file_name().unwrap().to_string_lossy(), new_ext);
                                            processed_path.set_file_name(complete_name);
                                            if let Err(e) = std::fs::rename(&path, &processed_path) {
                                                error!("Failed to rename processed file: {}", e);
                                            } else {
                                                info!("Renamed processed file to: {:?}", processed_path);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => error!("watch error: {e}"),
                    }
                }).expect("Failed to create watcher");
                watcher.watch(watch_dir.as_ref(), RecursiveMode::NonRecursive).expect("Failed to watch directory");
                info!("Watching {} for new webm files...", watch_dir);
                // Keep the main thread alive
                while running.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_secs(1));
                }
                info!("Shutdown complete.");
            }
        }
        Err(e) => error!("Failed to load config: {}", e),
    }
    info!("App running");
}
